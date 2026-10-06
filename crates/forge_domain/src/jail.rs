//! X20: `--jail DIR` / `-C DIR`, the bubblewrap jail that keeps the agent's
//! tools inside DIR.
//!
//! The jail keeps the agent's context clean; it is not a defence against a
//! hostile model. Under a jail:
//!
//! - every command a tool starts runs under `bwrap`: the system read-only,
//!   `/home`, the operator's home and `/run/user` replaced by empty tmpfs
//!   mounts, a private `/tmp`, DIR bound read-write at its real path, the PATH
//!   directories and rustup's toolchains under a hidden tree bound back
//!   read-only, the `jail_bind` entries bound as configured, the network
//!   shared;
//! - the file tools resolve every path and refuse one outside DIR and the bound
//!   paths ([`Jail::confine`]).

use std::path::{Component, Path, PathBuf};

/// A path visible in the jail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JailBind {
    pub path: PathBuf,
    pub rw: bool,
}

impl JailBind {
    /// Parses `path`, `path:ro` or `path:rw`; `~` expands to `home`.
    ///
    /// # Errors
    /// Returns an error for an empty or relative path.
    pub fn parse(spec: &str, home: &Path) -> anyhow::Result<Self> {
        let text = spec.trim();
        let (text, rw) = if let Some(rest) = text.strip_suffix(":rw") {
            (rest, true)
        } else if let Some(rest) = text.strip_suffix(":ro") {
            (rest, false)
        } else {
            (text, false)
        };
        if text.is_empty() {
            anyhow::bail!("empty jail bind: {spec:?}");
        }
        let path = match text.strip_prefix('~') {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(text),
        };
        if !path.is_absolute() {
            anyhow::bail!("jail bind path must be absolute or start with ~: {spec:?}");
        }
        Ok(Self { path: normalize(&path), rw })
    }
}

/// `path` with `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with symlinks resolved; the longest existing prefix is resolved
/// and the rest appended, so a file that does not exist yet still resolves.
fn real(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut existing = path.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&existing) {
            let mut out = resolved;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path,
        }
    }
}

fn under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// The jail of this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jail {
    /// DIR, resolved.
    pub root: PathBuf,
    /// The `bwrap` binary.
    pub bwrap: PathBuf,
    /// Private directory holding the jail's `/tmp`.
    pub private: PathBuf,
    /// `jail_bind` entries.
    pub binds: Vec<JailBind>,
    /// The operator's home.
    pub home: PathBuf,
    /// Directories the file tools may also read (tool results, spills).
    pub readable: Vec<PathBuf>,
}

impl Jail {
    /// The jail for `root` with `binds` (`path[:ro|:rw]` specs). The private
    /// `/tmp` lives under `state_dir/jail/<pid>/tmp`.
    ///
    /// # Errors
    /// Returns an error when `bwrap` is not on `path_env`, `root` is not a
    /// directory, or a bind spec is invalid.
    pub fn new(
        root: &Path,
        binds: &[String],
        home: &Path,
        state_dir: &Path,
        path_env: &str,
    ) -> anyhow::Result<Self> {
        let bwrap = find_bwrap(path_env).ok_or_else(|| {
            anyhow::anyhow!("--jail needs bubblewrap (bwrap) on PATH; install it or run without -C")
        })?;
        if !root.is_dir() {
            anyhow::bail!("--jail {}: not a directory", root.display());
        }
        let binds = binds
            .iter()
            .map(|spec| JailBind::parse(spec, home))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let private = state_dir.join("jail").join(std::process::id().to_string());
        std::fs::create_dir_all(private.join("tmp"))?;
        Ok(Self {
            root: real(root),
            bwrap,
            private,
            binds,
            home: home.to_path_buf(),
            readable: vec![state_dir.join("tool-results")],
        })
    }

    /// Trees replaced by an empty tmpfs, outermost first.
    pub fn hidden_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        let candidates = [
            PathBuf::from("/home"),
            real(&self.home),
            PathBuf::from("/run/user"),
        ];
        let mut sorted: Vec<PathBuf> = candidates.into_iter().filter(|p| p.is_dir()).collect();
        sorted.sort_by_key(|p| p.components().count());
        for candidate in sorted {
            if candidate == Path::new("/") || roots.iter().any(|r| under(&candidate, r)) {
                continue;
            }
            roots.push(candidate);
        }
        roots
    }

    /// `argv` wrapped to run in the jail with `cwd` as working directory.
    /// `path_env` is the PATH the command runs with; its directories under a
    /// hidden tree are bound back read-only.
    pub fn argv(&self, argv: &[String], cwd: &Path, path_env: &str) -> Vec<String> {
        let hidden = self.hidden_roots();
        let mut out: Vec<String> = [
            "--die-with-parent",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        out.push("--bind".to_string());
        out.push(self.private.join("tmp").display().to_string());
        out.push("/tmp".to_string());
        for root in &hidden {
            out.push("--tmpfs".to_string());
            out.push(root.display().to_string());
        }
        // The private directory also shows at its real path, so a socket or
        // file forge puts there (the kernel's tool bridge) is the same path on
        // both sides.
        out.push("--bind".to_string());
        out.push(self.private.display().to_string());
        out.push(self.private.display().to_string());

        let mut explicit = vec![JailBind { path: self.root.clone(), rw: true }];
        explicit.extend(self.binds.iter().cloned());
        let covered = |path: &Path| explicit.iter().any(|bind| under(path, &real(&bind.path)));
        let mut implicit: Vec<JailBind> = path_env
            .split(':')
            .filter(|entry| entry.starts_with('/'))
            .map(|entry| real(Path::new(entry)))
            .filter(|path| path.is_dir())
            .map(|path| JailBind { path, rw: false })
            .collect();
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".rustup"));
        if rustup.is_dir() {
            implicit.push(JailBind { path: real(&rustup), rw: false });
        }
        implicit.extend(
            self.readable
                .iter()
                .map(|p| JailBind { path: real(p), rw: false }),
        );
        // Only paths a tmpfs hides need binding back, and only when an
        // explicit bind does not cover them.
        let replaced: Vec<PathBuf> = hidden
            .iter()
            .cloned()
            .chain([PathBuf::from("/tmp")])
            .collect();
        implicit.retain(|bind| {
            replaced
                .iter()
                .any(|h| under(&bind.path, h) && bind.path != *h)
                && !covered(&bind.path)
        });
        let mut binds: Vec<JailBind> = implicit.into_iter().chain(explicit).collect();
        binds.dedup();
        // Outer paths first, so a bind inside another lands on top of it.
        binds.sort_by_key(|bind| bind.path.components().count());
        for bind in binds {
            out.push(
                if bind.rw {
                    "--bind-try"
                } else {
                    "--ro-bind-try"
                }
                .to_string(),
            );
            out.push(bind.path.display().to_string());
            out.push(bind.path.display().to_string());
        }
        out.push("--chdir".to_string());
        out.push(cwd.display().to_string());
        out.push("--".to_string());
        out.extend(argv.iter().cloned());
        out
    }

    /// The full argv (bwrap first) that runs `argv` in this jail, with the
    /// process's PATH bound back.
    pub fn wrap(&self, argv: &[String], cwd: &Path) -> Vec<String> {
        let path_env = std::env::var("PATH").unwrap_or_default();
        let mut out = vec![self.bwrap.display().to_string()];
        out.extend(self.argv(argv, cwd, &path_env));
        out
    }

    /// The host path a file tool uses for `path`.
    ///
    /// # Errors
    /// Returns a one-line error when the jail does not show `path`, or shows
    /// it read-only and `write` is set.
    pub fn confine(&self, path: &Path, write: bool) -> anyhow::Result<PathBuf> {
        let resolved = real(path);
        let mut areas: Vec<JailBind> = vec![
            JailBind { path: real(&self.root), rw: true },
            JailBind { path: real(&self.private), rw: true },
        ];
        areas.extend(
            self.binds
                .iter()
                .map(|b| JailBind { path: real(&b.path), rw: b.rw }),
        );
        areas.extend(
            self.readable
                .iter()
                .map(|p| JailBind { path: real(p), rw: false }),
        );
        // The innermost area containing the path decides, as the innermost
        // mount does inside the jail.
        let best = areas
            .iter()
            .filter(|area| under(&resolved, &area.path))
            .max_by_key(|area| area.path.components().count());
        match best {
            None => anyhow::bail!(
                "{} is outside the jail: forge -C keeps the tools in {} and its bound paths",
                path.display(),
                self.root.display()
            ),
            Some(area) if write && !area.rw => anyhow::bail!(
                "{} is read-only in the jail (bound from {})",
                path.display(),
                area.path.display()
            ),
            Some(_) => Ok(resolved),
        }
    }
}

/// Finds `bwrap` on `path_env`.
pub fn find_bwrap(path_env: &str) -> Option<PathBuf> {
    path_env
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("bwrap"))
        .find(|candidate| candidate.is_file())
}

/// The jail of this process, set once at startup (`--jail`). Subagents run
/// in this process and share it.
static ACTIVE: std::sync::OnceLock<Jail> = std::sync::OnceLock::new();

/// Enters `jail` for the rest of the process.
///
/// # Errors
/// Returns an error when a jail is already active.
pub fn enter_jail(jail: Jail) -> anyhow::Result<()> {
    ACTIVE
        .set(jail)
        .map_err(|_| anyhow::anyhow!("a jail is already active"))
}

/// The active jail, if any.
pub fn active_jail() -> Option<&'static Jail> {
    ACTIVE.get()
}

/// X20: `argv` as it must be spawned — wrapped in bwrap when a jail is
/// active, unchanged otherwise. Every process a tool starts goes through
/// this (or through the shell/terminal engines, which wrap the same way).
pub fn jailed_argv(argv: Vec<String>, cwd: &Path) -> Vec<String> {
    match active_jail() {
        Some(jail) => jail.wrap(&argv, cwd),
        None => argv,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn fixture(root: &Path, home: &Path) -> Jail {
        Jail {
            root: root.to_path_buf(),
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            private: root.join(".private"),
            binds: vec![JailBind { path: home.join("shared"), rw: false }],
            home: home.to_path_buf(),
            readable: vec![],
        }
    }

    #[test]
    fn test_parse_bind_modes_and_tilde() {
        let home = Path::new("/home/u");
        let actual = (
            JailBind::parse("~/data:rw", home).unwrap(),
            JailBind::parse("/opt/x", home).unwrap(),
            JailBind::parse("rel/path", home).is_err(),
        );
        let expected = (
            JailBind { path: PathBuf::from("/home/u/data"), rw: true },
            JailBind { path: PathBuf::from("/opt/x"), rw: false },
            true,
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_confine_allows_root_and_binds_refuses_outside_and_ro_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(home.join("shared")).unwrap();
        let jail = fixture(&root, &home);

        let inside = jail.confine(&root.join("src/new.rs"), true).is_ok();
        let bound_read = jail.confine(&home.join("shared/notes.md"), false).is_ok();
        let bound_write = jail
            .confine(&home.join("shared/notes.md"), true)
            .unwrap_err()
            .to_string();
        let outside = jail
            .confine(&home.join("secret"), false)
            .unwrap_err()
            .to_string();
        let escape = jail.confine(&root.join("../home/secret"), false).is_err();

        assert!(inside && bound_read && escape);
        assert!(
            bound_write.contains("is read-only in the jail"),
            "{bound_write}"
        );
        assert!(outside.contains("is outside the jail"), "{outside}");
    }

    #[test]
    fn test_symlink_out_of_the_jail_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();
        let jail = fixture(&root, &dir.path().join("home"));

        let actual = jail.confine(&root.join("link/file"), false).is_err();

        assert!(actual);
    }

    #[test]
    fn test_argv_binds_root_rw_and_ends_with_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let jail = fixture(&root, &dir.path().join("home"));

        let argv = jail.argv(
            &["sh".to_string(), "-c".to_string(), "true".to_string()],
            &root,
            "/usr/bin",
        );

        let root_text = root.display().to_string();
        let rw = argv
            .windows(3)
            .any(|w| w[0] == "--bind-try" && w[1] == root_text && w[2] == root_text);
        assert!(rw, "{argv:?}");
        assert!(
            argv.windows(3).any(|w| w == ["--ro-bind", "/", "/"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == "/home"),
            "{argv:?}"
        );
        assert_eq!(&argv[argv.len() - 4..], &["--", "sh", "-c", "true"]);
    }
}
