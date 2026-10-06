use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use forge_app::{EnvironmentInfra, FileWriterInfra, FsPatchService, PatchOutput, compute_hash};
use forge_domain::{
    LineSpan, PatchEdit, SnapshotCoverage, SnapshotRepository, ToolCallContext, transform_ranges,
};

use crate::tool_services::fs_write::diff_budget;
use crate::tool_services::patch_engine::{EditRequest, PatchError, apply_edit};
use crate::tool_services::provenance::{
    Staleness, changed_since_read, hash_bytes, key, line_count, require_read,
};
use crate::tool_services::syn::introduced_errors;
use crate::utils::assert_absolute_path;

/// Service for patching files with snapshot coordination.
///
/// Matching is local: exact first, then a normalised fallback (smart quotes,
/// Unicode dashes, no-break spaces, trailing whitespace). Before anything is
/// written the call must pass the read gate: the file was read in this
/// conversation, it has not changed since (a changed file is refused with a
/// diff), and every line any edit touches (every `replace_all` occurrence
/// included) was shown to the model. A batch is applied in memory edit by edit
/// and written once, so a failing edit writes nothing and the whole call is
/// one undo step. After writing, the file is parsed locally and only syntax
/// errors the edit introduced are reported.
pub struct ForgeFsPatch<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsPatch<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

#[async_trait::async_trait]
impl<F: FileWriterInfra + SnapshotRepository + EnvironmentInfra<Config = forge_config::ForgeConfig>>
    FsPatchService for ForgeFsPatch<F>
{
    async fn patch(
        &self,
        input_path: String,
        edits: Vec<PatchEdit>,
        batch: bool,
        context: &ToolCallContext,
    ) -> anyhow::Result<PatchOutput> {
        let path = Path::new(&input_path);
        assert_absolute_path(path)?;
        let config = self.infra.get_config()?;
        let ledger = context.reads()?;

        let label = |index: usize, error: anyhow::Error| -> anyhow::Error {
            if batch {
                anyhow::anyhow!("edit {}: {error}", index + 1)
            } else {
                error
            }
        };
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_string == edit.new_string {
                return Err(label(
                    index,
                    anyhow::anyhow!("old_string and new_string must be different"),
                ));
            }
        }

        if !path.exists() {
            anyhow::bail!("no such file: {}", path.display());
        }
        if !path.is_file() {
            anyhow::bail!("not a regular file: {}", path.display());
        }
        let current = std::fs::read(path)?;
        let current_hash = hash_bytes(&current);
        let known = ledger.with(|l| l.known_content(&key(path)).map(<[u8]>::to_vec));
        if let Staleness::Changed(message) = changed_since_read(
            &ledger,
            path,
            known.as_deref(),
            &current,
            "edit it",
            true,
            diff_budget(&config),
        ) {
            anyhow::bail!(message);
        }
        require_read(&ledger, path, "edit it", &current_hash, &[], None)?;
        let before = String::from_utf8(current)
            .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", path.display()))?;

        let coverage = ledger
            .with(|l| l.coverage(&key(path)).cloned())
            .unwrap_or_default();
        let was_whole = coverage.whole;
        let mut seen: Vec<LineSpan> = coverage.seen_ranges();
        let mut updated = before.clone();
        let mut replacements = 0;
        let mut fuzzy_edits = 0;
        for (index, edit) in edits.iter().enumerate() {
            let request = EditRequest {
                old_string: &edit.old_string,
                new_string: &edit.new_string,
                replace_all: edit.replace_all,
            };
            let outcome = apply_edit(&updated, &request).map_err(|error| {
                let error = if batch {
                    PatchError::Batch { index: index + 1, source: error }
                } else {
                    PatchError::Single(error)
                };
                anyhow::Error::from(error)
            })?;
            let spans: Vec<LineSpan> = outcome
                .line_spans
                .iter()
                .map(|&(start, end)| (start as u64, end as u64))
                .collect();
            require_read(&ledger, path, "edit it", &current_hash, &spans, Some(&seen))
                .map_err(|error| label(index, error))?;
            for &(start, end) in spans.iter().rev() {
                seen = transform_ranges(&seen, start, end, outcome.line_delta as i64);
            }
            replacements += if edit.replace_all { outcome.count } else { 1 };
            fuzzy_edits += usize::from(outcome.fuzzy);
            updated = outcome.updated;
        }

        let snapshot_coverage = SnapshotCoverage {
            hash: coverage.hash.clone(),
            ranges: coverage.ranges.clone(),
            whole: coverage.whole,
        };
        let notices = self
            .infra
            .insert_snapshot(context.conversation_id(), path, Some(snapshot_coverage))
            .await?;
        if let Err(error) = self.infra.write(path, Bytes::from(updated.clone())).await {
            self.infra
                .discard_snapshot(context.conversation_id(), path)
                .await?;
            return Err(error);
        }

        let new_hash = hash_bytes(updated.as_bytes());
        ledger.with(|l| {
            l.replace(&key(path), &new_hash, seen, line_count(&updated), was_whole);
            l.remember_content(&key(path), &new_hash, updated.clone().into_bytes());
        });
        let errors = introduced_errors(path, Some(&before), &updated);

        Ok(PatchOutput {
            errors,
            before,
            content_hash: compute_hash(&updated),
            after: updated,
            edits: edits.len(),
            replacements,
            fuzzy_edits,
            batch,
            notices,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use forge_domain::{ConversationId, Metrics, UndoOutcome};
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    struct Fixture {
        snapshots: Mutex<Vec<PathBuf>>,
    }

    impl EnvironmentInfra for Fixture {
        type Config = forge_config::ForgeConfig;
        fn get_env_var(&self, _key: &str) -> Option<String> {
            None
        }
        fn get_env_vars(&self) -> std::collections::BTreeMap<String, String> {
            Default::default()
        }
        fn get_environment(&self) -> forge_domain::Environment {
            use fake::{Fake, Faker};
            Faker.fake()
        }
        fn get_config(&self) -> anyhow::Result<Self::Config> {
            Ok(forge_config::ForgeConfig {
                max_tool_result_inline_bytes: 51200,
                max_tool_result_bytes: 262144,
                ..Default::default()
            })
        }
        async fn update_environment(
            &self,
            _ops: Vec<forge_domain::ConfigOperation>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
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
        async fn insert_snapshot(
            &self,
            _conversation: &ConversationId,
            path: &Path,
            _coverage: Option<SnapshotCoverage>,
        ) -> anyhow::Result<Vec<String>> {
            self.snapshots.lock().unwrap().push(path.to_path_buf());
            Ok(vec![])
        }
        async fn discard_snapshot(&self, _c: &ConversationId, _path: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn undo_snapshot(
            &self,
            _c: &ConversationId,
            _path: &Path,
        ) -> anyhow::Result<UndoOutcome> {
            unimplemented!()
        }
        async fn has_snapshot(&self, _c: &ConversationId, _path: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    /// Records a read of `file` covering `span` (whole file when `None`).
    fn read(context: &ToolCallContext, file: &Path, span: Option<(u64, u64)>) {
        let data = std::fs::read(file).unwrap();
        let total = line_count(std::str::from_utf8(&data).unwrap());
        context.reads().unwrap().with(|l| {
            l.record_read(
                &key(file),
                &hash_bytes(&data),
                span,
                Some(total),
                span.is_none(),
                None,
            );
            l.remember_content(&key(file), &hash_bytes(&data), data.clone());
        });
    }

    #[tokio::test]
    async fn test_batch_patch_writes_once_and_snapshots_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        tokio::fs::write(&file, "fn a() {}\nfn b() {}\n")
            .await
            .unwrap();
        let infra = Arc::new(Fixture::default());
        let service = ForgeFsPatch::new(infra.clone());
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, None);

        let actual = service
            .patch(
                file.display().to_string(),
                vec![
                    PatchEdit::new("fn a()", "fn x()"),
                    PatchEdit::new("fn b()", "fn y()"),
                ],
                true,
                &context,
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
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, None);

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("one", "1"), PatchEdit::new("three", "3")],
                true,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(
            actual.starts_with("edit 2: Could not find match"),
            "{actual}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(infra.snapshots.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_patch_reports_only_introduced_syntax_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        tokio::fs::write(&file, "def f():\n    return 1\n")
            .await
            .unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, None);

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("def f():", "def f(:")],
                false,
                &context,
            )
            .await
            .unwrap();

        assert_eq!(actual.errors.len(), 1);
    }

    #[tokio::test]
    async fn test_patch_without_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("u.txt");
        tokio::fs::write(&file, "x\n").await.unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("x", "y")],
                false,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();

        let expected = "You must read the file with the read tool before attempting to edit it.";
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_patch_on_unseen_lines_names_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.txt");
        tokio::fs::write(&file, "a\nb\nc\nd\ne\n").await.unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, Some((1, 2)));

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("d", "D")],
                false,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();

        let expected = "You must read the target line (4) before attempting to edit it.";
        assert_eq!(actual, expected);
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            "a\nb\nc\nd\ne\n"
        );
    }

    #[tokio::test]
    async fn test_replace_all_with_one_unseen_occurrence_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.txt");
        tokio::fs::write(&file, "foo\nbar\nfoo\n").await.unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, Some((1, 2)));

        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("foo", "baz").replace_all(true)],
                false,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();

        let expected = "You must read the target line (3) before attempting to edit it.";
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_changed_on_disk_refusal_then_retry_without_reread() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.txt");
        tokio::fs::write(&file, "alpha\nbeta\ngamma\n")
            .await
            .unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, None);
        tokio::fs::write(&file, "alpha\nBETA\ngamma\n")
            .await
            .unwrap();

        let refused = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("gamma", "delta")],
                false,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();
        let retry = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("gamma", "delta")],
                false,
                &context,
            )
            .await
            .unwrap();

        assert!(refused.contains("-beta\n+BETA\n"), "{refused}");
        assert_eq!(retry.after, "alpha\nBETA\ndelta\n");
    }

    #[tokio::test]
    async fn test_coverage_follows_edits_for_a_second_patch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        tokio::fs::write(&file, "1\n2\n3\n4\n").await.unwrap();
        let service = ForgeFsPatch::new(Arc::new(Fixture::default()));
        let context = ToolCallContext::new(Metrics::default());
        read(&context, &file, Some((1, 2)));

        service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("1\n", "0\n1\n")],
                false,
                &context,
            )
            .await
            .unwrap();
        let actual = service
            .patch(
                file.display().to_string(),
                vec![PatchEdit::new("2\n", "two\n")],
                false,
                &context,
            )
            .await
            .unwrap()
            .after;

        let expected = "0\n1\ntwo\n3\n4\n";
        assert_eq!(actual, expected);
    }
}
