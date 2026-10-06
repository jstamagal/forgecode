use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use bstr::ByteSlice;
use forge_app::domain::Environment;
use forge_app::{CommandInfra, EnvironmentInfra, ShellOutput, ShellService};
use forge_domain::{Shell, ShellAction};
use strip_ansi_escapes::strip;

use crate::tool_services::shell_jobs::{
    Excerpt, Exit, RunSpec, ShellJob, ShellJobs, filtered_references,
};

// Strips out the ansi codes from content.
fn strip_ansi(content: String) -> String {
    strip(content.as_bytes()).to_str_lossy().into_owned()
}

/// Environment names a command inherits when the config sets none.
pub const DEFAULT_ENV_ALLOW: [&str; 8] = [
    "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "PWD", "SHELL",
];

/// Toolchain locations inherited with PATH: rustup's cargo and rustc do not
/// run without them.
const TOOLCHAIN_ENV: [&str; 2] = ["RUSTUP_HOME", "CARGO_HOME"];

/// Seconds a call waits before returning a handle, when the model gives none.
const DEFAULT_WAIT_SECS: u64 = 30;

/// Bytes held back from the output budget for the header lines.
const FRAME_BYTES: usize = 512;

/// Runs shell commands for the model and for internal callers.
///
/// The `shell` tool path starts each command as a job in its own process
/// group (stdin `/dev/null`, `pipefail` under bash/zsh, filtered environment)
/// and waits a window for it; a command still running then keeps running and
/// the result names a handle. Commands run concurrently — there is no global
/// lock (X17).
pub struct ForgeShell<I> {
    env: Environment,
    infra: Arc<I>,
    jobs: Arc<ShellJobs>,
}

impl<I: EnvironmentInfra> ForgeShell<I> {
    /// Create a new Shell with environment configuration
    pub fn new(infra: Arc<I>) -> Self {
        let env = infra.get_environment();
        Self { env, infra, jobs: Arc::new(ShellJobs::default()) }
    }

    fn validate_command(command: &str) -> anyhow::Result<()> {
        if command.trim().is_empty() {
            bail!("Command string is empty or contains only whitespace");
        }
        Ok(())
    }
}

impl<I: EnvironmentInfra<Config = forge_config::ForgeConfig>> ForgeShell<I> {
    /// The allow-list plus `extra`, and the environment built from it.
    fn environment(&self, extra: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
        let configured = self
            .infra
            .get_config()
            .ok()
            .and_then(|config| config.shell_env_allow)
            .unwrap_or_else(|| DEFAULT_ENV_ALLOW.iter().map(|s| s.to_string()).collect());
        let mut allowed: Vec<String> = configured
            .into_iter()
            .chain(extra.iter().cloned())
            .collect();
        allowed.sort();
        allowed.dedup();
        let process_env = self.infra.get_env_vars();
        let env = allowed
            .iter()
            .map(String::as_str)
            .chain(TOOLCHAIN_ENV)
            .filter_map(|name| {
                process_env
                    .get(name)
                    .map(|value| (name.to_string(), value.clone()))
            })
            .collect();
        (allowed, env)
    }

    /// Output budget: the configured bash cap, kept inside the inline limit.
    fn output_budget(&self) -> usize {
        let config = self.infra.get_config().ok();
        let cap = config
            .as_ref()
            .map(|c| c.max_bash_output_bytes as usize)
            .filter(|cap| *cap > 0)
            .unwrap_or(150_000);
        let inline = config
            .map(|c| c.max_tool_result_inline_bytes as usize)
            .unwrap_or(0);
        if inline > FRAME_BYTES {
            cap.min(inline - FRAME_BYTES)
        } else {
            cap
        }
    }

    fn render_stream(
        &self,
        job: &ShellJob,
        name: &str,
        excerpt: &Excerpt,
        budget: usize,
    ) -> String {
        let clean = |bytes: &[u8]| {
            let text = bytes.to_str_lossy().into_owned();
            if job.keep_ansi {
                text
            } else {
                strip_ansi(text)
            }
        };
        let Some((first, stop)) = excerpt.omitted else {
            return clean(&excerpt.head);
        };
        let where_ = match &excerpt.path {
            Some(path) => format!(
                "the whole {name} is at {} — read it on with range {{\"start_byte\": {first}}}",
                path.display()
            ),
            None => format!("the whole {name} could not be saved"),
        };
        let mut head = clean(&excerpt.head);
        if !head.is_empty() && !head.ends_with('\n') {
            head.push('\n');
        }
        let tail = clean(&excerpt.tail);
        let marker = format!(
            "[truncated: output budget ({budget} bytes) reached; {name} bytes {first}-{stop} of {} ({} bytes) are not shown; {where_}]",
            excerpt.total,
            stop - first
        );
        if tail.is_empty() {
            format!("{head}{marker}")
        } else {
            format!("{head}{marker}\n{tail}")
        }
    }

    fn render_finished(
        &self,
        job: &ShellJob,
        exit: Exit,
        description: Option<&str>,
        since_last: bool,
        preface: Option<String>,
    ) -> String {
        let mut parts: Vec<String> = preface.into_iter().collect();
        parts.push(format!("shell={}", job.shell));
        parts.push(format!(
            "exit={}",
            exit.code.map_or("killed".to_string(), |c| c.to_string())
        ));
        if let Some(description) = description {
            parts.push(format!("description={description}"));
        }
        let process_env: HashMap<String, String> = self.infra.get_env_vars().into_iter().collect();
        let filtered = filtered_references(&job.command, &job.allowed, &process_env);
        if !filtered.is_empty() {
            parts.push(format!(
                "environment=filtered unset={} allowed={} present={}; names not allowed by shell_env_allow or the env parameter are unset",
                filtered.join(","),
                job.allowed.join(","),
                job.present.join(",")
            ));
        }
        let budget = self.output_budget();
        let [out, err] = job.output(budget, since_last);
        let stdout = self.render_stream(job, "stdout", &out, budget);
        let stderr = self.render_stream(job, "stderr", &err, budget);
        if !stdout.is_empty() {
            parts.push(format!("--- stdout ---\n{stdout}"));
        }
        if !stderr.is_empty() {
            parts.push(format!("--- stderr ---\n{stderr}"));
        }
        if stdout.is_empty() && stderr.is_empty() {
            parts.push("(no output)".to_string());
        }
        parts.join("\n")
    }

    fn render_running(&self, job: &ShellJob) -> String {
        let id = &job.id;
        let head = format!(
            "command still running after {}s (handle {id}, pid {}). It keeps running. Poll it with action=\"poll\", handle=\"{id}\" for new output, action=\"wait\", handle=\"{id}\", timeout=N to block for it, or action=\"kill\", handle=\"{id}\" to stop it.",
            job.started.elapsed().as_secs(),
            job.pid
        );
        let budget = self.output_budget();
        let [out, err] = job.output(budget, true);
        let stdout = self.render_stream(job, "stdout", &out, budget);
        let stderr = self.render_stream(job, "stderr", &err, budget);
        let mut parts = vec![head];
        if !stdout.is_empty() {
            parts.push(format!("--- stdout so far ---\n{stdout}"));
        }
        if !stderr.is_empty() {
            parts.push(format!("--- stderr so far ---\n{stderr}"));
        }
        parts.push(format!("HANDLE {id} RUNNING"));
        parts.join("\n")
    }

    async fn job_action(&self, input: &Shell) -> anyhow::Result<String> {
        let action = match input.action {
            ShellAction::Poll => "poll",
            ShellAction::Wait => "wait",
            _ => "kill",
        };
        let Some(job) = self.jobs.find(input.handle.as_deref()) else {
            bail!(
                "no shell job{} to {action}",
                input
                    .handle
                    .as_deref()
                    .map(|h| format!(" {h}"))
                    .unwrap_or_default()
            );
        };
        if input.action == ShellAction::Kill {
            let was_running = job.exit().is_none();
            let exit = job.kill().await.unwrap_or(Exit { code: None });
            let preface = if was_running {
                format!(
                    "killed handle {} after {}s",
                    job.id,
                    job.started.elapsed().as_secs()
                )
            } else {
                format!("handle {} had already exited", job.id)
            };
            return Ok(self.render_finished(
                &job,
                exit,
                input.description.as_deref(),
                true,
                Some(preface),
            ));
        }
        let window = match input.action {
            ShellAction::Poll => Duration::ZERO,
            _ => Duration::from_secs(input.timeout.unwrap_or(DEFAULT_WAIT_SECS).max(1)),
        };
        match job.wait(window).await {
            Some(exit) => {
                Ok(self.render_finished(&job, exit, input.description.as_deref(), true, None))
            }
            None => Ok(self.render_running(&job)),
        }
    }
}

#[async_trait::async_trait]
impl<I: CommandInfra + EnvironmentInfra<Config = forge_config::ForgeConfig>> ShellService
    for ForgeShell<I>
{
    async fn execute(
        &self,
        command: String,
        cwd: PathBuf,
        keep_ansi: bool,
        silent: bool,
        env_vars: Option<Vec<String>>,
        description: Option<String>,
    ) -> anyhow::Result<ShellOutput> {
        Self::validate_command(&command)?;

        let mut output = self
            .infra
            .execute_command(command, cwd, silent, env_vars)
            .await?;

        if !keep_ansi {
            output.stdout = strip_ansi(output.stdout);
            output.stderr = strip_ansi(output.stderr);
        }

        Ok(ShellOutput { output, shell: self.env.shell.clone(), description })
    }

    async fn shell_tool(&self, input: Shell, cwd: PathBuf) -> anyhow::Result<String> {
        if input.action != ShellAction::Run {
            return self.job_action(&input).await;
        }
        if input.command.trim().is_empty() {
            bail!("command is required for action \"run\"");
        }
        let (allowed, env) = self.environment(input.env.as_deref().unwrap_or_default());
        let spill_dir = self.env.base_path.join("tool-results");
        let job = self.jobs.start(RunSpec {
            command: input.command.clone(),
            cwd,
            shell: self.env.shell.clone(),
            env,
            allowed,
            keep_ansi: input.keep_ansi,
            cap: self.output_budget(),
            spill_dir,
            jail: forge_domain::active_jail().cloned(),
        })?;
        let window = Duration::from_secs(input.timeout.unwrap_or(DEFAULT_WAIT_SECS).max(1));
        // A call that is cancelled (the turn is interrupted) drops this future;
        // the guard then kills the job's process group.
        let mut guard = KillOnDrop(Some(job.clone()));
        let exit = job.wait(window).await;
        guard.0 = None;
        match exit {
            // The command outlived the window. It is NOT killed: the model
            // gets a handle to come back to it.
            None => Ok(self.render_running(&job)),
            Some(exit) => {
                Ok(self.render_finished(&job, exit, input.description.as_deref(), false, None))
            }
        }
    }
}

/// Kills a job's process group when a waiting call is dropped (cancelled).
struct KillOnDrop(Option<Arc<ShellJob>>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(job) = self.0.take()
            && job.exit().is_none()
        {
            job.kill_now();
        }
    }
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use forge_app::domain::{CommandOutput, Environment};
    use forge_app::{CommandInfra, EnvironmentInfra, ShellService};
    use forge_domain::ConfigOperation;
    use pretty_assertions::assert_eq;

    use super::*;

    struct MockCommandInfra {
        expected_env_vars: Option<Vec<String>>,
    }

    #[async_trait]
    impl CommandInfra for MockCommandInfra {
        async fn execute_command(
            &self,
            command: String,
            _working_dir: PathBuf,
            _silent: bool,
            env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<CommandOutput> {
            // Verify that environment variables are passed through correctly
            assert_eq!(env_vars, self.expected_env_vars);

            Ok(CommandOutput {
                stdout: "Mock output".to_string(),
                stderr: "".to_string(),
                command,
                exit_code: Some(0),
            })
        }

        async fn execute_command_raw(
            &self,
            _command: &str,
            _working_dir: PathBuf,
            _env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<std::process::ExitStatus> {
            unimplemented!()
        }
    }

    impl EnvironmentInfra for MockCommandInfra {
        type Config = forge_config::ForgeConfig;

        fn get_environment(&self) -> Environment {
            use fake::{Fake, Faker};
            Faker.fake()
        }

        fn get_config(&self) -> anyhow::Result<forge_config::ForgeConfig> {
            Ok(forge_config::ForgeConfig::default())
        }

        async fn update_environment(&self, _ops: Vec<ConfigOperation>) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn get_env_var(&self, _key: &str) -> Option<String> {
            None
        }

        fn get_env_vars(&self) -> std::collections::BTreeMap<String, String> {
            std::collections::BTreeMap::new()
        }
    }

    #[tokio::test]
    async fn test_shell_service_forwards_env_vars() {
        let fixture = ForgeShell::new(Arc::new(MockCommandInfra {
            expected_env_vars: Some(vec!["PATH".to_string(), "HOME".to_string()]),
        }));

        let actual = fixture
            .execute(
                "echo hello".to_string(),
                PathBuf::from("."),
                false,
                false,
                Some(vec!["PATH".to_string(), "HOME".to_string()]),
                None,
            )
            .await
            .unwrap();

        assert_eq!(actual.output.stdout, "Mock output");
        assert_eq!(actual.output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_shell_service_forwards_no_env_vars() {
        let fixture = ForgeShell::new(Arc::new(MockCommandInfra { expected_env_vars: None }));

        let actual = fixture
            .execute(
                "echo hello".to_string(),
                PathBuf::from("."),
                false,
                false,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(actual.output.stdout, "Mock output");
        assert_eq!(actual.output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_shell_service_forwards_empty_env_vars() {
        let fixture = ForgeShell::new(Arc::new(MockCommandInfra {
            expected_env_vars: Some(vec![]),
        }));

        let actual = fixture
            .execute(
                "echo hello".to_string(),
                PathBuf::from("."),
                false,
                false,
                Some(vec![]),
                None,
            )
            .await
            .unwrap();

        assert_eq!(actual.output.stdout, "Mock output");
        assert_eq!(actual.output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_shell_service_with_description() {
        let fixture = ForgeShell::new(Arc::new(MockCommandInfra { expected_env_vars: None }));

        let actual = fixture
            .execute(
                "echo hello".to_string(),
                PathBuf::from("."),
                false,
                false,
                None,
                Some("Prints hello to stdout".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(actual.output.stdout, "Mock output");
        assert_eq!(actual.output.exit_code, Some(0));
        assert_eq!(
            actual.description,
            Some("Prints hello to stdout".to_string())
        );
    }

    #[tokio::test]
    async fn test_shell_service_without_description() {
        let fixture = ForgeShell::new(Arc::new(MockCommandInfra { expected_env_vars: None }));

        let actual = fixture
            .execute(
                "echo hello".to_string(),
                PathBuf::from("."),
                false,
                false,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(actual.output.stdout, "Mock output");
        assert_eq!(actual.output.exit_code, Some(0));
        assert_eq!(actual.description, None);
    }

    /// Infra with a real environment: bash shell, a secret variable that is
    /// not allowed through, and PATH.
    struct RealEnv {
        base: PathBuf,
    }

    #[async_trait]
    impl CommandInfra for RealEnv {
        async fn execute_command(
            &self,
            _command: String,
            _working_dir: PathBuf,
            _silent: bool,
            _env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<CommandOutput> {
            unimplemented!()
        }

        async fn execute_command_raw(
            &self,
            _command: &str,
            _working_dir: PathBuf,
            _env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<std::process::ExitStatus> {
            unimplemented!()
        }
    }

    impl EnvironmentInfra for RealEnv {
        type Config = forge_config::ForgeConfig;

        fn get_environment(&self) -> Environment {
            use fake::{Fake, Faker};
            let env: Environment = Faker.fake();
            env.shell("bash".to_string()).base_path(self.base.clone())
        }

        fn get_config(&self) -> anyhow::Result<forge_config::ForgeConfig> {
            Ok(forge_config::ForgeConfig {
                max_bash_output_bytes: 2000,
                max_tool_result_inline_bytes: 51200,
                ..Default::default()
            })
        }

        async fn update_environment(&self, _ops: Vec<ConfigOperation>) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn get_env_var(&self, key: &str) -> Option<String> {
            self.get_env_vars().get(key).cloned()
        }

        fn get_env_vars(&self) -> std::collections::BTreeMap<String, String> {
            let mut vars = std::collections::BTreeMap::new();
            vars.insert(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            );
            vars.insert("FORGE_TEST_SECRET".to_string(), "hunter2".to_string());
            vars
        }
    }

    fn shell(command: &str) -> Shell {
        Shell { command: command.to_string(), ..Default::default() }
    }

    #[tokio::test]
    async fn test_shell_tool_returns_handle_then_poll_reports_exit() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));

        let started = fixture
            .shell_tool(
                Shell {
                    timeout: Some(1),
                    ..shell("echo first; sleep 2; echo second")
                },
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();
        let waited = fixture
            .shell_tool(
                Shell {
                    action: ShellAction::Wait,
                    handle: Some("1".to_string()),
                    timeout: Some(10),
                    ..Default::default()
                },
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();

        assert!(started.contains("HANDLE 1 RUNNING"), "{started}");
        assert!(
            started.contains("--- stdout so far ---\nfirst\n"),
            "{started}"
        );
        assert!(waited.contains("exit=0"), "{waited}");
        assert!(waited.contains("--- stdout ---\nsecond\n"), "{waited}");
        assert!(
            !waited.contains("first"),
            "poll reports only new output: {waited}"
        );
    }

    #[tokio::test]
    async fn test_shell_tool_filters_environment_and_names_unset_reference() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));

        let actual = fixture
            .shell_tool(
                shell("echo \"[$FORGE_TEST_SECRET]\""),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();

        assert!(actual.contains("--- stdout ---\n[]\n"), "{actual}");
        assert!(
            actual.contains("environment=filtered unset=FORGE_TEST_SECRET"),
            "{actual}"
        );
    }

    #[tokio::test]
    async fn test_shell_tool_env_param_passes_named_variable() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));

        let actual = fixture
            .shell_tool(
                Shell {
                    env: Some(vec!["FORGE_TEST_SECRET".to_string()]),
                    ..shell("echo \"[$FORGE_TEST_SECRET]\"")
                },
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();

        assert!(actual.contains("--- stdout ---\n[hunter2]\n"), "{actual}");
    }

    #[tokio::test]
    async fn test_shell_tool_kill_stops_running_job() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));
        fixture
            .shell_tool(
                Shell { timeout: Some(1), ..shell("sleep 60") },
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();

        let actual = fixture
            .shell_tool(
                Shell { action: ShellAction::Kill, ..Default::default() },
                dir.path().to_path_buf(),
            )
            .await
            .unwrap();

        assert!(actual.starts_with("killed handle 1 after"), "{actual}");
    }

    #[tokio::test]
    async fn test_shell_tool_large_output_names_spill_file() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));

        let actual = fixture
            .shell_tool(shell("seq 1 50000"), dir.path().to_path_buf())
            .await
            .unwrap();

        assert!(
            actual.contains("[truncated: output budget (2000 bytes) reached; stdout bytes"),
            "{actual}"
        );
        assert!(
            actual.contains("read it on with range {\"start_byte\":"),
            "{actual}"
        );
        assert!(actual.trim_end().ends_with("50000"), "{actual}");
    }

    #[tokio::test]
    async fn test_concurrent_shell_calls_do_not_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Arc::new(ForgeShell::new(Arc::new(RealEnv {
            base: dir.path().to_path_buf(),
        })));
        let started = std::time::Instant::now();

        let (a, b) = tokio::join!(
            fixture.shell_tool(shell("sleep 1; echo a"), dir.path().to_path_buf()),
            fixture.shell_tool(shell("sleep 1; echo b"), dir.path().to_path_buf()),
        );

        assert!(a.unwrap().contains("a\n"));
        assert!(b.unwrap().contains("b\n"));
        assert!(started.elapsed() < std::time::Duration::from_millis(1900));
    }

    #[tokio::test]
    async fn test_cancelled_call_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let fixture = ForgeShell::new(Arc::new(RealEnv { base: dir.path().to_path_buf() }));
        let command = format!("sleep 60 & echo $! > {}; wait", marker.display());

        // Interrupting the turn drops the call future while it waits.
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            fixture.shell_tool(shell(&command), dir.path().to_path_buf()),
        )
        .await;
        let pid: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 only probes whether the process exists.
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        assert!(cancelled.is_err());
        assert!(
            !alive,
            "the backgrounded grandchild {pid} survived the cancel"
        );
    }
}
