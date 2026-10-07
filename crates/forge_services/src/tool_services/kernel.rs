//! T16: a persistent IPython kernel per conversation, driven through an
//! embedded Python driver (`kernel_driver.py`, `jupyter_client`), with a
//! tool bridge: `tools.<name>(**kwargs)` in a cell calls back into forge over
//! a unix socket, and the call goes through the same registry checks as a
//! direct call (the caller supplies that dispatch).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;

const DRIVER: &str = include_str!("kernel_driver.py");
const CLIENT: &str = include_str!("kernel_client.py");

/// Runs a tool for a cell: (tool name, JSON arguments) → result text, or the
/// refusal/error text.
pub type ToolBridge = Arc<
    dyn Fn(String, Value) -> futures::future::BoxFuture<'static, Result<String, String>>
        + Send
        + Sync,
>;

/// Lists the tools a cell may call.
pub type ToolNames = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

#[derive(Debug, Default)]
struct Cell {
    first_line: String,
    output: String,
    /// Bytes of `output` already returned to the model.
    read: usize,
    done: Option<Value>,
}

#[derive(Default)]
struct State {
    cells: BTreeMap<u64, Cell>,
    running: Option<u64>,
    last: Option<u64>,
    dead: Option<String>,
}

/// One kernel.
pub struct KernelProcess {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    child: tokio::sync::Mutex<tokio::process::Child>,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    next_id: Mutex<u64>,
    bridge_task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for KernelProcess {
    fn drop(&mut self) {
        self.bridge_task.abort();
    }
}

impl KernelProcess {
    /// Starts a kernel in `cwd` whose cells reach tools through `bridge`.
    ///
    /// # Errors
    /// Returns an error when python3 / jupyter_client are missing or the
    /// kernel does not come up.
    pub async fn start(
        cwd: &Path,
        image_dir: &Path,
        bridge: ToolBridge,
        names: ToolNames,
    ) -> anyhow::Result<Self> {
        // Under a jail the socket directory and the image directory live in
        // the jail's private directory, which shows at the same path inside.
        // Images sit outside the socket directory so a restart keeps them.
        let jail = forge_domain::active_jail();
        let dir = match jail {
            Some(jail) => tempfile::tempdir_in(&jail.private),
            None => tempfile::tempdir(),
        }
        .context("could not create the kernel's socket directory")?;
        let image_dir = match jail {
            Some(jail) => jail.private.join("kernel-images"),
            None => image_dir.to_path_buf(),
        };
        let driver = dir.path().join("forge_kernel_driver.py");
        std::fs::write(&driver, DRIVER)?;
        let socket_path = dir.path().join("tools.sock");
        let token = format!("{:032x}", rand_token());
        let listener = UnixListener::bind(&socket_path)?;
        let bridge_task = tokio::spawn(serve_bridge(listener, token.clone(), bridge, names));

        let setup = format!(
            "def __forge_install_tools():\n    import sys, types\n    module = types.ModuleType('forge_tools')\n    exec(compile({client:?}, 'forge_tools', 'exec'), module.__dict__)\n    sys.modules['forge_tools'] = module\n    globals()['tools'] = module.Tools({sock:?}, {token:?})\n__forge_install_tools()\ndel __forge_install_tools\n",
            client = CLIENT,
            sock = socket_path.display().to_string(),
        );
        let argv = forge_domain::jailed_argv(
            vec!["python3".to_string(), driver.display().to_string()],
            cwd,
        );
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .env("FORGE_KERNEL_SETUP", setup)
            .env("FORGE_KERNEL_IMAGES", &image_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("could not start python3 for the kernel")?;
        let stdin = child.stdin.take().context("kernel stdin")?;
        let stdout = child.stdout.take().context("kernel stdout")?;
        let mut stderr = child.stderr.take().context("kernel stderr")?;
        let state = Arc::new(Mutex::new(State::default()));
        let changed = Arc::new(Notify::new());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        {
            let state = state.clone();
            let changed = changed.clone();
            tokio::spawn(async move {
                let mut ready = Some(ready_tx);
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    match event["event"].as_str() {
                        Some("ready") => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Ok(()));
                            }
                        }
                        Some("output") => {
                            if let Some(cell) =
                                event["id"].as_u64().and_then(|id| s.cells.get_mut(&id))
                            {
                                let text = event["text"].as_str().unwrap_or_default();
                                match event["kind"].as_str() {
                                    Some("image") => {
                                        cell.output.push_str(&format!("IMAGE {text}\n"))
                                    }
                                    _ => cell.output.push_str(text),
                                }
                            }
                        }
                        Some("done") => {
                            if let Some(id) = event["id"].as_u64() {
                                if let Some(cell) = s.cells.get_mut(&id) {
                                    cell.done = Some(event.clone());
                                }
                                if s.running == Some(id) {
                                    s.running = None;
                                }
                            }
                        }
                        Some("dead") => {
                            let message = event["message"]
                                .as_str()
                                .unwrap_or("the kernel died")
                                .to_string();
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(message.clone()));
                            }
                            s.dead = Some(message);
                            s.running = None;
                        }
                        _ => {}
                    }
                    drop(s);
                    changed.notify_waiters();
                }
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                if s.dead.is_none() {
                    s.dead = Some("the kernel driver exited".to_string());
                }
                s.running = None;
                drop(s);
                changed.notify_waiters();
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Err(
                        "the kernel driver exited before the kernel was ready".to_string()
                    ));
                }
            });
        }
        let ready = tokio::time::timeout(Duration::from_secs(90), ready_rx).await;
        let failure = match ready {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(message))) => Some(message),
            Ok(Err(_)) => Some("the kernel driver exited".to_string()),
            Err(_) => Some("the kernel did not become ready within 90s".to_string()),
        };
        if let Some(reason) = failure {
            let mut err = String::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(2), stderr.read_to_string(&mut err)).await;
            let hint = if err.contains("No module named 'jupyter_client'") {
                " (python3 has no jupyter_client; install jupyter_client and ipykernel)"
            } else {
                ""
            };
            let tail: String = err.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            let tail = if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            };
            bail!("kernel failed to start: {reason}{hint}{tail}");
        }
        Ok(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            child: tokio::sync::Mutex::new(child),
            state,
            changed,
            next_id: Mutex::new(0),
            bridge_task,
            _dir: dir,
        })
    }

    async fn send(&self, request: Value) -> anyhow::Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Submits a cell; returns its id.
    ///
    /// # Errors
    /// Returns an error when a cell is running or the kernel is dead.
    pub async fn submit(&self, code: &str) -> anyhow::Result<u64> {
        let id = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(dead) = &s.dead {
                bail!("the kernel is dead ({dead}); use action=\"restart\"");
            }
            if let Some(running) = s.running {
                let first = s
                    .cells
                    .get(&running)
                    .map(|c| c.first_line.clone())
                    .unwrap_or_default();
                bail!(
                    "the previous cell is still running ({first}); interrupt it or wait (handle {running})"
                );
            }
            let mut next = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            let id = *next;
            s.cells.insert(
                id,
                Cell {
                    first_line: code.lines().next().unwrap_or_default().to_string(),
                    ..Default::default()
                },
            );
            s.running = Some(id);
            s.last = Some(id);
            id
        };
        self.send(json!({"op": "run", "id": id, "code": code}))
            .await?;
        Ok(id)
    }

    /// The cell to act on: `handle`, else the running one, else the last.
    ///
    /// # Errors
    /// Returns an error when there is no such cell.
    pub fn resolve(&self, handle: Option<&str>) -> anyhow::Result<u64> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = match handle {
            Some(h) => h
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|id| s.cells.contains_key(id)),
            None => s.running.or(s.last),
        };
        id.with_context(|| {
            format!(
                "no kernel cell{}",
                handle.map(|h| format!(" {h}")).unwrap_or_default()
            )
        })
    }

    /// Waits up to `timeout` for cell `id` to finish, then reports what it
    /// produced since the last report.
    pub async fn wait(&self, id: u64, timeout: Duration) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            {
                let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let finished =
                    s.cells.get(&id).is_none_or(|c| c.done.is_some()) || s.dead.is_some();
                if finished {
                    break;
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
        self.report(id)
    }

    fn report(&self, id: u64) -> String {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dead = s.dead.clone();
        let Some(cell) = s.cells.get_mut(&id) else {
            return format!("no kernel cell {id}");
        };
        let new = cell.output[cell.read..].to_string();
        cell.read = cell.output.len();
        let mut out = new;
        match (&cell.done, dead) {
            (Some(done), _) => {
                if done["status"] == "error" && !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
                let names = |key: &str| -> String {
                    done[key]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "(none)".to_string())
                };
                out.push_str(&format!("NAMESPACE {}\n", names("namespace")));
                let defined = names("defined");
                if defined != "(none)" {
                    out.push_str(&format!("DEFINED {defined}\n"));
                }
            }
            (None, Some(dead)) => out.push_str(&format!(
                "KERNEL DEAD: {dead} (cell {id}); use action=\"restart\"\n"
            )),
            (None, None) => out.push_str(&format!(
                "cell {id} is still running.\nHANDLE {id} RUNNING\n"
            )),
        }
        out
    }

    /// Interrupts the running cell.
    ///
    /// # Errors
    /// Returns an error when the driver cannot be reached.
    pub async fn interrupt(&self) -> anyhow::Result<()> {
        self.send(json!({"op": "interrupt"})).await
    }

    /// Stops the kernel.
    pub async fn shutdown(&self) {
        let _ = self.send(json!({"op": "shutdown"})).await;
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

fn rand_token() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    let mut a = std::collections::hash_map::RandomState::new().build_hasher();
    a.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    b.write_u64(a.finish());
    ((a.finish() as u128) << 64) | b.finish() as u128
}

/// Serves `tools.<name>(...)` calls: one JSON request per connection.
async fn serve_bridge(listener: UnixListener, token: String, bridge: ToolBridge, names: ToolNames) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        if tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
            .await
            .is_err()
        {
            continue;
        }
        let reply = match serde_json::from_slice::<Value>(&buf) {
            Ok(request) if request["token"].as_str() == Some(token.as_str()) => match request["op"]
                .as_str()
            {
                Some("names") => json!({"value": names()}),
                Some("call") => {
                    let tool = request["tool"].as_str().unwrap_or_default().to_string();
                    let args = request.get("kwargs").cloned().unwrap_or(json!({}));
                    // One request at a time, in order: the bridge is serial.
                    match bridge(tool, args).await {
                        Ok(value) => json!({"value": value}),
                        Err(error) => json!({"error": error}),
                    }
                }
                _ => json!({"error": "unknown bridge request"}),
            },
            _ => json!({"error": "bad or unauthenticated bridge request"}),
        };
        let _ = stream.write_all(reply.to_string().as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

/// Every kernel of the process, one per conversation.
#[derive(Default)]
pub struct Kernels {
    kernels: tokio::sync::Mutex<BTreeMap<String, Arc<KernelProcess>>>,
}

impl Kernels {
    /// The conversation's kernel, started on first use.
    ///
    /// # Errors
    /// Returns an error when the kernel cannot be started.
    pub async fn get_or_start(
        &self,
        owner: &str,
        cwd: &Path,
        image_dir: &Path,
        bridge: ToolBridge,
        names: ToolNames,
    ) -> anyhow::Result<Arc<KernelProcess>> {
        let mut kernels = self.kernels.lock().await;
        if let Some(kernel) = kernels.get(owner) {
            return Ok(kernel.clone());
        }
        let kernel = Arc::new(KernelProcess::start(cwd, image_dir, bridge, names).await?);
        kernels.insert(owner.to_string(), kernel.clone());
        Ok(kernel)
    }

    /// Stops and forgets the conversation's kernel.
    pub async fn stop(&self, owner: &str) -> bool {
        let kernel = self.kernels.lock().await.remove(owner);
        match kernel {
            Some(kernel) => {
                kernel.shutdown().await;
                true
            }
            None => false,
        }
    }

    /// The conversation's kernel if it is running.
    pub async fn get(&self, owner: &str) -> Option<Arc<KernelProcess>> {
        self.kernels.lock().await.get(owner).cloned()
    }
}

/// Default image directory for kernel output.
pub fn image_dir(base: &Path) -> PathBuf {
    base.join("kernel")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn bridge() -> (ToolBridge, ToolNames) {
        let bridge: ToolBridge = Arc::new(|tool: String, args: Value| {
            Box::pin(async move {
                match tool.as_str() {
                    "echo" => Ok(format!(
                        "echo:{}",
                        args["text"].as_str().unwrap_or_default()
                    )),
                    _ => Err(format!("unknown tool {tool}")),
                }
            })
        });
        let names: ToolNames = Arc::new(|| vec!["echo".to_string()]);
        (bridge, names)
    }

    fn jupyter_available() -> bool {
        std::process::Command::new("python3")
            .args(["-c", "import jupyter_client, ipykernel"])
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_state_persists_tools_bridge_and_handles() {
        if !jupyter_available() {
            eprintln!("skipped: python3 with jupyter_client/ipykernel is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (bridge, names) = bridge();
        let kernel = KernelProcess::start(dir.path(), &dir.path().join("img"), bridge, names)
            .await
            .unwrap();

        let first = kernel
            .submit("def double(x):\n    return 2 * x\nvalue = double(21)")
            .await
            .unwrap();
        let first_report = kernel.wait(first, Duration::from_secs(60)).await;
        let second = kernel
            .submit("print(value, tools.echo(text='hi'))")
            .await
            .unwrap();
        let second_report = kernel.wait(second, Duration::from_secs(60)).await;
        let slow = kernel.submit("import time\ntime.sleep(30)").await.unwrap();
        let running = kernel.wait(slow, Duration::from_millis(500)).await;
        let refused = kernel.submit("1").await.unwrap_err().to_string();
        kernel.interrupt().await.unwrap();
        let interrupted = kernel.wait(slow, Duration::from_secs(30)).await;
        let survived = kernel.submit("print(double(5))").await.unwrap();
        let survived_report = kernel.wait(survived, Duration::from_secs(60)).await;
        kernel.shutdown().await;

        assert!(first_report.contains("NAMESPACE double"), "{first_report}");
        assert!(
            first_report.contains("DEFINED double, value"),
            "{first_report}"
        );
        assert!(second_report.starts_with("42 echo:hi\n"), "{second_report}");
        assert!(
            running.ends_with(&format!("HANDLE {slow} RUNNING\n")),
            "{running}"
        );
        assert!(
            refused.starts_with("the previous cell is still running"),
            "{refused}"
        );
        assert!(interrupted.contains("KeyboardInterrupt"), "{interrupted}");
        assert_eq!(survived_report.lines().next(), Some("10"));
    }
}
