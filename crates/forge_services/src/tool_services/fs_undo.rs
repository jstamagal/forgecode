use std::path::Path;
use std::sync::Arc;

use bstr::ByteSlice;
use forge_app::{FsUndoOutput, FsUndoService, UndoKind};
use forge_domain::{SnapshotRepository, ToolCallContext, UndoOutcome};

use crate::tool_services::provenance::{hash_bytes, key, line_count};
use crate::utils::assert_absolute_path;

/// Restores the newest snapshot of a path from the conversation's undo store.
///
/// File, directory (with modes), symlink and "did not exist" snapshots are
/// restored; an unusable snapshot is discarded with an error so a retry
/// reaches an older entry. Read coverage is restored only when the restored
/// bytes hash to the coverage recorded in the snapshot.
#[derive(Default)]
pub struct ForgeFsUndo<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsUndo<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

fn utf8(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .and_then(|data| String::from_utf8(data).ok())
}

#[async_trait::async_trait]
impl<F: SnapshotRepository> FsUndoService for ForgeFsUndo<F> {
    async fn undo(&self, path: String, context: &ToolCallContext) -> anyhow::Result<FsUndoOutput> {
        let path = Path::new(&path);
        assert_absolute_path(path)?;
        let before_undo = utf8(path);
        let outcome = self
            .infra
            .undo_snapshot(context.conversation_id(), path)
            .await?;
        let ledger = context.reads()?;
        let kind = match outcome {
            UndoOutcome::NoSnapshot => {
                anyhow::bail!("no snapshot available for {}", path.display())
            }
            UndoOutcome::Discarded { reason } => anyhow::bail!(
                "discarded unusable snapshot for {}: {reason}; retry undo for an older entry",
                path.display()
            ),
            UndoOutcome::RestoredFile { data, coverage } => {
                let hash = hash_bytes(&data);
                let total = line_count(&data.to_str_lossy());
                let valid = coverage.filter(|c| c.hash == hash);
                ledger.with(|l| {
                    match &valid {
                        Some(c) => l.replace(&key(path), &hash, c.ranges.clone(), total, c.whole),
                        None => l.replace(&key(path), &hash, vec![], total, false),
                    }
                    l.remember_content(&key(path), &hash, data.clone());
                });
                UndoKind::File
            }
            UndoOutcome::RestoredDirectory => UndoKind::Directory,
            UndoOutcome::RestoredSymlink { target } => UndoKind::Symlink { target },
            UndoOutcome::RestoredDeletion => UndoKind::Deletion,
        };
        let after_undo = match kind {
            UndoKind::File => utf8(path),
            _ => None,
        };
        Ok(FsUndoOutput { before_undo, after_undo, kind })
    }
}

#[cfg(test)]
mod tests {
    use forge_domain::{ConversationId, Metrics, SnapshotCoverage, SnapshotData};
    use pretty_assertions::assert_eq;

    use super::*;

    struct StoreInfra(forge_snaps::SnapshotStore);

    #[async_trait::async_trait]
    impl SnapshotRepository for StoreInfra {
        async fn insert_snapshot(
            &self,
            _conversation: &ConversationId,
            path: &Path,
            coverage: Option<SnapshotCoverage>,
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
        ) -> anyhow::Result<UndoOutcome> {
            let Some(data) = self.0.peek(path) else {
                return Ok(UndoOutcome::NoSnapshot);
            };
            if let SnapshotData::Corrupt { reason } | SnapshotData::Unavailable { reason } = &data {
                let reason = reason.clone();
                self.0.pop(path);
                return Ok(UndoOutcome::Discarded { reason });
            }
            forge_snaps::restore(path, &data)?;
            self.0.pop(path);
            Ok(match data {
                SnapshotData::Missing => UndoOutcome::RestoredDeletion,
                SnapshotData::File { data, coverage, .. } => {
                    UndoOutcome::RestoredFile { data, coverage }
                }
                SnapshotData::Symlink { target } => UndoOutcome::RestoredSymlink { target },
                _ => UndoOutcome::RestoredDirectory,
            })
        }
        async fn has_snapshot(&self, _c: &ConversationId, path: &Path) -> anyhow::Result<bool> {
            Ok(self.0.has(path))
        }
    }

    fn fixture() -> (ForgeFsUndo<StoreInfra>, Arc<StoreInfra>, ToolCallContext) {
        let infra = Arc::new(StoreInfra(forge_snaps::SnapshotStore::in_memory()));
        (
            ForgeFsUndo::new(infra.clone()),
            infra,
            ToolCallContext::new(Metrics::default()),
        )
    }

    #[tokio::test]
    async fn test_undo_of_create_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let (service, infra, context) = fixture();
        infra
            .insert_snapshot(context.conversation_id(), &file, None)
            .await
            .unwrap();
        std::fs::write(&file, "created").unwrap();

        let output = service
            .undo(file.display().to_string(), &context)
            .await
            .unwrap();

        let actual = (output.kind, file.exists());
        let expected = (UndoKind::Deletion, false);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_undo_restores_bytes_and_matching_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let (service, infra, context) = fixture();
        let hash = hash_bytes(b"one\ntwo\n");
        let coverage = SnapshotCoverage { hash: hash.clone(), ranges: vec![(1, 1)], whole: false };
        infra
            .insert_snapshot(context.conversation_id(), &file, Some(coverage))
            .await
            .unwrap();
        std::fs::write(&file, "changed\n").unwrap();

        service
            .undo(file.display().to_string(), &context)
            .await
            .unwrap();

        let ranges = context
            .reads()
            .unwrap()
            .with(|l| l.coverage(&key(&file)).unwrap().ranges.clone());
        let actual = (std::fs::read_to_string(&file).unwrap(), ranges);
        let expected = ("one\ntwo\n".to_string(), vec![(1, 1)]);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_undo_without_snapshot_errors() {
        let (service, _infra, context) = fixture();
        let actual = service
            .undo("/tmp/forge-no-snapshot".to_string(), &context)
            .await
            .unwrap_err()
            .to_string();
        let expected = "no snapshot available for /tmp/forge-no-snapshot";
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_corrupt_snapshot_is_discarded_with_retry_message() {
        let (service, infra, context) = fixture();
        let path = Path::new("/tmp/forge-corrupt-target");
        infra.0.record(path, SnapshotData::Missing);
        infra.0.record(
            path,
            SnapshotData::Corrupt { reason: "bad json".to_string() },
        );

        let actual = service
            .undo(path.display().to_string(), &context)
            .await
            .unwrap_err()
            .to_string();

        let expected = "discarded unusable snapshot for /tmp/forge-corrupt-target: bad json; retry undo for an older entry";
        assert_eq!(actual, expected);
        assert!(infra.0.has(path));
    }
}
