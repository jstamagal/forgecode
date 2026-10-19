//! X27: managed binaries — the few tools not replaced by crates, kept in a
//! directory under forgecode's data dir and verified against sha256 pins in
//! source. Resolution prefers the managed copy and falls back to PATH with a
//! warning. Nothing is downloaded unless the user runs `forge tools
//! --install`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A pinned release of a managed binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedBinary {
    pub name: &'static str,
    pub version: &'static str,
    /// Release asset (a `.tar.gz`).
    pub url: &'static str,
    pub asset_sha256: &'static str,
    /// Member of the archive that is the executable.
    pub member: &'static str,
    pub executable_sha256: &'static str,
    /// Files the executable will not run without: (archive member, sha256).
    pub companions: &'static [(&'static str, &'static str)],
}

/// The managed binaries (pins copied from js's `tool_binaries.py`, read from
/// the projects' release pages on 2026-08-10). Only x86_64 Linux is pinned.
pub const MANAGED_BINARIES: [ManagedBinary; 1] = [ManagedBinary {
    name: "obscura",
    version: "0.2.2",
    url: "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.2/obscura-x86_64-linux-stealth.tar.gz",
    asset_sha256: "faf46c28948c10c6d44d6f46faad577adba43d63bb19b83cdb92a5e22bdd5da1",
    member: "obscura",
    executable_sha256: "0e30b1ee35e3f3f291fed3cc55f7284964c8c741dcc24de1b171fcefa3444cdf",
    companions: &[(
        "obscura-worker",
        "34edfd3c79c45f04e86a8bae6994956b06d611cb5951d9b497ef91f449541321",
    )],
}];

/// The managed bin directory under `base_path`.
pub fn managed_bin_dir(base_path: &Path) -> PathBuf {
    base_path.join("bin")
}

/// Hex sha256 of a file.
///
/// # Errors
/// Returns an error when the file cannot be read.
pub fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Where a binary was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The verified managed copy.
    Managed(PathBuf),
    /// A copy on PATH (unverified); the warning says so.
    Path { path: PathBuf, warning: String },
}

impl Resolved {
    /// The executable path.
    pub fn path(&self) -> &Path {
        match self {
            Resolved::Managed(path) => path,
            Resolved::Path { path, .. } => path,
        }
    }
}

/// Resolves `name`: the managed copy when it (and its companions) match their
/// pins, else the first `name` on `path_env` with a warning.
///
/// # Errors
/// Returns an error naming the install command when neither exists.
pub fn resolve_binary(name: &str, base_path: &Path, path_env: &str) -> anyhow::Result<Resolved> {
    let dir = managed_bin_dir(base_path);
    if let Some(pin) = MANAGED_BINARIES.iter().find(|b| b.name == name) {
        let executable = dir.join(pin.name);
        let verified = file_sha256(&executable).is_ok_and(|h| h == pin.executable_sha256)
            && pin
                .companions
                .iter()
                .all(|(member, sha)| file_sha256(&dir.join(member)).is_ok_and(|h| h == *sha));
        if verified {
            return Ok(Resolved::Managed(executable));
        }
    }
    let on_path = path_env
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(name))
        .find(|candidate| candidate.is_file());
    match on_path {
        Some(path) => Ok(Resolved::Path {
            warning: format!(
                "{name}: using {} from PATH; it is not the pinned managed copy (run `forge tools --install`)",
                path.display()
            ),
            path,
        }),
        None => anyhow::bail!(
            "{name} is not installed: run `forge tools --install` to download the pinned release into {}",
            dir.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_unverified_managed_copy_falls_back_to_path_with_warning() {
        let base = tempfile::tempdir().unwrap();
        let bin = managed_bin_dir(base.path());
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("obscura"), b"tampered").unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("obscura"), b"x").unwrap();

        let actual =
            resolve_binary("obscura", base.path(), &other.path().display().to_string()).unwrap();

        assert_eq!(actual.path(), other.path().join("obscura"));
        assert!(
            matches!(actual, Resolved::Path { ref warning, .. } if warning.contains("not the pinned managed copy"))
        );
    }

    #[test]
    fn test_missing_binary_names_the_install_command() {
        let base = tempfile::tempdir().unwrap();
        let actual = resolve_binary("obscura", base.path(), "")
            .unwrap_err()
            .to_string();
        assert!(actual.contains("forge tools --install"), "{actual}");
    }

    #[test]
    fn test_file_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"abc").unwrap();
        let actual = file_sha256(&file).unwrap();
        assert_eq!(
            actual,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
