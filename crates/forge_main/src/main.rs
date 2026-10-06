use std::io::{IsTerminal, Read};
use std::panic;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use forge_api::ForgeAPI;
use forge_config::ForgeConfig;
use forge_domain::TitleFormat;
use forge_main::{Cli, Sandbox, TitleDisplayExt, TopLevelCommand, UI, tracker};

/// Enables ENABLE_VIRTUAL_TERMINAL_PROCESSING on the stdout console handle.
///
/// The `enable_ansi_support` crate sets VT processing on the `CONOUT$` handle,
/// but console mode flags are **per-handle** on Windows. The `CONOUT$` flag may
/// not propagate to the individual `STD_OUTPUT_HANDLE` handle on all Windows
/// configurations (e.g. older builds, cmd.exe launched in certain ways, or
/// when handles have been duplicated).
///
/// Without VT processing on stdout, ANSI escape codes from forge's markdown
/// renderer (bold, colors, inline code styling) are displayed as raw text
/// like `←[33m` instead of being interpreted as formatting.
///
/// We intentionally do NOT set VT processing on stderr. The `console` crate
/// (used by `indicatif`) uses `GetConsoleMode` to detect VT support and
/// switches between Win32 Console APIs and ANSI escapes accordingly. The
/// Win32 Console API path (`FillConsoleOutputCharacterA` /
/// `SetConsoleCursorPosition`) modifies the screen buffer in-place, which
/// produces clean scrollback when clearing spinner lines. Enabling VT
/// processing on stderr would cause `console` to use ANSI escapes instead,
/// leaving spinner artifacts in the terminal scrollback buffer.
#[cfg(windows)]
fn enable_stdout_vt_processing() {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{}", TitleFormat::error(format!("{err}")).display());
        if let Some(cause) = err.chain().nth(1) {
            eprintln!("{cause}");
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Enable ANSI escape code support on Windows console.
    // `enable_ansi_support` sets VT processing on the `CONOUT$` screen buffer
    // handle. We additionally set it on `STD_OUTPUT_HANDLE` directly, since
    // console mode flags are per-handle and `CONOUT$` may not propagate to
    // individual handles on all Windows configurations.
    #[cfg(windows)]
    {
        let _ = enable_ansi_support::enable_ansi_support();
        enable_stdout_vt_processing();
    }

    // Install default rustls crypto provider (ring) before any TLS connections
    // This is required for rustls 0.23+ when multiple crypto providers are
    // available
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Set up panic hook for better error display
    panic::set_hook(Box::new(|panic_info| {
        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unexpected error occurred".to_string()
        };

        println!("{}", TitleFormat::error(message.to_string()).display());
        tracker::error_blocking(message);
        std::process::exit(1);
    }));

    // Initialize and run the UI
    let mut cli = Cli::parse();

    // Check if there's piped input, but skip for `forge select` since that
    // command uses stdin for its item list.
    let is_select = matches!(cli.subcommands, Some(TopLevelCommand::Select(_)));
    if !is_select && !std::io::stdin().is_terminal() {
        let mut stdin_content = String::new();
        std::io::stdin().read_to_string(&mut stdin_content)?;
        let trimmed_content = stdin_content.trim();
        if !trimmed_content.is_empty() {
            cli.piped_input = Some(trimmed_content.to_string());
        }
    }

    // Read and validate configuration at startup so any errors are surfaced
    // immediately rather than silently falling back to defaults at runtime.
    let mut config =
        ForgeConfig::read().context("Failed to read Forge configuration from .forge.toml")?;
    // X18: `--restricted` turns the permission policy on.
    if cli.restricted {
        config.restricted = true;
    }
    for (tool, bans) in &config.tool_bans {
        for ban in bans {
            if let Some(error) = forge_domain::invalid_ban(&ban.pattern) {
                anyhow::bail!(
                    "tool_bans.{tool}: invalid pattern {:?}: {error}",
                    ban.pattern
                );
            }
        }
    }

    // Handle worktree creation if specified
    let cwd: PathBuf = match (&cli.worktree, &cli.directory, &cli.jail) {
        (_, _, Some(jail)) => jail
            .canonicalize()
            .with_context(|| format!("--jail {}: no such directory", jail.display()))?,
        (Some(worktree), Some(cli), _) => {
            let mut worktree = Sandbox::new(worktree).create()?;
            worktree.push(cli);
            worktree
        }
        (Some(worktree), _, _) => Sandbox::new(worktree).create()?,
        (_, Some(cli), _) => match cli.canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => panic!("Invalid path: {}", cli.display()),
        },
        (_, _, _) => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };

    // X20: enter the jail before anything spawns a tool process. A missing
    // bwrap is an error here, not at the first tool call.
    let jail_root = cli
        .jail
        .as_ref()
        .map(|_| cwd.clone())
        .or(config.jail.clone());
    if let Some(root) = jail_root {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let state_dir = forge_config::ConfigReader::base_path();
        let path_env = std::env::var("PATH").unwrap_or_default();
        let jail = forge_domain::Jail::new(&root, &config.jail_bind, &home, &state_dir, &path_env)?;
        forge_domain::enter_jail(jail)?;
    }

    // X19: session_start / session_end hooks bracket the whole session.
    let hooks = config.hooks.clone();
    let session_cwd = cwd.clone();
    run_session_hooks(&hooks, forge_config::HookEvent::SessionStart, &session_cwd).await;

    let mut ui = UI::init(cli, config, move |config| {
        ForgeAPI::init(cwd.clone(), config)
    })?;
    ui.run().await;

    run_session_hooks(&hooks, forge_config::HookEvent::SessionEnd, &session_cwd).await;
    Ok(())
}

/// Runs the session hooks of `event` (observers; exit status ignored).
async fn run_session_hooks(
    hooks: &[forge_config::UserHook],
    event: forge_config::HookEvent,
    cwd: &std::path::Path,
) {
    let payload = serde_json::json!({"hook_event_name": event.name(), "cwd": cwd});
    for hook in forge_app::user_hooks::matching(hooks, event, None) {
        if let Err(error) = forge_app::user_hooks::run_and_queue(hook, &payload, cwd).await {
            tracing::warn!(command = %hook.command, %error, "hook failed to start");
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_main::TopLevelCommand;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_stdin_detection_logic() {
        // This test verifies that the logic for detecting stdin is correct
        // We can't easily test the actual stdin reading in a unit test,
        // but we can verify the logic flow

        // Test that when prompt is provided, it remains independent of piped
        // input
        let cli_with_prompt = Cli::parse_from(["forge", "--prompt", "existing prompt"]);
        let original_prompt = cli_with_prompt.prompt.clone();

        // The prompt should remain as provided
        assert_eq!(original_prompt, Some("existing prompt".to_string()));

        // Test that when no prompt is provided, piped_input field exists
        let cli_no_prompt = Cli::parse_from(["forge"]);
        assert_eq!(cli_no_prompt.prompt, None);
        assert_eq!(cli_no_prompt.piped_input, None);
    }

    #[test]
    fn test_cli_parsing_with_short_flag() {
        // Test that the short flag -p also works correctly
        let cli_with_short_prompt = Cli::parse_from(["forge", "-p", "short flag prompt"]);
        assert_eq!(
            cli_with_short_prompt.prompt,
            Some("short flag prompt".to_string())
        );
    }

    #[test]
    fn test_cli_parsing_other_flags_work_with_piping() {
        // Test that other CLI flags still work when expecting stdin input
        let cli_with_flags = Cli::parse_from(["forge", "--verbose"]);
        assert_eq!(cli_with_flags.prompt, None);
        assert_eq!(cli_with_flags.verbose, true);
    }

    #[test]
    fn test_commit_command_diff_field_initially_none() {
        // Test that the diff field in CommitCommandGroup starts as None
        let cli = Cli::parse_from(["forge", "commit", "--preview"]);
        if let Some(TopLevelCommand::Commit(commit_group)) = cli.subcommands {
            assert_eq!(commit_group.preview, true);
            assert_eq!(commit_group.diff, None);
        } else {
            panic!("Expected Commit command");
        }
    }
}
