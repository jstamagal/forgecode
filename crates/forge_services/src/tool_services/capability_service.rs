//! The capability service (T11–T19, T25): owns the long-lived terminal,
//! kernel and language-server state and dispatches every phase 7 tool to its
//! engine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use forge_app::{CapabilityService, CellToolBridge, EnvironmentInfra};
use forge_domain::{
    Image, Kernel, SnapshotCoverage, SnapshotRepository, ToolCallContext, ToolCatalog, ToolOutput,
};
use serde_json::json;

use super::capabilities as engines;
use super::kernel::{Kernels, ToolBridge, ToolNames, image_dir};
use super::lsp::LanguageServers;
use super::provenance::{Staleness, changed_since_read, hash_bytes, key, require_read};
use super::terminal::{StartSpec, Terminals};
use super::toolbox::{self, ToolboxDirs};

/// The capability service.
pub struct ForgeCapabilities<F> {
    infra: Arc<F>,
    terminals: Terminals,
    kernels: Kernels,
    servers: LanguageServers,
    endpoints: engines::Endpoints,
}

impl<F> ForgeCapabilities<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self {
            infra,
            terminals: Terminals::default(),
            kernels: Kernels::default(),
            servers: LanguageServers::default(),
            endpoints: engines::Endpoints::default(),
        }
    }
}

fn key_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl<F: EnvironmentInfra<Config = forge_config::ForgeConfig> + SnapshotRepository + Send + Sync>
    ForgeCapabilities<F>
{
    fn shell_env(&self) -> std::collections::BTreeMap<String, String> {
        let config = self.infra.get_config().ok();
        let allow: Vec<String> = config.and_then(|c| c.shell_env_allow).unwrap_or_else(|| {
            [
                "PATH",
                "HOME",
                "USER",
                "LANG",
                "LC_ALL",
                "TERM",
                "PWD",
                "SHELL",
                "RUSTUP_HOME",
                "CARGO_HOME",
            ]
            .map(str::to_string)
            .to_vec()
        });
        let vars = self.infra.get_env_vars();
        allow
            .into_iter()
            .filter_map(|name| vars.get(&name).map(|v| (name, v.clone())))
            .collect()
    }

    fn base(&self) -> PathBuf {
        self.infra.get_environment().base_path
    }

    async fn terminal(
        &self,
        input: forge_domain::TerminalSession,
        cwd: &Path,
        owner: &str,
    ) -> anyhow::Result<ToolOutput> {
        let name = input
            .session
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "main".to_string());
        let wait = Duration::from_millis(input.wait_ms.unwrap_or(700).min(10_000));
        let action = input.action.trim().to_lowercase();
        if action != "start" && (input.cols.is_some() || input.rows.is_some()) {
            bail!("cols and rows apply only to action=start");
        }
        let value = match action.as_str() {
            "start" => {
                let command = input
                    .command
                    .clone()
                    .filter(|c| !c.trim().is_empty())
                    .context("action=start requires command")?;
                let spec = StartSpec {
                    command,
                    cwd: input
                        .cwd
                        .map(PathBuf::from)
                        .unwrap_or_else(|| cwd.to_path_buf()),
                    cols: input.cols.unwrap_or(120).clamp(1, 400),
                    rows: input.rows.unwrap_or(40).clamp(1, 200),
                    shell: self.infra.get_environment().shell,
                    env: self.shell_env(),
                    jail: forge_domain::active_jail().cloned(),
                };
                let terminals = &self.terminals;
                tokio::task::block_in_place(|| terminals.start(owner, &name, spec, wait))?
            }
            "send" => {
                let keys = input.keys.clone().unwrap_or_default();
                let terminals = &self.terminals;
                tokio::task::block_in_place(|| terminals.send(owner, &name, &keys, wait))?
            }
            "look" => {
                let terminals = &self.terminals;
                tokio::task::block_in_place(|| terminals.look(owner, &name, wait))?
            }
            "stop" => self.terminals.stop(owner, &name)?,
            "list" => self.terminals.list(owner),
            other => bail!("action must be start, send, look, stop or list; got {other:?}"),
        };
        Ok(ToolOutput::text(serde_json::to_string_pretty(&value)?))
    }

    async fn notebook(
        &self,
        input: forge_domain::NotebookEdit,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        let path = PathBuf::from(&input.file_path);
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            bail!("notebook_edit works on .ipynb files");
        }
        let current =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let ledger = context.reads()?;
        let known = ledger.with(|l| l.known_content(&key(&path)).map(<[u8]>::to_vec));
        if let Staleness::Changed(message) = changed_since_read(
            &ledger,
            &path,
            known.as_deref(),
            &current,
            "edit it",
            false,
            0,
        ) {
            bail!("{message}");
        }
        require_read(&ledger, &path, "edit it", &hash_bytes(&current), &[], None)?;
        let mode = input.edit_mode.as_deref().unwrap_or("replace");
        let (updated, summary) = engines::notebook_edit(
            &current,
            input.cell_id.as_deref(),
            input.new_source.as_deref(),
            input.cell_type.as_deref(),
            mode,
        )?;
        let coverage = ledger.with(|l| {
            l.coverage(&key(&path)).map(|c| SnapshotCoverage {
                hash: c.hash.clone(),
                ranges: c.ranges.clone(),
                whole: c.whole,
            })
        });
        let notices = self
            .infra
            .insert_snapshot(context.conversation_id(), &path, coverage)
            .await?;
        std::fs::write(&path, &updated)?;
        // The model saw the whole edit; the notebook stays read.
        let hash = hash_bytes(&updated);
        let lines =
            super::provenance::line_count(&bstr::ByteSlice::to_str_lossy(updated.as_slice()));
        ledger.with(|l| {
            l.replace(&key(&path), &hash, vec![], lines, true);
            l.remember_content(&key(&path), &hash, updated.clone());
        });
        let mut text = format!("{summary} in {}", path.display());
        for notice in notices {
            text.push('\n');
            text.push_str(&notice);
        }
        Ok(ToolOutput::text(text))
    }
}

#[async_trait::async_trait]
impl<
    F: EnvironmentInfra<Config = forge_config::ForgeConfig>
        + SnapshotRepository
        + Send
        + Sync
        + 'static,
> CapabilityService for ForgeCapabilities<F>
{
    async fn run_capability(
        &self,
        input: ToolCatalog,
        cwd: PathBuf,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        let owner = context.conversation_id().to_string();
        let path_env = self.infra.get_env_var("PATH").unwrap_or_default();
        match input {
            ToolCatalog::TerminalSession(input) => self.terminal(input, &cwd, &owner).await,
            ToolCatalog::TerminalSnapshot(input) => {
                let name = input.session.unwrap_or_else(|| "main".to_string());
                let wait = Duration::from_millis(input.wait_ms.unwrap_or(100).min(10_000));
                let dir = self.base().join("tool-results").join("terminal");
                let terminals = &self.terminals;
                let (path, png) =
                    tokio::task::block_in_place(|| terminals.snapshot(&owner, &name, wait, &dir))?;
                let mut output =
                    ToolOutput::text(format!("terminal snapshot written to {}", path.display()));
                if context.vision() {
                    output = output.combine(ToolOutput::image(Image::new_bytes(png, "image/png")));
                }
                Ok(output)
            }
            ToolCatalog::Lsp(input) => {
                let text = self
                    .servers
                    .query(
                        &input.operation,
                        Path::new(&input.file_path),
                        input.line,
                        input.character,
                        &cwd,
                        &path_env,
                    )
                    .await?;
                Ok(ToolOutput::text(text))
            }
            ToolCatalog::Toolbox(input) => {
                let dirs = ToolboxDirs {
                    global: self.base().join("toolbox"),
                    project: cwd.join(".forge").join("toolbox"),
                };
                let (text, code) = toolbox::run(&dirs, &input, &owner).await?;
                let Some(code) = code else {
                    return Ok(ToolOutput::text(text));
                };
                // load: exec the functions into this conversation's kernel.
                let Some(kernel) = self.kernels.get(&owner).await else {
                    bail!("no kernel is running; run a kernel cell first, then load");
                };
                let id = kernel.submit(&code).await?;
                let report = kernel.wait(id, Duration::from_secs(60)).await;
                Ok(ToolOutput::text(format!("{text}\n{report}")))
            }
            ToolCatalog::Browse(input) => {
                let resolved = forge_domain::resolve_binary("obscura", &self.base(), &path_env)?;
                let mut text = engines::browse(
                    resolved.path(),
                    &input.url,
                    input.dump.as_deref().unwrap_or("markdown"),
                    input.screenshot.as_deref().map(Path::new),
                    &cwd,
                )
                .await?;
                if let forge_domain::Resolved::Path { warning, .. } = resolved {
                    text.push_str(&format!("\n[{warning}]"));
                }
                Ok(ToolOutput::text(text))
            }
            ToolCatalog::BrowserProbe(input) => {
                let dir = input
                    .output_dir
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        self.base().join("tool-results").join(format!(
                            "probe-{}",
                            chrono::Utc::now().format("%Y%m%d-%H%M%S%3f")
                        ))
                    });
                let spec = json!({
                    "target": input.target,
                    "click": input.click,
                    "press": input.press,
                    "settle_ms": input.settle_ms,
                    "hold_ms": input.hold_ms,
                    "viewport_width": input.viewport_width,
                    "viewport_height": input.viewport_height,
                });
                Ok(ToolOutput::text(
                    engines::browser_probe(spec, &dir, &cwd).await?,
                ))
            }
            ToolCatalog::SerperSearch(input) => Ok(ToolOutput::text(
                engines::serper(
                    &self.endpoints,
                    key_env("SERPER_API_KEY"),
                    &input.query,
                    input.num.unwrap_or(8),
                )
                .await?,
            )),
            ToolCatalog::TavilySearch(input) => Ok(ToolOutput::text(
                engines::tavily(
                    &self.endpoints,
                    key_env("TAVILY_API_KEY"),
                    &input.query,
                    input.max_results.unwrap_or(8),
                )
                .await?,
            )),
            ToolCatalog::ExaSearch(input) => Ok(ToolOutput::text(
                engines::exa(
                    &self.endpoints,
                    key_env("EXA_API_KEY"),
                    &input.query,
                    input.num.unwrap_or(8),
                    input.text_chars.unwrap_or(1500),
                )
                .await?,
            )),
            ToolCatalog::DocsSearch(input) => Ok(ToolOutput::text(
                engines::docs(
                    &self.endpoints,
                    key_env("CONTEXT7_API_KEY"),
                    &input.library,
                    input.topic.as_deref(),
                    input.tokens.unwrap_or(4000),
                )
                .await?,
            )),
            ToolCatalog::NotebookEdit(input) => self.notebook(input, context).await,
            ToolCatalog::WikiConvert(input) => Ok(ToolOutput::text(
                engines::wiki_convert(
                    Path::new(&input.file_path),
                    input.vault.as_deref().map(Path::new),
                    &cwd,
                )
                .await?,
            )),
            ToolCatalog::WikiWrite(input) => Ok(ToolOutput::text(engines::wiki_write(
                Path::new(&input.vault),
                &input.kind,
                &input.title,
                &input.body,
                &input.tags.unwrap_or_default(),
                input.overwrite,
            )?)),
            ToolCatalog::WikiFinishIngest(input) => Ok(ToolOutput::text(
                engines::wiki_finish_ingest(
                    Path::new(&input.vault),
                    Path::new(&input.source),
                    &input.note,
                )
                .await?,
            )),
            other => bail!("{other} is not a capability tool"),
        }
    }

    async fn run_kernel(
        &self,
        input: Kernel,
        cwd: PathBuf,
        context: &ToolCallContext,
        bridge: CellToolBridge,
        names: Vec<String>,
    ) -> anyhow::Result<String> {
        let owner = context.conversation_id().to_string();
        let action = input.action.clone().unwrap_or_else(|| "run".to_string());
        let timeout = Duration::from_secs(input.timeout.unwrap_or(120).max(1));
        match action.as_str() {
            "kill" => {
                let stopped = self.kernels.stop(&owner).await;
                return Ok(if stopped {
                    "kernel stopped".to_string()
                } else {
                    "no kernel was running".to_string()
                });
            }
            "restart" => {
                self.kernels.stop(&owner).await;
            }
            _ => {}
        }
        let tool_bridge: ToolBridge = bridge;
        let tool_names: ToolNames = Arc::new(move || names.clone());
        let kernel = self
            .kernels
            .get_or_start(
                &owner,
                &cwd,
                &image_dir(&self.base()),
                tool_bridge,
                tool_names,
            )
            .await?;
        match action.as_str() {
            "restart" => Ok("kernel restarted; the namespace is empty".to_string()),
            "run" => {
                let code = input.code.unwrap_or_default();
                let code = if code.trim().is_empty() {
                    "pass".to_string()
                } else {
                    code
                };
                let id = kernel.submit(&code).await?;
                Ok(kernel.wait(id, timeout).await)
            }
            "poll" => {
                let id = kernel.resolve(input.handle.as_deref())?;
                Ok(kernel.wait(id, Duration::ZERO).await)
            }
            "wait" => {
                let id = kernel.resolve(input.handle.as_deref())?;
                Ok(kernel.wait(id, timeout).await)
            }
            "interrupt" => {
                let id = kernel.resolve(input.handle.as_deref())?;
                kernel.interrupt().await?;
                Ok(kernel.wait(id, Duration::from_secs(10)).await)
            }
            other => {
                bail!("action must be run, poll, wait, interrupt, restart or kill; got {other:?}")
            }
        }
    }
}
