use std::path::{Path, PathBuf};
use std::sync::Arc;

use bstr::ByteSlice;
use forge_app::{FsRemoveOutput, FsRemoveService, RemoveMode};
use forge_domain::{SnapshotRepository, ToolCallContext};

use crate::utils::assert_absolute_path;

/// Largest target, in bytes, moved to the trash; larger needs `permanent`.
const TRASH_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Removes files, directories and symlinks (never following a symlink).
///
/// By default the target goes to the trash through `trash` or `trash-put`
/// from PATH; when neither exists or the command fails the call errors and
/// nothing is deleted. `permanent` deletes directly. Either way the target is
/// snapshotted first so `undo` can bring it back.
pub struct ForgeFsRemove<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsRemove<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

/// Size of `path` without following symlinks; stops counting past `cap`.
fn size_no_follow(path: &Path, cap: u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return cap;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = meta.len();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            total += meta.len();
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(entry.path());
            }
            if total > cap {
                return total;
            }
        }
    }
    total
}

/// Finds `trash` or `trash-put` on PATH.
fn trash_command() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    ["trash", "trash-put"].iter().find_map(|name| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[async_trait::async_trait]
impl<F: SnapshotRepository> FsRemoveService for ForgeFsRemove<F> {
    async fn remove(
        &self,
        input_path: String,
        permanent: bool,
        context: &ToolCallContext,
    ) -> anyhow::Result<FsRemoveOutput> {
        let path = Path::new(&input_path);
        assert_absolute_path(path)?;
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => anyhow::bail!("no such path: {}", path.display()),
        };
        let size = size_no_follow(path, TRASH_MAX_BYTES + 1);
        if !permanent && size > TRASH_MAX_BYTES {
            anyhow::bail!(
                "target is over the 512 MiB trash limit ({size} bytes); confirm with the operator and pass permanent=true to delete directly."
            );
        }
        let content = if meta.is_file() {
            std::fs::read(path)
                .ok()
                .and_then(|data| String::from_utf8(data).ok())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let notices = self
            .infra
            .insert_snapshot(context.conversation_id(), path, None)
            .await?;

        let mode = if permanent {
            forge_snaps::remove_no_follow(path)?;
            RemoveMode::Deleted
        } else {
            let Some(command) = trash_command() else {
                self.infra
                    .discard_snapshot(context.conversation_id(), path)
                    .await?;
                anyhow::bail!(
                    "trash command not found (install trash-cli for `trash`/`trash-put`); pass permanent=true to delete without trash."
                );
            };
            let output = tokio::process::Command::new(&command)
                .arg(path)
                .stdin(std::process::Stdio::null())
                .output()
                .await;
            let failure = match output {
                Ok(output) if output.status.success() => None,
                Ok(output) => {
                    let stderr: String = output
                        .stderr
                        .to_str_lossy()
                        .trim()
                        .chars()
                        .take(500)
                        .collect();
                    Some(if stderr.is_empty() {
                        format!("exit {}", output.status)
                    } else {
                        stderr
                    })
                }
                Err(error) => Some(error.to_string()),
            };
            if let Some(reason) = failure {
                self.infra
                    .discard_snapshot(context.conversation_id(), path)
                    .await?;
                anyhow::bail!(
                    "trash failed: {reason}; pass permanent=true to delete without trash."
                );
            }
            RemoveMode::Trashed
        };

        Ok(FsRemoveOutput { content, mode, notices })
    }
}

#[cfg(test)]
mod tests {
    use forge_domain::{ConversationId, Metrics};
    use pretty_assertions::assert_eq;

    use super::*;

    struct StoreInfra(forge_snaps::SnapshotStore);

    #[async_trait::async_trait]
    impl SnapshotRepository for StoreInfra {
        async fn insert_snapshot(
            &self,
            _conversation: &ConversationId,
            path: &Path,
            coverage: Option<forge_domain::SnapshotCoverage>,
        ) -> anyhow::Result<Vec<String>> {
            Ok(self
                .0
                .record(path, forge_snaps::capture(path, coverage))
                .notices)
        }
        async fn discard_snapshot(&self, _c: &ConversationId, path: &Path) -> anyhow::Result<()> {
            self.0.pop(path);
            Ok(())
        }
        async fn undo_snapshot(
            &self,
            _c: &ConversationId,
            path: &Path,
        ) -> anyhow::Result<forge_domain::UndoOutcome> {
            let data = self.0.pop(path).unwrap();
            forge_snaps::restore(path, &data)?;
            Ok(forge_domain::UndoOutcome::RestoredDirectory)
        }
        async fn has_snapshot(&self, _c: &ConversationId, path: &Path) -> anyhow::Result<bool> {
            Ok(self.0.has(path))
        }
    }

    #[tokio::test]
    async fn test_permanent_remove_of_directory_is_undoable() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("x")).unwrap();
        std::fs::write(tree.join("x/f.txt"), "data").unwrap();
        let infra = Arc::new(StoreInfra(forge_snaps::SnapshotStore::in_memory()));
        let service = ForgeFsRemove::new(infra.clone());
        let context = ToolCallContext::new(Metrics::default());

        let output = service
            .remove(tree.display().to_string(), true, &context)
            .await
            .unwrap();
        let gone = !tree.exists();
        infra
            .undo_snapshot(context.conversation_id(), &tree)
            .await
            .unwrap();

        let actual = (
            output.mode,
            gone,
            std::fs::read_to_string(tree.join("x/f.txt")).unwrap(),
        );
        let expected = (RemoveMode::Deleted, true, "data".to_string());
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_symlink_removed_as_link_not_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let infra = Arc::new(StoreInfra(forge_snaps::SnapshotStore::in_memory()));
        let service = ForgeFsRemove::new(infra);
        let context = ToolCallContext::new(Metrics::default());

        service
            .remove(link.display().to_string(), true, &context)
            .await
            .unwrap();

        let actual = (
            link.symlink_metadata().is_ok(),
            std::fs::read_to_string(&target).unwrap(),
        );
        let expected = (false, "keep".to_string());
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_missing_path_is_an_error() {
        let infra = Arc::new(StoreInfra(forge_snaps::SnapshotStore::in_memory()));
        let service = ForgeFsRemove::new(infra);
        let context = ToolCallContext::new(Metrics::default());

        let actual = service
            .remove("/nonexistent/forge-test-path".to_string(), false, &context)
            .await
            .unwrap_err()
            .to_string();

        let expected = "no such path: /nonexistent/forge-test-path";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_size_no_follow_stops_past_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("b"), vec![0u8; 100]).unwrap();
        let actual = size_no_follow(dir.path(), 50) > 50;
        assert!(actual);
    }
}
