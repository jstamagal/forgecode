//! Undo snapshot data model shared by the file tools and the snapshot store.

use serde::{Deserialize, Serialize};

/// Read coverage carried by a file snapshot. It is restored by an undo only
/// when the restored bytes hash to `hash`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotCoverage {
    /// Content hash the coverage describes.
    pub hash: String,
    /// Line spans the model had seen.
    #[serde(default)]
    pub ranges: Vec<(u64, u64)>,
    /// Whether every line had been seen.
    #[serde(default)]
    pub whole: bool,
}

/// One node of a directory snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TreeEntry {
    /// A regular file with its bytes and permission mode.
    File {
        #[serde(with = "b64")]
        data: Vec<u8>,
        mode: u32,
    },
    /// A directory and its permission mode.
    Dir { mode: u32 },
    /// A symlink and its target.
    Symlink { target: String },
}

/// The state a path had before a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotData {
    /// The path did not exist; undo deletes whatever is there.
    Missing,
    /// A regular file.
    File {
        #[serde(with = "b64")]
        data: Vec<u8>,
        mode: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        coverage: Option<SnapshotCoverage>,
    },
    /// A symlink, captured without following it.
    Symlink { target: String },
    /// A directory tree; names are `/`-separated paths relative to the root,
    /// parents listed before children.
    Directory {
        mode: u32,
        entries: Vec<(String, TreeEntry)>,
    },
    /// The capture failed or the entry could not be kept; undo discards it.
    Unavailable { reason: String },
    /// The stored entry could not be decoded; undo discards it.
    Corrupt { reason: String },
}

impl SnapshotData {
    /// Raw payload size, used to decide whether the entry fits on disk.
    pub fn raw_size(&self) -> u64 {
        match self {
            SnapshotData::File { data, .. } => data.len() as u64,
            SnapshotData::Directory { entries, .. } => entries
                .iter()
                .map(|(name, entry)| {
                    name.len() as u64
                        + match entry {
                            TreeEntry::File { data, .. } => data.len() as u64,
                            TreeEntry::Symlink { target } => target.len() as u64,
                            TreeEntry::Dir { .. } => 0,
                        }
                })
                .sum(),
            SnapshotData::Symlink { target } => target.len() as u64,
            _ => 0,
        }
    }
}

/// What an undo did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// No snapshot exists for the path.
    NoSnapshot,
    /// The newest snapshot was unusable and was discarded; an older one may
    /// remain.
    Discarded { reason: String },
    /// A regular file was restored with these bytes.
    RestoredFile {
        data: Vec<u8>,
        coverage: Option<SnapshotCoverage>,
    },
    /// A directory tree was restored.
    RestoredDirectory,
    /// A symlink was restored.
    RestoredSymlink { target: String },
    /// The path did not exist before; it was deleted.
    RestoredDeletion,
}

mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::Deserialize;

    pub fn serialize<S: serde::Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_snapshot_data_round_trips_bytes_as_base64() {
        let fixture = SnapshotData::File { data: vec![0, 159, 255], mode: 0o644, coverage: None };
        let json = serde_json::to_string(&fixture).unwrap();
        let actual: SnapshotData = serde_json::from_str(&json).unwrap();
        assert_eq!(actual, fixture);
    }
}
