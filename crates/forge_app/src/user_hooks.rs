//! X19: user hooks (`hooks` config) on tool calls, tool results and turn
//! boundaries, with Claude Code's protocol: the event goes to the command's
//! stdin as one JSON object, and under `tool_call` exit status 2 refuses the
//! call; the first non-blank line of stderr (else stdout) is the refusal the
//! model reads.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use forge_config::{HookEvent, UserHook};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The exit status that refuses a `tool_call` (Claude Code's blocking
/// status).
pub const REFUSE_STATUS: i32 = 2;
/// Output kept per stream.
const OUTPUT_CAP: usize = 64 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How one hook command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// `None` when the command was killed at its timeout.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl HookOutcome {
    /// The refusal line of a `tool_call` hook that exited with status 2.
    pub fn refusal(&self, tool: &str) -> Option<String> {
        if self.status != Some(REFUSE_STATUS) {
            return None;
        }
        let line = [&self.stderr, &self.stdout]
            .iter()
            .flat_map(|stream| stream.lines())
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string);
        Some(line.unwrap_or_else(|| format!("a tool_call hook refused {tool}.")))
    }
}

/// The hooks of `hooks` that run on `event` for `tool` (`None` for events
/// without a tool).
pub fn matching<'a>(
    hooks: &'a [UserHook],
    event: HookEvent,
    tool: Option<&str>,
) -> impl Iterator<Item = &'a UserHook> {
    hooks.iter().filter(move |hook| {
        hook.event == event
            && (hook.tools.is_empty()
                || tool.is_some_and(|name| hook.tools.iter().any(|t| t == name)))
    })
}

/// Hook stdout waiting for the next user message, where it reaches the model
/// once as a reminder.
static PENDING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Queues a hook's non-blank stdout for the next user message.
pub fn queue_reminder(stdout: &str) {
    let text = stdout.trim();
    if !text.is_empty() {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
    }
}

/// Takes every queued hook output.
pub fn take_reminders() -> Vec<String> {
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Runs `hook` and queues its stdout as a reminder (observers and
/// non-refusing `tool_call` hooks).
///
/// # Errors
/// Returns an error when the command cannot be started.
pub async fn run_and_queue(
    hook: &UserHook,
    payload: &Value,
    cwd: &Path,
) -> anyhow::Result<HookOutcome> {
    let outcome = run_hook(hook, payload, cwd).await?;
    if outcome.status != Some(REFUSE_STATUS) {
        queue_reminder(&outcome.stdout);
    }
    Ok(outcome)
}

/// Runs one hook command under `sh -c` in its own process group with
/// `payload` on stdin; the group is killed at the hook's timeout.
///
/// # Errors
/// Returns an error when the command cannot be started.
pub async fn run_hook(hook: &UserHook, payload: &Value, cwd: &Path) -> anyhow::Result<HookOutcome> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&hook.command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();

    let input = payload.to_string();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let io = async {
        if let Some(mut stdin) = stdin.take() {
            // A hook that ignores stdin may close it early; that is fine.
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        let read = |stream: Option<tokio::process::ChildStdout>| async move {
            let mut buf = Vec::new();
            if let Some(stream) = stream {
                let _ = stream.take(OUTPUT_CAP as u64).read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = |stream: Option<tokio::process::ChildStderr>| async move {
            let mut buf = Vec::new();
            if let Some(stream) = stream {
                let _ = stream.take(OUTPUT_CAP as u64).read_to_end(&mut buf).await;
            }
            buf
        };
        let (out, err) = tokio::join!(read(stdout.take()), read_err(stderr.take()));
        let status = child.wait().await;
        (out, err, status)
    };
    let timeout = hook
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);
    match tokio::time::timeout(timeout, io).await {
        Ok((out, err, status)) => Ok(HookOutcome {
            status: status.ok().and_then(|s| s.code()),
            stdout: bstr::ByteSlice::to_str_lossy(out.as_slice()).into_owned(),
            stderr: bstr::ByteSlice::to_str_lossy(err.as_slice()).into_owned(),
        }),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // SAFETY: signals the hook's own process group.
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
            Ok(HookOutcome { status: None, stdout: String::new(), stderr: String::new() })
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn hook(event: HookEvent, command: &str) -> UserHook {
        UserHook {
            event,
            command: command.to_string(),
            tools: vec![],
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn test_exit_two_refuses_with_first_stderr_line_and_reads_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = hook(
            HookEvent::ToolCall,
            "grep -q '\"tool_name\":\"shell\"' && { echo '' >&2; echo 'no shell today' >&2; exit 2; }",
        );

        let outcome = run_hook(&fixture, &json!({"tool_name": "shell"}), dir.path())
            .await
            .unwrap();

        assert_eq!(outcome.refusal("shell"), Some("no shell today".to_string()));
    }

    #[tokio::test]
    async fn test_stdout_is_queued_once_and_refusals_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let note = hook(HookEvent::TurnEnd, "echo 'tests are green'");
        let refuse = hook(HookEvent::ToolCall, "echo 'never shown'; exit 2");
        take_reminders();

        run_and_queue(&note, &json!({}), dir.path()).await.unwrap();
        run_and_queue(&refuse, &json!({}), dir.path())
            .await
            .unwrap();
        let first = take_reminders();
        let second = take_reminders();

        assert_eq!(first, vec!["tests are green".to_string()]);
        assert_eq!(second, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_other_exit_status_does_not_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = hook(HookEvent::ToolCall, "echo fine; exit 1");

        let outcome = run_hook(&fixture, &json!({}), dir.path()).await.unwrap();

        assert_eq!((outcome.status, outcome.refusal("read")), (Some(1), None));
    }

    #[tokio::test]
    async fn test_timeout_kills_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = UserHook {
            timeout_secs: Some(1),
            ..hook(HookEvent::ToolCall, "sleep 30")
        };
        let started = std::time::Instant::now();

        let outcome = run_hook(&fixture, &json!({}), dir.path()).await.unwrap();

        assert_eq!(outcome.status, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_matching_filters_event_and_tools() {
        let hooks = vec![
            UserHook {
                tools: vec!["shell".to_string()],
                ..hook(HookEvent::ToolCall, "a")
            },
            hook(HookEvent::ToolCall, "b"),
            hook(HookEvent::TurnEnd, "c"),
        ];
        let names = |tool| {
            matching(&hooks, HookEvent::ToolCall, Some(tool))
                .map(|h| h.command.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("shell"), vec!["a", "b"]);
        assert_eq!(names("read"), vec!["b"]);
    }
}
