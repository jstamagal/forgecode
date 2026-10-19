use std::path::PathBuf;
use std::sync::Arc;

use forge_domain::{ToolCallContext, ToolCatalog, ToolOutput};

use crate::fmt::content::FormatContent;
use crate::operation::ToolOperation;
use crate::services::{CapabilityService, ReadRequest, Services, ShellService};
use crate::{
    AgentRegistry, ConversationService, EnvironmentInfra, FsPatchService, FsReadService,
    FsRemoveService, FsSearchService, FsUndoService, FsWriteService, ImageReadService, McpService,
    NetFetchService, PlanCreateService, ProviderService, SkillFetchService,
};

pub struct ToolExecutor<S> {
    services: Arc<S>,
}

impl<
    S: FsReadService
        + ImageReadService
        + FsWriteService
        + FsSearchService
        + NetFetchService
        + FsRemoveService
        + FsPatchService
        + FsUndoService
        + ShellService
        + ConversationService
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + PlanCreateService
        + SkillFetchService
        + AgentRegistry
        + ProviderService
        + Services,
> ToolExecutor<S>
{
    pub fn new(services: Arc<S>) -> Self {
        Self { services }
    }

    /// Converts a path to absolute by joining it with the current working
    /// directory if it's relative
    fn normalize_path(&self, path: String) -> String {
        let env = self.services.get_environment();
        let path_buf = PathBuf::from(&path);

        if path_buf.is_absolute() {
            path
        } else {
            PathBuf::from(&env.cwd).join(path_buf).display().to_string()
        }
    }

    /// [`Self::normalize_path`], then X20: under a jail the path must be
    /// inside the jail (and writable when `write`); the refusal is a
    /// [`forge_domain::ToolRefusal`] because retrying cannot change it.
    fn confined(&self, path: String, write: bool) -> anyhow::Result<String> {
        let path = self.normalize_path(path);
        match forge_domain::active_jail() {
            Some(jail) => jail
                .confine(std::path::Path::new(&path), write)
                .map(|_| path)
                .map_err(|error| anyhow::Error::new(forge_domain::ToolRefusal(error.to_string()))),
            None => Ok(path),
        }
    }

    async fn call_internal(
        &self,
        input: ToolCatalog,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOperation> {
        Ok(match input {
            ToolCatalog::Read(input) => {
                let normalized_path = self.confined(input.file_path.clone(), false)?;
                let range = input.range.clone().unwrap_or_default();
                let request = ReadRequest {
                    path: normalized_path,
                    start_line: range.start_line.map(|line| line.max(1) as u64),
                    end_line: range.end_line.map(|line| line.max(1) as u64),
                    start_byte: range.start_byte,
                    end_byte: range.end_byte,
                    show_line_numbers: input.show_line_numbers,
                    vision: context.vision(),
                };
                let output = self.services.read_tool(request, context).await?;

                (input, output).into()
            }
            ToolCatalog::Write(input) => {
                let normalized_path = self.confined(input.file_path.clone(), true)?;
                let output = self
                    .services
                    .write_tool(
                        normalized_path,
                        input.content.clone(),
                        input.overwrite,
                        context,
                    )
                    .await?;
                (input, output).into()
            }
            ToolCatalog::FsSearch(input) => {
                let mut params = input;
                // Resolve the search root against the working directory.
                let root = params
                    .path
                    .clone()
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                params.path = Some(self.confined(root, false)?);
                let text = self.services.search(params).await?;
                ToolOperation::Output { output: ToolOutput::text(text) }
            }
            ToolCatalog::AstSearch(input) => {
                let mut params = input;
                let root = params
                    .path
                    .clone()
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                params.path = Some(self.confined(root, params.apply)?);
                let text = self.services.ast_search(params, context).await?;
                ToolOperation::Output { output: ToolOutput::text(text) }
            }
            ToolCatalog::Remove(input) => {
                let normalized_path = self.confined(input.path.clone(), true)?;
                let output = self
                    .services
                    .remove(normalized_path, input.permanent, context)
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Patch(input) => {
                let normalized_path = self.confined(input.file_path.clone(), true)?;
                let edits = input.resolve_edits()?;
                let output = self
                    .services
                    .patch(normalized_path, edits, input.is_batch(), context)
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Undo(input) => {
                let normalized_path = self.confined(input.path.clone(), true)?;
                let output = self.services.undo(normalized_path, context).await?;
                (input, output).into()
            }
            ToolCatalog::Shell(input) => {
                let cwd = input
                    .cwd
                    .clone()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                let normalized_cwd = self.confined(cwd, false)?;
                let text = self
                    .services
                    .shell_tool(input, PathBuf::from(normalized_cwd))
                    .await?;
                ToolOperation::Text { text }
            }
            ToolCatalog::Fetch(input) => {
                let save = input
                    .save
                    .clone()
                    .map(|path| self.confined(path, true).map(PathBuf::from))
                    .transpose()?;
                if let Some(target) = &save {
                    // A save overwrites; snapshot it so undo brings it back.
                    self.services.snapshot_before_write(target, context).await?;
                }
                let request = crate::services::FetchRequest {
                    url: input.url.clone(),
                    method: input.method(),
                    headers: input.headers.clone().unwrap_or_default(),
                    body: input.body.clone(),
                    json_body: input.json_body.clone(),
                    raw: input.raw.unwrap_or(false),
                    save,
                    vision: context.vision(),
                };
                let output = self.services.fetch_tool(request).await?;
                ToolOperation::Output { output }
            }
            ToolCatalog::Plan(input) => {
                let output = self
                    .services
                    .create_plan(
                        input.plan_name.clone(),
                        input.version.clone(),
                        input.content.clone(),
                        input.overwrite,
                        context,
                    )
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Skill(input) => {
                let skill = self.services.fetch_skill(input.name.clone()).await?;
                if skill.disable_model_invocation {
                    anyhow::bail!(
                        "skill {} is invoked by the user only (disable-model-invocation)",
                        skill.name
                    );
                }
                // T21: the skill's `tools:` that this agent may load are
                // loaded (published from the next model call).
                let surface = context.surface();
                let state = context.surface_state()?;
                for tool in &skill.tools {
                    if let Some(id) = surface.loadable_id(tool, &state.snapshot()) {
                        state.with(|s| s.loaded.insert(id));
                    }
                }
                state.with(|s| s.offered_skills.insert(skill.name.clone()));
                ToolOperation::Skill { output: skill }
            }
            ToolCatalog::TodoWrite(input) => {
                let before = context.get_todos()?;
                context.update_todos(input.todos.clone())?;
                let after = context.get_todos()?;
                ToolOperation::TodoWrite { before, after }
            }
            ToolCatalog::TodoRead(_input) => {
                let todos = context.get_todos()?;
                ToolOperation::TodoRead { output: todos }
            }
            ToolCatalog::Task(_) | ToolCatalog::ToolDiscovery(_) | ToolCatalog::Kernel(_) => {
                // Handled in ToolRegistry: they act on agents, on the
                // request's tool surface, or call back into the registry.
                unreachable!("task, tool_discovery and kernel are handled in ToolRegistry")
            }
            // Phase 7 capability tools: paths are made absolute and confined
            // (X20) here, the work happens in the capability service.
            mut input @ (ToolCatalog::TerminalSession(_)
            | ToolCatalog::TerminalSnapshot(_)
            | ToolCatalog::Lsp(_)
            | ToolCatalog::Toolbox(_)
            | ToolCatalog::Browse(_)
            | ToolCatalog::BrowserProbe(_)
            | ToolCatalog::SerperSearch(_)
            | ToolCatalog::TavilySearch(_)
            | ToolCatalog::ExaSearch(_)
            | ToolCatalog::DocsSearch(_)
            | ToolCatalog::NotebookEdit(_)
            | ToolCatalog::WikiConvert(_)
            | ToolCatalog::WikiWrite(_)
            | ToolCatalog::WikiFinishIngest(_)) => {
                match &mut input {
                    ToolCatalog::TerminalSession(i) => {
                        if let Some(cwd) = i.cwd.take() {
                            i.cwd = Some(self.confined(cwd, false)?);
                        }
                    }
                    ToolCatalog::Lsp(i) => {
                        i.file_path = self.confined(i.file_path.clone(), false)?
                    }
                    ToolCatalog::Browse(i) => {
                        if let Some(shot) = i.screenshot.take() {
                            i.screenshot = Some(self.confined(shot, true)?);
                        }
                        if let Some(path) = i.url.strip_prefix("file://") {
                            self.confined(path.to_string(), false)?;
                        }
                    }
                    ToolCatalog::BrowserProbe(i) => {
                        if let Some(dir) = i.output_dir.take() {
                            i.output_dir = Some(self.confined(dir, true)?);
                        }
                        if !i.target.contains("://") {
                            i.target = self.confined(i.target.clone(), false)?;
                        }
                    }
                    ToolCatalog::NotebookEdit(i) => {
                        i.file_path = self.confined(i.file_path.clone(), true)?
                    }
                    ToolCatalog::WikiConvert(i) => {
                        i.file_path = self.confined(i.file_path.clone(), false)?;
                        if let Some(vault) = i.vault.take() {
                            i.vault = Some(self.confined(vault, true)?);
                        }
                    }
                    ToolCatalog::WikiWrite(i) => i.vault = self.confined(i.vault.clone(), true)?,
                    ToolCatalog::WikiFinishIngest(i) => {
                        i.vault = self.confined(i.vault.clone(), true)?;
                        i.source = self.confined(i.source.clone(), true)?;
                    }
                    _ => {}
                }
                let cwd = self.services.get_environment().cwd;
                let output = self.services.run_capability(input, cwd, context).await?;
                ToolOperation::Output { output }
            }
            // M3: resource and prompt controls on a connected MCP server.
            ToolCatalog::McpResourceList(input) => {
                self.mcp_control(&input.server, crate::McpControl::ListResources)
                    .await?
            }
            ToolCatalog::McpResourceTemplates(input) => {
                self.mcp_control(&input.server, crate::McpControl::ListResourceTemplates)
                    .await?
            }
            ToolCatalog::McpResourceRead(input) => {
                self.mcp_control(
                    &input.server,
                    crate::McpControl::ReadResource { uri: input.uri },
                )
                .await?
            }
            ToolCatalog::McpResourceSubscribe(input) => {
                self.mcp_control(
                    &input.server,
                    crate::McpControl::Subscribe { uri: input.uri },
                )
                .await?
            }
            ToolCatalog::McpResourceUnsubscribe(input) => {
                self.mcp_control(
                    &input.server,
                    crate::McpControl::Unsubscribe { uri: input.uri },
                )
                .await?
            }
            ToolCatalog::McpPromptList(input) => {
                self.mcp_control(&input.server, crate::McpControl::ListPrompts)
                    .await?
            }
            ToolCatalog::McpPromptGet(input) => {
                let arguments = input.arguments.map(|args| {
                    args.into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect()
                });
                self.mcp_control(
                    &input.server,
                    crate::McpControl::GetPrompt { name: input.name, arguments },
                )
                .await?
            }
        })
    }

    async fn mcp_control(
        &self,
        server: &str,
        request: crate::McpControl,
    ) -> anyhow::Result<ToolOperation> {
        let value = self.services.mcp_control(server, request).await?;
        Ok(ToolOperation::Output { output: ToolOutput::text(serde_json::to_string(&value)?) })
    }

    pub async fn execute(
        &self,
        tool_input: ToolCatalog,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        let tool_kind = tool_input.kind();
        let env = self.services.get_environment();

        let execution_result = self.call_internal(tool_input.clone(), context).await;

        if let Err(ref error) = execution_result {
            tracing::error!(error = ?error, "Tool execution failed");
        }

        let operation = execution_result?;

        // Send formatted output message
        if let Some(output) = operation.to_content(&env) {
            context.send(output).await?;
        }

        context.with_metrics(|metrics| operation.into_tool_output(tool_kind, &env, metrics))
    }
}
