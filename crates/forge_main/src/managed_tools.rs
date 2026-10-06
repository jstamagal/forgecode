//! `forge tools [--install]` (X27): reports the managed binaries and, with
//! `--install`, downloads each pinned release, verifies the archive and the
//! extracted files against their sha256 pins, and installs them atomically
//! into the managed bin directory.

use std::path::Path;

use anyhow::{Context, bail};
use forge_domain::{MANAGED_BINARIES, ManagedBinary, Resolved, file_sha256, managed_bin_dir};

/// Runs `forge tools`; returns the report to print.
///
/// # Errors
/// Returns an error when an install fails (download, checksum or extract).
pub async fn run(install: bool, base_path: &Path) -> anyhow::Result<String> {
    let mut lines = Vec::new();
    for binary in MANAGED_BINARIES {
        if install {
            install_one(&binary, base_path).await?;
        }
        let path_env = std::env::var("PATH").unwrap_or_default();
        let state = match forge_domain::resolve_binary(binary.name, base_path, &path_env) {
            Ok(Resolved::Managed(path)) => format!("installed (verified) at {}", path.display()),
            Ok(Resolved::Path { warning, .. }) => warning,
            Err(error) => error.to_string(),
        };
        lines.push(format!("{} {}: {state}", binary.name, binary.version));
    }
    Ok(lines.join("\n"))
}

async fn install_one(binary: &ManagedBinary, base_path: &Path) -> anyhow::Result<()> {
    let dir = managed_bin_dir(base_path);
    std::fs::create_dir_all(&dir)?;
    let work = tempfile::tempdir_in(&dir)?;
    let archive = work.path().join("asset.tar.gz");

    let bytes = reqwest::get(binary.url)
        .await
        .with_context(|| format!("download of {} failed", binary.url))?
        .error_for_status()?
        .bytes()
        .await?;
    std::fs::write(&archive, &bytes)?;
    let actual = file_sha256(&archive)?;
    if actual != binary.asset_sha256 {
        bail!(
            "{}: archive sha256 {actual} does not match the pinned {}",
            binary.name,
            binary.asset_sha256
        );
    }

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(work.path())
        .status()
        .context("could not run tar to extract the archive")?;
    if !status.success() {
        bail!("{}: tar exited with {status}", binary.name);
    }

    let mut files = vec![(binary.member, binary.name, binary.executable_sha256)];
    files.extend(
        binary
            .companions
            .iter()
            .map(|(member, sha)| (*member, *member, *sha)),
    );
    for (member, _, sha) in &files {
        let extracted = work.path().join(member);
        let actual = file_sha256(&extracted)
            .with_context(|| format!("{}: {member} missing from the archive", binary.name))?;
        if actual != *sha {
            bail!(
                "{}: {member} sha256 {actual} does not match the pinned {sha}",
                binary.name
            );
        }
    }
    for (member, installed, _) in files {
        let target = dir.join(installed);
        let staged = work.path().join(member);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staged, &target)?;
    }
    Ok(())
}
