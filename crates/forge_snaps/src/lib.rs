//! Persistent, bounded undo store.
//!
//! One store per conversation, under `<base>/snapshots/<conversation_id>/`.
//! Each mutated path gets a directory named by the SHA-256 of the path, holding
//! `path.json` and one `<ns-timestamp>-<seq>.snapshot` JSON file per entry.
//! The store keeps at most [`MAX_ENTRIES`] entries and [`MAX_BYTES`] on disk,
//! pruning oldest first. An entry whose raw size exceeds [`MAX_BYTES`] is kept
//! in memory only and is lost when the process exits.

mod store;

pub use store::*;
