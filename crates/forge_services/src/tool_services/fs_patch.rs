use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use forge_app::{FileWriterInfra, FsPatchService, PatchOutput, compute_hash};
use forge_domain::{PatchEdit, SnapshotRepository};
use tokio::fs;

use crate::tool_services::patch_engine::{EditRequest, apply_edits};
use crate::tool_services::syn::introduced_errors;
use crate::utils::assert_absolute_path;

/// Service for patching files with snapshot coordination.
///
/// Matching is local: exact first, then a normalised fallback (smart quotes,
/// Unicode dashes, no-break spaces, trailing whitespace). A batch is applied in
/// memory edit by edit and written once, so a failing edit writes nothing and
/// the whole call is one undo step. After writing, the file is parsed locally
/// and only syntax errors the edit introduced are reported.
pub struct ForgeFsPatch<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsPatch<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

#[async_trait::async_trait]
impl<F: FileWriterInfra + SnapshotRepository> FsPatchService for ForgeFsPatch<F> {
    async fn patch(
        &self,
        input_path: String,
        edits: Vec<PatchEdit>,
        batch: bool,
    ) -> anyhow::Result<PatchOutput> {
        let path = Path::new(&input_path);
        assert_absolute_path(path)?;

        if !path.exists() {
            anyhow::bail!("no such file: {}", path.display());
        }
        if !path.is_file() {
            anyhow::bail!("not a regular file: {}", path.display());
        }
        let before = fs::read_to_string(path).await?;

        let requests: Vec<EditRequest<'_>> = edits
            .iter()
            .map(|edit| EditRequest {
                old_string: &edit.old_string,
                new_string: &edit.new_string,
                replace_all: edit.replace_all,
            })
            .collect();
        let outcome = apply_edits(&before, &requests, batch).map_err(anyhow::Error::from)?;

        // SNAPSHOT COORDINATION: Always capture snapshot before modifying
        self.infra.insert_snapshot(path).await?;
        self.infra
            .write(path, Bytes::from(outcome.updated.clone()))
            .await?;

        let content_hash = compute_hash(&outcome.updated);
        let errors = introduced_errors(path, Some(&before), &outcome.updated);

        Ok(PatchOutput {
            errors,
            before,
            after: outcome.updated,
            content_hash,
            edits: edits.len(),
            replacements: outcome.replacements,
            fuzzy_edits: outcome.fuzzy_edits,
            batch,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use forge_domain::Snapshot;
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    struct Fixture {
        snapshots: Mutex<Vec<PathBuf>>,
    }

    #[async_trait::async_trait]
    impl FileWriterInfra for Fixture {
        async fn write(&self, path: &Path, contents: Bytes) -> anyhow::Result<()> {
            Ok(tokio::fs::write(path, contents).await?)
        }

        async fn append(&self, _path: &Path, _contents: Bytes) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn write_temp(
            &self,
            _prefix: &str,
            _ext: &str,
            _content: &str,
        ) -> anyhow::Result<PathBuf> {
            unimplemented!()
        }
    }

    #[async_trait::async_trait]
    impl SnapshotRepository for Fixture {
        async fn insert_snapshot(&self, file_path: &Path) -> anyhow::Result<Snapshot> {
            self.snapshots.lock().unwrap().push(file_path.to_path_buf());
            Snapshot::create(file_path.to_path_buf())
        }

        async fn undo_snapshot(&self, _file_path: &Path) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_batch_patch_writes_once_and_snapshots_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        tokio::fs::write(&file, "fn a() {}\nfn b() {}\n").await.unwrap();
        let infra = Arc::new(Fixture::default());
        let service = ForgeFsPatch::new(infra.clone());

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("fn a()", "fn x()"), PatchEdit::new("fn b()", "fn y()")],
                true,
            )
            .await
            .unwrap();

        let expected = "fn x() {}\nfn y() {}\n";
        assert_eq!(actual.after, expected);
        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), expected);
        assert_eq!(infra.snapshots.lock().unwrap().len(), 1);
        assert_eq!(actual.errors, vec![]);
    }

    #[tokio::test]
    async fn test_failing_edit_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "one\ntwo\n").await.unwrap();
        let infra = Arc::new(Fixture::default());
        let service = ForgeFsPatch::new(infra.clone());

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("one", "1"), PatchEdit::new("three", "3")],
                true,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(actual.starts_with("edit 2: Could not find match"), "{actual}");
        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), "one\ntwo\n");
        assert_eq!(infra.snapshots.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_patch_reports_only_introduced_syntax_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        tokio::fs::write(&file, "def f():\n    return 1\n").await.unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("def f():", "def f(:")],
                false,
            )
            .await
            .unwrap();

        assert_eq!(actual.errors.len(), 1);
    }
}
