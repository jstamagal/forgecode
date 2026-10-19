//! T18: language-server queries. One server process per (server, workspace
//! root), kept alive for the process; JSON-RPC over stdio; the file is sent
//! as it is on disk at each call (didOpen once, then didChange + didSave).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

/// A server forge knows how to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: &'static str,
    pub command: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub roots: &'static [&'static str],
}

/// Built-in servers, in preference order per extension.
pub const SERVERS: [ServerSpec; 6] = [
    ServerSpec {
        name: "rust-analyzer",
        command: &["rust-analyzer"],
        extensions: &["rs"],
        roots: &["Cargo.toml"],
    },
    ServerSpec {
        name: "basedpyright",
        command: &["basedpyright-langserver", "--stdio"],
        extensions: &["py", "pyi"],
        roots: &["pyproject.toml", "setup.py", "setup.cfg"],
    },
    ServerSpec {
        name: "pyright",
        command: &["pyright-langserver", "--stdio"],
        extensions: &["py", "pyi"],
        roots: &["pyproject.toml", "setup.py", "setup.cfg"],
    },
    ServerSpec {
        name: "gopls",
        command: &["gopls"],
        extensions: &["go"],
        roots: &["go.mod"],
    },
    ServerSpec {
        name: "typescript-language-server",
        command: &["typescript-language-server", "--stdio"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        roots: &["tsconfig.json", "package.json"],
    },
    ServerSpec {
        name: "clangd",
        command: &["clangd"],
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        roots: &["compile_commands.json", "CMakeLists.txt", ".clangd"],
    },
];

/// The LSP language id of a file extension.
pub fn language_id(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "go" => "go",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "c" | "h" => "c",
        _ => "cpp",
    }
}

fn on_path(program: &str, path_env: &str) -> bool {
    path_env
        .split(':')
        .any(|dir| !dir.is_empty() && Path::new(dir).join(program).is_file())
}

/// The first installed server for `file`.
///
/// # Errors
/// Returns an error naming the servers looked for.
pub fn pick_server(file: &Path, path_env: &str) -> anyhow::Result<&'static ServerSpec> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let candidates: Vec<&ServerSpec> = SERVERS
        .iter()
        .filter(|s| s.extensions.contains(&ext.as_str()))
        .collect();
    if candidates.is_empty() {
        bail!("no language server covers .{ext} files");
    }
    candidates
        .iter()
        .find(|s| on_path(s.command[0], path_env))
        .copied()
        .with_context(|| {
            format!(
                "no language server for .{ext} is installed (looked for {} on PATH)",
                candidates
                    .iter()
                    .map(|s| s.command[0])
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The nearest ancestor of `file` holding one of `markers`, else `fallback`.
pub fn workspace_root(file: &Path, markers: &[&str], fallback: &Path) -> PathBuf {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if markers.iter().any(|m| d.join(m).exists()) {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    fallback.to_path_buf()
}

fn uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

fn path_of(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

type Writer = Arc<Mutex<tokio::process::ChildStdin>>;
type Pending = Arc<std::sync::Mutex<BTreeMap<i64, oneshot::Sender<Value>>>>;
type Diagnostics = Arc<std::sync::Mutex<BTreeMap<String, (u64, Value)>>>;

async fn write_message(stdin: &Writer, message: &Value) -> anyhow::Result<()> {
    let body = message.to_string();
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        .await?;
    stdin.flush().await?;
    Ok(())
}

/// One running language server.
pub struct Server {
    stdin: Writer,
    _child: Mutex<tokio::process::Child>,
    pending: Pending,
    next: std::sync::Mutex<i64>,
    diagnostics: Diagnostics,
    diag_changed: Arc<tokio::sync::Notify>,
    open: Mutex<BTreeMap<String, (i64, String)>>,
}

impl Server {
    /// Starts and initializes `spec` in `root`.
    ///
    /// # Errors
    /// Returns an error when the server cannot be started or initialized.
    pub async fn start(spec: &ServerSpec, root: &Path) -> anyhow::Result<Self> {
        // X20: the server reads the workspace, so it runs in the jail.
        let argv =
            forge_domain::jailed_argv(spec.command.iter().map(|s| s.to_string()).collect(), root);
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start {}", spec.name))?;
        let stdin: Writer = Arc::new(Mutex::new(child.stdin.take().context("server stdin")?));
        let stdout = child.stdout.take().context("server stdout")?;
        let pending: Pending = Default::default();
        let diagnostics: Diagnostics = Default::default();
        let diag_changed = Arc::new(tokio::sync::Notify::new());
        {
            let (stdin, pending, diagnostics, diag_changed) = (
                stdin.clone(),
                pending.clone(),
                diagnostics.clone(),
                diag_changed.clone(),
            );
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let line = line.trim();
                        if line.is_empty() {
                            break;
                        }
                        if let Some(v) = line.strip_prefix("Content-Length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    if reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    let is_response = message.get("method").is_none();
                    if is_response {
                        if let Some(id) = message.get("id").and_then(Value::as_i64)
                            && let Some(tx) = pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&id)
                        {
                            let _ = tx.send(message);
                        }
                        continue;
                    }
                    if message["method"] == "textDocument/publishDiagnostics" {
                        let uri = message["params"]["uri"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let mut d = diagnostics.lock().unwrap_or_else(|e| e.into_inner());
                        let n = d.get(&uri).map(|(n, _)| n + 1).unwrap_or(1);
                        d.insert(uri, (n, message["params"]["diagnostics"].clone()));
                        drop(d);
                        diag_changed.notify_waiters();
                    } else if message.get("id").is_some() {
                        // A server request (configuration, progress,
                        // registration) gets an empty answer so it does not
                        // wait on us.
                        let result = if message["method"] == "workspace/configuration" {
                            let n = message["params"]["items"]
                                .as_array()
                                .map(Vec::len)
                                .unwrap_or(0);
                            Value::Array(vec![Value::Null; n])
                        } else {
                            Value::Null
                        };
                        let reply =
                            json!({"jsonrpc": "2.0", "id": message["id"], "result": result});
                        let _ = write_message(&stdin, &reply).await;
                    }
                }
            });
        }
        let server = Self {
            stdin,
            _child: Mutex::new(child),
            pending,
            next: std::sync::Mutex::new(0),
            diagnostics,
            diag_changed,
            open: Mutex::new(BTreeMap::new()),
        };
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let init = server
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": uri(root),
                    "workspaceFolders": [{"uri": uri(root), "name": name}],
                    "capabilities": {
                        "textDocument": {
                            "publishDiagnostics": {"relatedInformation": false},
                            "hover": {"contentFormat": ["markdown", "plaintext"]},
                            "synchronization": {"didSave": true},
                        },
                        "workspace": {"workspaceFolders": true, "configuration": true},
                    },
                }),
                Duration::from_secs(60),
            )
            .await?;
        if init.get("error").is_some() {
            bail!("{} refused initialize: {}", spec.name, init["error"]);
        }
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        write_message(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await?;
        tokio::time::timeout(timeout, rx)
            .await
            .with_context(|| format!("{method} timed out"))?
            .context("the language server exited")
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        write_message(
            &self.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    /// Sends `file` as it is on disk; returns its URI and whether it changed.
    async fn sync(&self, file: &Path) -> anyhow::Result<(String, bool)> {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let uri = uri(file);
        let mut open = self.open.lock().await;
        let changed = match open.get_mut(&uri) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id(&ext), "version": 1, "text": text}}),
                )
                .await?;
                open.insert(uri.clone(), (1, text));
                true
            }
            Some((version, old)) if *old != text => {
                *version += 1;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": *version}, "contentChanges": [{"text": text}]}),
                )
                .await?;
                self.notify(
                    "textDocument/didSave",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
                *old = text;
                true
            }
            Some(_) => false,
        };
        Ok((uri, changed))
    }

    /// Diagnostics for `file`: waits for a fresh publish after a change.
    async fn diagnostics(&self, file: &Path, wait: Duration) -> anyhow::Result<Value> {
        let before = {
            let d = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
            d.get(&uri(file)).map(|(n, _)| *n).unwrap_or(0)
        };
        let (uri, changed) = self.sync(file).await?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.diag_changed.notified();
            {
                let d = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((n, list)) = d.get(&uri)
                    && (!changed || *n > before)
                {
                    return Ok(list.clone());
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let d = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
                return Ok(d.get(&uri).map(|(_, l)| l.clone()).unwrap_or(json!([])));
            }
        }
    }

    async fn at(
        &self,
        method: &str,
        file: &Path,
        line: u32,
        character: u32,
        extra: Value,
    ) -> anyhow::Result<Value> {
        let (uri, _) = self.sync(file).await?;
        let mut params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line.saturating_sub(1), "character": character.saturating_sub(1)},
        });
        if let (Some(p), Value::Object(extra)) = (params.as_object_mut(), extra) {
            p.extend(extra);
        }
        let reply = self
            .request(method, params, Duration::from_secs(60))
            .await?;
        if let Some(error) = reply.get("error") {
            bail!(
                "{method} failed: {}",
                error["message"].as_str().unwrap_or("unknown error")
            );
        }
        Ok(reply["result"].clone())
    }
}

fn source_line(path: &Path, line: u64) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .nth(line as usize)
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_default()
}

fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

fn render_locations(result: &Value, cwd: &Path) -> String {
    let items: Vec<Value> = match result {
        Value::Array(items) => items.clone(),
        Value::Null => vec![],
        single => vec![single.clone()],
    };
    let lines: Vec<String> = items
        .iter()
        .filter_map(|loc| {
            let uri = loc["uri"].as_str().or_else(|| loc["targetUri"].as_str())?;
            let range = if loc["range"].is_object() {
                &loc["range"]
            } else {
                &loc["targetSelectionRange"]
            };
            let path = path_of(uri)?;
            let line = range["start"]["line"].as_u64()?;
            let col = range["start"]["character"].as_u64()?;
            Some(format!(
                "{}:{}:{}: {}",
                shown(&path, cwd),
                line + 1,
                col + 1,
                source_line(&path, line)
            ))
        })
        .collect();
    if lines.is_empty() {
        "(no locations)".to_string()
    } else {
        lines.join("\n")
    }
}

fn render_hover(result: &Value) -> String {
    let contents = &result["contents"];
    let text = match contents {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| i["value"].as_str().unwrap_or_default().to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => contents["value"].as_str().unwrap_or_default().to_string(),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        "(no hover information)".to_string()
    } else {
        text
    }
}

fn render_diagnostics(list: &Value, file: &Path, cwd: &Path) -> String {
    let items = list.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return format!("{}: no diagnostics", shown(file, cwd));
    }
    items
        .iter()
        .map(|d| {
            let severity = match d["severity"].as_u64() {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                _ => "hint",
            };
            let line = d["range"]["start"]["line"].as_u64().unwrap_or(0);
            let col = d["range"]["start"]["character"].as_u64().unwrap_or(0);
            let source = d["source"]
                .as_str()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            format!(
                "{}:{}:{}: {severity}{source}: {}",
                shown(file, cwd),
                line + 1,
                col + 1,
                d["message"].as_str().unwrap_or_default().replace('\n', " ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every running language server, by (server, root).
#[derive(Default)]
pub struct LanguageServers {
    servers: Mutex<BTreeMap<(String, PathBuf), Arc<Server>>>,
}

impl LanguageServers {
    /// Runs one `lsp` query.
    ///
    /// # Errors
    /// Returns an error for a missing server, a bad operation or position.
    pub async fn query(
        &self,
        operation: &str,
        file: &Path,
        line: Option<u32>,
        character: Option<u32>,
        cwd: &Path,
        path_env: &str,
    ) -> anyhow::Result<String> {
        if !file.is_file() {
            bail!("no such file: {}", file.display());
        }
        let spec = pick_server(file, path_env)?;
        let root = workspace_root(file, spec.roots, cwd);
        let server = {
            let mut servers = self.servers.lock().await;
            let key = (spec.name.to_string(), root.clone());
            match servers.get(&key) {
                Some(server) => server.clone(),
                None => {
                    let server = Arc::new(Server::start(spec, &root).await?);
                    servers.insert(key, server.clone());
                    server
                }
            }
        };
        let position = || -> anyhow::Result<(u32, u32)> {
            Ok((
                line.context("this operation needs line (1-based)")?,
                character.context("this operation needs character (1-based column)")?,
            ))
        };
        match operation {
            "diagnostics" => {
                let list = server.diagnostics(file, Duration::from_secs(20)).await?;
                Ok(render_diagnostics(&list, file, cwd))
            }
            "definition" => {
                let (l, c) = position()?;
                Ok(render_locations(
                    &server
                        .at("textDocument/definition", file, l, c, json!({}))
                        .await?,
                    cwd,
                ))
            }
            "references" => {
                let (l, c) = position()?;
                let extra = json!({"context": {"includeDeclaration": true}});
                Ok(render_locations(
                    &server
                        .at("textDocument/references", file, l, c, extra)
                        .await?,
                    cwd,
                ))
            }
            "hover" => {
                let (l, c) = position()?;
                Ok(render_hover(
                    &server
                        .at("textDocument/hover", file, l, c, json!({}))
                        .await?,
                ))
            }
            other => bail!(
                "operation must be diagnostics, definition, references or hover; got {other:?}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_missing_server_names_what_was_looked_for() {
        let actual = (
            pick_server(Path::new("x.rs"), "").unwrap_err().to_string(),
            pick_server(Path::new("x.zig"), "").unwrap_err().to_string(),
        );
        let expected = (
            "no language server for .rs is installed (looked for rust-analyzer on PATH)"
                .to_string(),
            "no language server covers .zig files".to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_workspace_root_finds_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("m/src")).unwrap();
        std::fs::write(dir.path().join("m/go.mod"), "module m\n").unwrap();
        let actual = workspace_root(&dir.path().join("m/src/a.go"), &["go.mod"], dir.path());
        assert_eq!(actual, dir.path().join("m"));
    }

    #[tokio::test]
    async fn test_gopls_diagnostics_definition_and_hover() {
        let path_env = std::env::var("PATH").unwrap_or_default();
        if !on_path("gopls", &path_env) || !on_path("go", &path_env) {
            eprintln!("skipped: gopls/go not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module m\n\ngo 1.21\n").unwrap();
        let file = dir.path().join("main.go");
        std::fs::write(
            &file,
            "package main\n\nfunc add(a, b int) int { return a + b }\n\nfunc main() {\n\tvar s string = add(1, 2)\n\t_ = s\n}\n",
        )
        .unwrap();
        let servers = LanguageServers::default();

        let diagnostics = servers
            .query("diagnostics", &file, None, None, dir.path(), &path_env)
            .await
            .unwrap();
        let definition = servers
            .query(
                "definition",
                &file,
                Some(6),
                Some(17),
                dir.path(),
                &path_env,
            )
            .await
            .unwrap();
        let hover = servers
            .query("hover", &file, Some(6), Some(17), dir.path(), &path_env)
            .await
            .unwrap();

        assert!(diagnostics.contains("main.go:6:"), "{diagnostics}");
        assert!(diagnostics.contains("error"), "{diagnostics}");
        assert_eq!(
            definition,
            "main.go:3:6: func add(a, b int) int { return a + b }"
        );
        assert!(
            hover.contains("func add(a int, b int) int")
                || hover.contains("func add(a, b int) int"),
            "{hover}"
        );
    }
}
