use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use forge_domain::{
    ConversationId, Environment, SnapshotCoverage, SnapshotData, SnapshotRepository, UndoOutcome,
};
use forge_snaps::{SnapshotStore, capture, restore};

/// Per-conversation persistent undo stores under `<base>/snapshots/<id>/`.
pub struct ForgeFileSnapshotService {
    root: PathBuf,
    stores: Mutex<HashMap<String, Arc<SnapshotStore>>>,
}

impl ForgeFileSnapshotService {
    pub fn new(env: Environment) -> Self {
        Self {
            root: env.snapshot_path(),
            stores: Mutex::new(HashMap::new()),
        }
    }

    fn store(&self, conversation: &ConversationId) -> Arc<SnapshotStore> {
        let id = conversation.to_string();
        let mut stores = self
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stores
            .entry(id.clone())
            .or_insert_with(|| Arc::new(SnapshotStore::open(self.root.join(id))))
            .clone()
    }
}

#[async_trait::async_trait]
impl SnapshotRepository for ForgeFileSnapshotService {
    async fn insert_snapshot(
        &self,
        conversation: &ConversationId,
        path: &Path,
        coverage: Option<SnapshotCoverage>,
    ) -> Result<Vec<String>> {
        let store = self.store(conversation);
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || store.record(&path, capture(&path, coverage)).notices)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn discard_snapshot(&self, conversation: &ConversationId, path: &Path) -> Result<()> {
        self.store(conversation).pop(path);
        Ok(())
    }

    async fn undo_snapshot(
        &self,
        conversation: &ConversationId,
        path: &Path,
    ) -> Result<UndoOutcome> {
        let store = self.store(conversation);
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<UndoOutcome> {
            let Some(data) = store.peek(&path) else {
                return Ok(UndoOutcome::NoSnapshot);
            };
            if let SnapshotData::Unavailable { reason } | SnapshotData::Corrupt { reason } = &data {
                let reason = reason.clone();
                store.pop(&path);
                return Ok(UndoOutcome::Discarded { reason });
            }
            restore(&path, &data)?;
            store.pop(&path);
            Ok(match data {
                SnapshotData::Missing => UndoOutcome::RestoredDeletion,
                SnapshotData::File { data, coverage, .. } => {
                    UndoOutcome::RestoredFile { data, coverage }
                }
                SnapshotData::Symlink { target } => UndoOutcome::RestoredSymlink { target },
                SnapshotData::Directory { .. } => UndoOutcome::RestoredDirectory,
                SnapshotData::Unavailable { reason } | SnapshotData::Corrupt { reason } => {
                    UndoOutcome::Discarded { reason }
                }
            })
        })
        .await?
    }

    async fn has_snapshot(&self, conversation: &ConversationId, path: &Path) -> Result<bool> {
        Ok(self.store(conversation).has(path))
    }
}
