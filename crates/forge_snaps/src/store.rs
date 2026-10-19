use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use forge_domain::{SnapshotCoverage, SnapshotData, TreeEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Most entries the store keeps on disk.
pub const MAX_ENTRIES: usize = 100;
/// Most bytes the store keeps on disk; also the per-entry raw-size limit
/// above which an entry is held in memory only.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
const FORMAT_VERSION: u32 = 1;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Captures the current state of `path` without following a final symlink.
///
/// # Arguments
/// * `path` - Absolute path to capture.
/// * `coverage` - Read coverage to attach to a regular-file capture.
pub fn capture(path: &Path, coverage: Option<SnapshotCoverage>) -> SnapshotData {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SnapshotData::Missing,
        Err(error) => {
            return SnapshotData::Unavailable {
                reason: format!(
                    "could not capture undo snapshot for {}: {error}",
                    path.display()
                ),
            };
        }
    };
    let result = if meta.file_type().is_symlink() {
        fs::read_link(path)
            .map(|target| SnapshotData::Symlink { target: target.to_string_lossy().into_owned() })
    } else if meta.is_dir() {
        capture_tree(path).map(|entries| SnapshotData::Directory { mode: mode_of(&meta), entries })
    } else {
        fs::read(path).map(|data| SnapshotData::File { data, mode: mode_of(&meta), coverage })
    };
    result.unwrap_or_else(|error| SnapshotData::Unavailable {
        reason: format!(
            "could not capture undo snapshot for {}: {error}",
            path.display()
        ),
    })
}

fn capture_tree(root: &Path) -> io::Result<Vec<(String, TreeEntry)>> {
    let mut entries = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let mut children: Vec<_> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = format!("{prefix}{}", child.file_name().to_string_lossy());
            let meta = fs::symlink_metadata(child.path())?;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(child.path())?;
                entries.push((
                    name,
                    TreeEntry::Symlink { target: target.to_string_lossy().into_owned() },
                ));
            } else if meta.is_dir() {
                entries.push((name.clone(), TreeEntry::Dir { mode: mode_of(&meta) }));
                stack.push((child.path(), format!("{name}/")));
            } else {
                entries.push((
                    name,
                    TreeEntry::File { data: fs::read(child.path())?, mode: mode_of(&meta) },
                ));
            }
        }
    }
    Ok(entries)
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, _link: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "symlinks are not supported on this platform",
    ))
}

/// Removes whatever is at `path` (file, symlink or tree) without following
/// symlinks. A missing path is not an error.
pub fn remove_no_follow(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Puts `path` back into the state `data` describes.
///
/// # Errors
/// Returns an IO error when the filesystem refuses the restore; the caller
/// keeps the snapshot so the undo can be retried.
pub fn restore(path: &Path, data: &SnapshotData) -> io::Result<()> {
    match data {
        SnapshotData::Missing => remove_no_follow(path),
        SnapshotData::File { data, mode, .. } => {
            if fs::symlink_metadata(path)
                .is_ok_and(|meta| meta.is_dir() || meta.file_type().is_symlink())
            {
                remove_no_follow(path)?;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, data)?;
            set_mode(path, *mode)
        }
        SnapshotData::Symlink { target } => {
            remove_no_follow(path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            make_symlink(target, path)
        }
        SnapshotData::Directory { mode, entries } => {
            remove_no_follow(path)?;
            fs::create_dir_all(path)?;
            let mut dir_modes = Vec::new();
            for (name, entry) in entries {
                let child = path.join(name);
                match entry {
                    TreeEntry::Dir { mode } => {
                        fs::create_dir_all(&child)?;
                        dir_modes.push((child, *mode));
                    }
                    TreeEntry::File { data, mode } => {
                        if let Some(parent) = child.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&child, data)?;
                        set_mode(&child, *mode)?;
                    }
                    TreeEntry::Symlink { target } => {
                        if let Some(parent) = child.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        make_symlink(target, &child)?;
                    }
                }
            }
            // Deepest directories first so a read-only parent is set last.
            dir_modes.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));
            for (dir, mode) in dir_modes {
                set_mode(&dir, mode)?;
            }
            set_mode(path, *mode)
        }
        SnapshotData::Unavailable { reason } | SnapshotData::Corrupt { reason } => {
            Err(io::Error::other(reason.clone()))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    path: String,
    snapshot: SnapshotData,
}

#[derive(Serialize, Deserialize)]
struct PathMeta {
    version: u32,
    path: String,
}

#[derive(Debug, Clone)]
struct Entry {
    data: SnapshotData,
    /// Backing file; `None` for memory-only entries.
    file: Option<PathBuf>,
}

/// Result of recording a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordOutcome {
    /// Warnings the tool result should carry (memory-only entry, failed
    /// persistence, failed capture).
    pub notices: Vec<String>,
}

/// A conversation's undo stacks, persisted under one directory.
#[derive(Debug)]
pub struct SnapshotStore {
    root: Option<PathBuf>,
    stacks: Mutex<HashMap<PathBuf, Vec<Entry>>>,
}

impl SnapshotStore {
    /// Opens (and loads) the store rooted at `root`. A missing directory is
    /// an empty store.
    pub fn open(root: PathBuf) -> Self {
        let store = Self { root: Some(root), stacks: Mutex::new(HashMap::new()) };
        store.load();
        store
    }

    /// A store that keeps everything in memory (no persistence).
    pub fn in_memory() -> Self {
        Self { root: None, stacks: Mutex::new(HashMap::new()) }
    }

    fn path_dir(&self, root: &Path, path: &Path) -> PathBuf {
        let digest = Sha256::digest(path.to_string_lossy().as_bytes());
        root.join(hex::encode(digest))
    }

    fn load(&self) {
        let Some(root) = &self.root else { return };
        let Ok(dirs) = fs::read_dir(root) else { return };
        let mut stacks = self.lock();
        for dir in dirs.flatten() {
            let dir_path = dir.path();
            if !dir_path.is_dir() {
                continue;
            }
            let Ok(meta) = fs::read(dir_path.join("path.json"))
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<PathMeta>(&bytes)?))
            else {
                continue;
            };
            let target = PathBuf::from(&meta.path);
            if !target.is_absolute() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(&dir_path)
                .map(|it| {
                    it.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|ext| ext == "snapshot"))
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            let stack = stacks.entry(target.clone()).or_default();
            for file in files {
                let data = fs::read(&file)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(serde_json::from_slice::<Envelope>(&bytes)?))
                    .and_then(|envelope| {
                        if envelope.version != FORMAT_VERSION {
                            anyhow::bail!("unsupported snapshot format");
                        }
                        if envelope.path != meta.path {
                            anyhow::bail!("snapshot path does not match its store");
                        }
                        Ok(envelope.snapshot)
                    })
                    .unwrap_or_else(|error| SnapshotData::Corrupt { reason: error.to_string() });
                stack.push(Entry { data, file: Some(file) });
            }
        }
        drop(stacks);
        self.prune();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Vec<Entry>>> {
        self.stacks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records `data` as the newest state of `path`.
    pub fn record(&self, path: &Path, data: SnapshotData) -> RecordOutcome {
        let mut notices = Vec::new();
        if let SnapshotData::Unavailable { reason } = &data {
            notices.push(reason.clone());
        }
        let file = match &self.root {
            Some(root) if data.raw_size() > MAX_BYTES => {
                notices.push(format!(
                    "undo snapshot of {} exceeds the {} MiB session undo cap; this undo is available only until the process exits",
                    path.display(),
                    MAX_BYTES / (1024 * 1024)
                ));
                let _ = root;
                None
            }
            Some(root) => match self.persist(root, path, &data) {
                Ok(file) => Some(file),
                Err(error) => {
                    notices.push(format!(
                        "could not persist undo snapshot for {}: {error}; this undo is available only until the process exits",
                        path.display()
                    ));
                    None
                }
            },
            None => None,
        };
        self.lock()
            .entry(path.to_path_buf())
            .or_default()
            .push(Entry { data, file });
        self.prune();
        RecordOutcome { notices }
    }

    fn persist(&self, root: &Path, path: &Path, data: &SnapshotData) -> anyhow::Result<PathBuf> {
        let dir = self.path_dir(root, path);
        fs::create_dir_all(&dir)?;
        let meta = serde_json::to_vec(&PathMeta {
            version: FORMAT_VERSION,
            path: path.to_string_lossy().into_owned(),
        })?;
        atomic_write(&dir.join("path.json"), &meta)?;
        let envelope = serde_json::to_vec(&Envelope {
            version: FORMAT_VERSION,
            path: path.to_string_lossy().into_owned(),
            snapshot: data.clone(),
        })?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file = dir.join(format!("{stamp:020}-{sequence:08}.snapshot"));
        atomic_write(&file, &envelope)?;
        Ok(file)
    }

    /// Drops the oldest persisted entries until the store is within
    /// [`MAX_ENTRIES`] and [`MAX_BYTES`].
    fn prune(&self) {
        if self.root.is_none() {
            return;
        }
        let mut stacks = self.lock();
        let mut persisted: Vec<(String, u64, PathBuf)> = stacks
            .values()
            .flatten()
            .filter_map(|entry| entry.file.clone())
            .map(|file| {
                let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (name, size, file)
            })
            .collect();
        persisted.sort();
        let mut total: u64 = persisted.iter().map(|(_, size, _)| size).sum();
        let mut count = persisted.len();
        let mut victims = Vec::new();
        for (_, size, file) in persisted {
            if count <= MAX_ENTRIES && total <= MAX_BYTES {
                break;
            }
            victims.push(file);
            count -= 1;
            total = total.saturating_sub(size);
        }
        if victims.is_empty() {
            return;
        }
        for stack in stacks.values_mut() {
            stack.retain(|entry| {
                entry
                    .file
                    .as_ref()
                    .is_none_or(|file| !victims.contains(file))
            });
        }
        stacks.retain(|_, stack| !stack.is_empty());
        drop(stacks);
        for victim in victims {
            let _ = fs::remove_file(&victim);
            if let Some(dir) = victim.parent() {
                cleanup_dir(dir);
            }
        }
    }

    /// Whether `path` has any snapshot.
    pub fn has(&self, path: &Path) -> bool {
        self.lock().get(path).is_some_and(|stack| !stack.is_empty())
    }

    /// The newest snapshot of `path`, without removing it.
    pub fn peek(&self, path: &Path) -> Option<SnapshotData> {
        self.lock()
            .get(path)
            .and_then(|stack| stack.last())
            .map(|entry| entry.data.clone())
    }

    /// Removes the newest snapshot of `path` (after a successful restore, or
    /// to discard an unusable entry).
    pub fn pop(&self, path: &Path) -> Option<SnapshotData> {
        let mut stacks = self.lock();
        let stack = stacks.get_mut(path)?;
        let entry = stack.pop()?;
        if stack.is_empty() {
            stacks.remove(path);
        }
        drop(stacks);
        if let Some(file) = &entry.file {
            let _ = fs::remove_file(file);
            if let Some(dir) = file.parent() {
                cleanup_dir(dir);
            }
        }
        Some(entry.data)
    }

    /// Number of entries held (memory and disk).
    pub fn len(&self) -> usize {
        self.lock().values().map(Vec::len).sum()
    }

    /// Whether the store holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn cleanup_dir(dir: &Path) {
    let has_entries = fs::read_dir(dir).is_ok_and(|mut it| {
        it.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|ext| ext == "snapshot")))
    });
    if !has_entries {
        let _ = fs::remove_file(dir.join("path.json"));
        let _ = fs::remove_dir(dir);
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    fs::write(&temporary, data)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        (dir, store, work)
    }

    #[test]
    fn test_file_round_trip_survives_reopen() {
        let (_dir, store_dir, work) = fixture();
        let file = work.join("a.txt");
        fs::write(&file, "one").unwrap();
        let store = SnapshotStore::open(store_dir.clone());
        store.record(&file, capture(&file, None));
        fs::write(&file, "two").unwrap();

        let reopened = SnapshotStore::open(store_dir);
        let data = reopened.peek(&file).unwrap();
        restore(&file, &data).unwrap();
        reopened.pop(&file);

        let actual = (fs::read_to_string(&file).unwrap(), reopened.has(&file));
        let expected = ("one".to_string(), false);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_missing_snapshot_undo_deletes_created_file() {
        let (_dir, store_dir, work) = fixture();
        let file = work.join("new.txt");
        let store = SnapshotStore::open(store_dir);
        store.record(&file, capture(&file, None));
        fs::write(&file, "created").unwrap();

        restore(&file, &store.pop(&file).unwrap()).unwrap();

        let actual = file.exists();
        let expected = false;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_directory_tree_with_symlink_and_modes_restored() {
        let (_dir, store_dir, work) = fixture();
        let tree = work.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/f.txt"), "deep").unwrap();
        set_mode(&tree.join("sub/f.txt"), 0o640).unwrap();
        make_symlink("sub/f.txt", &tree.join("link")).unwrap();
        let store = SnapshotStore::open(store_dir);
        store.record(&tree, capture(&tree, None));
        fs::remove_dir_all(&tree).unwrap();

        restore(&tree, &store.pop(&tree).unwrap()).unwrap();

        let meta = fs::metadata(tree.join("sub/f.txt")).unwrap();
        let actual = (
            fs::read_to_string(tree.join("sub/f.txt")).unwrap(),
            fs::read_link(tree.join("link")).unwrap(),
            mode_of(&meta),
        );
        let expected = ("deep".to_string(), PathBuf::from("sub/f.txt"), 0o640);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_symlink_captured_without_following() {
        let (_dir, _store_dir, work) = fixture();
        let link = work.join("l");
        make_symlink("/nonexistent/target", &link).unwrap();
        let actual = capture(&link, None);
        let expected = SnapshotData::Symlink { target: "/nonexistent/target".to_string() };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_prune_keeps_newest_hundred() {
        let (_dir, store_dir, work) = fixture();
        let store = SnapshotStore::open(store_dir.clone());
        for i in 0..105 {
            let file = work.join(format!("f{i}.txt"));
            store.record(&file, SnapshotData::Missing);
        }
        let reopened = SnapshotStore::open(store_dir);
        let actual = (
            store.len(),
            reopened.len(),
            reopened.has(&work.join("f0.txt")),
            reopened.has(&work.join("f104.txt")),
        );
        let expected = (MAX_ENTRIES, MAX_ENTRIES, false, true);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_corrupt_entry_loads_as_corrupt() {
        let (_dir, store_dir, work) = fixture();
        let file = work.join("c.txt");
        let store = SnapshotStore::open(store_dir.clone());
        store.record(&file, SnapshotData::Missing);
        let entry_dir = fs::read_dir(&store_dir)
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let snap = fs::read_dir(&entry_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "snapshot"))
            .unwrap();
        fs::write(&snap, "not json").unwrap();

        let reopened = SnapshotStore::open(store_dir);
        let actual = matches!(reopened.peek(&file), Some(SnapshotData::Corrupt { .. }));
        let expected = true;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_in_memory_store_keeps_stack_order() {
        let store = SnapshotStore::in_memory();
        let path = PathBuf::from("/x");
        store.record(&path, SnapshotData::Missing);
        store.record(&path, SnapshotData::Symlink { target: "t".to_string() });
        let actual = (store.pop(&path), store.pop(&path), store.pop(&path));
        let expected = (
            Some(SnapshotData::Symlink { target: "t".to_string() }),
            Some(SnapshotData::Missing),
            None,
        );
        assert_eq!(actual, expected);
    }
}
