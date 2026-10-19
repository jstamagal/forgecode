//! T17: the kernel's persistent function library. One JSON file per function
//! (`<dir>/<name>.json`) holds every revision with its provenance; nothing is
//! overwritten. `save` refuses a source that reads free names it does not
//! define (checked with Python's `ast` via `toolbox_check.py`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CHECKER: &str = include_str!("toolbox_check.py");

/// One saved revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub revision: u32,
    pub source: String,
    pub note: String,
    pub session: String,
    pub saved_at: String,
}

/// A function and its history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub revisions: Vec<Revision>,
    /// Deleted: listed in history only.
    #[serde(default)]
    pub deleted: bool,
}

/// The two scopes' directories.
#[derive(Debug, Clone)]
pub struct ToolboxDirs {
    pub global: PathBuf,
    pub project: PathBuf,
}

impl ToolboxDirs {
    fn dir(&self, scope: &str) -> anyhow::Result<&Path> {
        match scope {
            "global" => Ok(&self.global),
            "project" => Ok(&self.project),
            other => bail!("scope must be global or project, got {other:?}"),
        }
    }
}

fn valid_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !ok {
        bail!("name must be a Python identifier, got {name:?}");
    }
    Ok(())
}

fn load(dir: &Path, name: &str) -> anyhow::Result<Option<Entry>> {
    let path = dir.join(format!("{name}.json"));
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("{} is corrupt", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn store(dir: &Path, entry: &Entry) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", entry.name));
    let partial = path.with_extension("json.tmp");
    std::fs::write(&partial, serde_json::to_string_pretty(entry)?)?;
    std::fs::rename(&partial, &path)?;
    Ok(())
}

fn entries(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    out.sort_by(|a: &Entry, b| a.name.cmp(&b.name));
    out
}

/// Checks `source`: one top-level def/class named `name`, no free names.
///
/// # Errors
/// Returns the refusal naming every free name, or a syntax/shape error.
pub async fn check_source(name: &str, source: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(CHECKER)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("toolbox needs python3 to check a function")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(source.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    let report: Value =
        serde_json::from_slice(&output.stdout).context("the toolbox checker returned no report")?;
    if let Some(error) = report["error"].as_str() {
        bail!("save refused: {error}");
    }
    let defined = report["name"].as_str().unwrap_or_default();
    if defined != name {
        bail!("save refused: the source defines {defined:?}, not {name:?}");
    }
    let free: Vec<&str> = report["free"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !free.is_empty() {
        bail!(
            "save refused: {name} reads names the source does not define or import: {}. Define or import them inside the source, or save them as their own functions first.",
            free.join(", ")
        );
    }
    Ok(())
}

/// Runs one toolbox action and returns the model-facing text. `load` returns
/// the code to exec into the kernel in `Ok((text, Some(code)))`.
///
/// # Errors
/// Returns the error the model should see.
pub async fn run(
    dirs: &ToolboxDirs,
    input: &forge_domain::Toolbox,
    session: &str,
) -> anyhow::Result<(String, Option<String>)> {
    let action = input.action.as_deref().unwrap_or("list");
    let name = input.name.as_deref();
    let source = input.source.as_deref();
    let note = input.note.as_deref();
    let scope = input.scope.as_deref().unwrap_or("global");
    let revision = input.revision;
    match action {
        "list" => {
            let mut lines = Vec::new();
            for (label, dir) in [("project", &dirs.project), ("global", &dirs.global)] {
                for entry in entries(dir).into_iter().filter(|e| !e.deleted) {
                    let last = entry.revisions.last();
                    lines.push(format!(
                        "{} r{} [{label}] {}",
                        entry.name,
                        last.map(|r| r.revision).unwrap_or(0),
                        last.map(|r| r.note.as_str()).unwrap_or_default()
                    ));
                }
            }
            Ok((
                if lines.is_empty() {
                    "(toolbox is empty)".to_string()
                } else {
                    lines.join("\n")
                },
                None,
            ))
        }
        "show" => {
            let name = name.context("show requires name")?;
            let entry = find(dirs, name)?.1;
            let rev = pick(&entry, revision)?;
            Ok((
                format!("# {name} r{} ({})\n{}", rev.revision, rev.note, rev.source),
                None,
            ))
        }
        "history" => {
            let name = name.context("history requires name")?;
            let entry = find(dirs, name)?.1;
            let lines: Vec<String> = entry
                .revisions
                .iter()
                .map(|r| {
                    format!(
                        "r{} {} session={} {}",
                        r.revision, r.saved_at, r.session, r.note
                    )
                })
                .collect();
            let deleted = if entry.deleted { "\n(deleted)" } else { "" };
            Ok((format!("{}{deleted}", lines.join("\n")), None))
        }
        "delete" => {
            let name = name.context("delete requires name")?;
            let (dir, mut entry) = find(dirs, name)?;
            entry.deleted = true;
            store(&dir, &entry)?;
            Ok((
                format!(
                    "deleted {name}; its {} revision(s) stay in history",
                    entry.revisions.len()
                ),
                None,
            ))
        }
        "save" => {
            let name = name.context("save requires name")?;
            valid_name(name)?;
            let source = source.context("save requires source (one top-level def)")?;
            check_source(name, source).await?;
            let dir = dirs.dir(scope)?.to_path_buf();
            let mut entry = load(&dir, name)?
                .unwrap_or_else(|| Entry { name: name.to_string(), ..Default::default() });
            entry.deleted = false;
            let next = entry.revisions.last().map(|r| r.revision + 1).unwrap_or(1);
            entry.revisions.push(Revision {
                revision: next,
                source: source.to_string(),
                note: note.unwrap_or_default().to_string(),
                session: session.to_string(),
                saved_at: chrono::Utc::now().to_rfc3339(),
            });
            store(&dir, &entry)?;
            Ok((format!("saved {name} r{next} [{scope}]"), None))
        }
        "load" => {
            let mut chosen: Vec<(String, Revision)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for dir in [&dirs.project, &dirs.global] {
                for entry in entries(dir).into_iter().filter(|e| !e.deleted) {
                    if name.is_some_and(|n| n != entry.name) || !seen.insert(entry.name.clone()) {
                        continue;
                    }
                    if let Ok(rev) = pick(&entry, revision) {
                        chosen.push((entry.name.clone(), rev.clone()));
                    }
                }
            }
            if chosen.is_empty() {
                bail!(
                    "nothing to load{}",
                    name.map(|n| format!(" named {n}")).unwrap_or_default()
                );
            }
            let code = chosen
                .iter()
                .map(|(_, rev)| rev.source.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let names: Vec<String> = chosen
                .iter()
                .map(|(n, r)| format!("{n} r{}", r.revision))
                .collect();
            Ok((format!("loaded {}", names.join(", ")), Some(code)))
        }
        other => bail!("action must be list, show, save, load, history or delete; got {other:?}"),
    }
}

fn find(dirs: &ToolboxDirs, name: &str) -> anyhow::Result<(PathBuf, Entry)> {
    for dir in [&dirs.project, &dirs.global] {
        if let Some(entry) = load(dir, name)? {
            return Ok((dir.clone(), entry));
        }
    }
    bail!("no saved function named {name:?}")
}

fn pick(entry: &Entry, revision: Option<u32>) -> anyhow::Result<&Revision> {
    match revision {
        Some(n) => entry
            .revisions
            .iter()
            .find(|r| r.revision == n)
            .with_context(|| format!("{} has no revision r{n}", entry.name)),
        None => entry
            .revisions
            .last()
            .with_context(|| format!("{} has no revisions", entry.name)),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn dirs(root: &Path) -> ToolboxDirs {
        ToolboxDirs { global: root.join("global"), project: root.join("project") }
    }

    /// A toolbox request: action, name, source, note, scope.
    fn req(
        action: &str,
        name: Option<&str>,
        source: Option<&str>,
        note: Option<&str>,
        scope: &str,
    ) -> forge_domain::Toolbox {
        forge_domain::Toolbox {
            action: Some(action.to_string()),
            name: name.map(str::to_string),
            source: source.map(str::to_string),
            note: note.map(str::to_string),
            scope: Some(scope.to_string()),
            revision: None,
        }
    }

    #[tokio::test]
    async fn test_save_revisions_history_load_and_free_name_refusal() {
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs(root.path());
        let v1 = "def twice(x):\n    return 2 * x\n";
        let v2 = "def twice(x):\n    import math\n    return math.prod([2, x])\n";
        let bad = "def twice(x):\n    return factor * x\n";

        let first = run(
            &dirs,
            &req("save", Some("twice"), Some(v1), Some("first"), "global"),
            "s1",
        )
        .await
        .unwrap();
        let second = run(
            &dirs,
            &req("save", Some("twice"), Some(v2), Some("use math"), "global"),
            "s2",
        )
        .await
        .unwrap();
        let refused = run(
            &dirs,
            &req("save", Some("twice"), Some(bad), None, "global"),
            "s3",
        )
        .await
        .unwrap_err()
        .to_string();
        let history = run(
            &dirs,
            &req("history", Some("twice"), None, None, "global"),
            "s3",
        )
        .await
        .unwrap();
        let old_request = forge_domain::Toolbox {
            revision: Some(1),
            ..req("show", Some("twice"), None, None, "global")
        };
        let old = run(&dirs, &old_request, "s3").await.unwrap();
        let loaded = run(&dirs, &req("load", None, None, None, "global"), "s3")
            .await
            .unwrap();

        assert_eq!(first.0, "saved twice r1 [global]");
        assert_eq!(second.0, "saved twice r2 [global]");
        assert!(refused.contains("factor"), "{refused}");
        assert_eq!(history.0.lines().count(), 2);
        assert!(history.0.contains("session=s2 use math"), "{}", history.0);
        assert!(old.0.contains("return 2 * x"), "{}", old.0);
        assert_eq!(
            (loaded.0.as_str(), loaded.1.as_deref()),
            ("loaded twice r2", Some(v2))
        );
    }

    #[tokio::test]
    async fn test_project_scope_shadows_global_and_delete_keeps_history() {
        let root = tempfile::tempdir().unwrap();
        let dirs = dirs(root.path());
        run(
            &dirs,
            &req(
                "save",
                Some("f"),
                Some("def f():\n    return 'g'\n"),
                None,
                "global",
            ),
            "s",
        )
        .await
        .unwrap();
        run(
            &dirs,
            &req(
                "save",
                Some("f"),
                Some("def f():\n    return 'p'\n"),
                None,
                "project",
            ),
            "s",
        )
        .await
        .unwrap();

        let shown = run(&dirs, &req("show", Some("f"), None, None, "global"), "s")
            .await
            .unwrap();
        run(&dirs, &req("delete", Some("f"), None, None, "global"), "s")
            .await
            .unwrap();
        let listed = run(&dirs, &req("list", None, None, None, "global"), "s")
            .await
            .unwrap();

        assert!(shown.0.contains("'p'"), "{}", shown.0);
        assert_eq!(listed.0, "f r1 [global] ");
    }
}
