use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use bstr::ByteSlice;
use bytes::Bytes;
use forge_app::{
    EnvironmentInfra, FileDirectoryInfra, FileInfoInfra, FileReaderInfra, FileWriterInfra,
    FsWriteOutput, FsWriteService, compute_hash,
};
use forge_domain::{SnapshotCoverage, SnapshotRepository, ToolCallContext};

use crate::tool_services::patch_engine::{detect_line_ending, normalize_line_endings};
use crate::tool_services::provenance::{
    Staleness, changed_since_read, hash_bytes, key, line_count, require_read,
};
use crate::tool_services::syn::introduced_errors;
use crate::utils::assert_absolute_path;

/// Service for creating and replacing files.
///
/// The `write` tool replaces an existing file only with `overwrite=true`,
/// after a read of that file in this conversation, and only when the file is
/// unchanged since; a changed file is refused with a diff. Every write is
/// snapshotted first (a new file snapshots as "did not exist", so undo
/// deletes it). The file's existing line-ending style (LF, CRLF or CR) is
/// kept.
pub struct ForgeFsWrite<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsWrite<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

/// Half of the tighter result cap: the most diff text one result carries.
pub(crate) fn diff_budget(config: &forge_config::ForgeConfig) -> usize {
    [
        config.max_tool_result_inline_bytes,
        config.max_tool_result_bytes,
    ]
    .into_iter()
    .filter(|cap| *cap > 0)
    .min()
    .map(|cap| (cap / 2) as usize)
    .unwrap_or(0)
}

#[async_trait::async_trait]
impl<
    F: FileDirectoryInfra
        + FileInfoInfra
        + FileReaderInfra
        + FileWriterInfra
        + SnapshotRepository
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + Send
        + Sync,
> FsWriteService for ForgeFsWrite<F>
{
    async fn write(
        &self,
        path: String,
        content: String,
        overwrite: bool,
    ) -> anyhow::Result<FsWriteOutput> {
        let path = Path::new(&path);
        assert_absolute_path(path)?;

        if let Some(parent) = Path::new(&path).parent() {
            self.infra
                .create_dirs(parent)
                .await
                .with_context(|| format!("Failed to create directories: {}", path.display()))?;
        }

        let file_exists = self.infra.is_file(path).await?;
        if file_exists && !overwrite {
            return Err(anyhow::anyhow!(
                "Cannot overwrite existing file: overwrite flag not set.",
            ))
            .with_context(|| format!("File already exists at {}", path.display()));
        }

        let old_content = if file_exists {
            Some(self.infra.read_utf8(path).await?)
        } else {
            None
        };
        let ending = old_content
            .as_deref()
            .map(detect_line_ending)
            .unwrap_or("\n");
        let normalized_content = normalize_line_endings(&content, ending);
        self.infra
            .write(path, Bytes::from(normalized_content.clone()))
            .await?;
        let content_hash = compute_hash(&normalized_content);
        let errors = introduced_errors(path, old_content.as_deref(), &normalized_content);

        Ok(FsWriteOutput {
            path: path.display().to_string(),
            before: old_content,
            errors,
            content_hash,
            bytes: normalized_content.len(),
            notices: Vec::new(),
        })
    }

    async fn write_tool(
        &self,
        path: String,
        content: String,
        overwrite: bool,
        context: &ToolCallContext,
    ) -> anyhow::Result<FsWriteOutput> {
        let path = Path::new(&path);
        assert_absolute_path(path)?;
        let config = self.infra.get_config()?;
        let ledger = context.reads()?;

        let existing = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                anyhow::bail!("{} is a directory", path.display())
            }
            Ok(_) => Some(std::fs::read(path)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        let old_content = match &existing {
            Some(current) => {
                if !overwrite {
                    anyhow::bail!(
                        "{} already exists. To change part of it use patch; to replace it whole, read it first and pass overwrite=true.",
                        path.display()
                    );
                }
                let known = ledger.with(|l| l.known_content(&key(path)).map(<[u8]>::to_vec));
                if let Staleness::Changed(message) = changed_since_read(
                    &ledger,
                    path,
                    known.as_deref(),
                    current,
                    "overwrite it",
                    false,
                    diff_budget(&config),
                ) {
                    anyhow::bail!(message);
                }
                require_read(
                    &ledger,
                    path,
                    "overwrite it",
                    &hash_bytes(current),
                    &[],
                    None,
                )?;
                Some(current.to_str_lossy().into_owned())
            }
            None => None,
        };

        let coverage = ledger.with(|l| {
            l.coverage(&key(path)).map(|c| SnapshotCoverage {
                hash: c.hash.clone(),
                ranges: c.ranges.clone(),
                whole: c.whole,
            })
        });
        let notices = self
            .infra
            .insert_snapshot(context.conversation_id(), path, coverage)
            .await?;

        if let Some(parent) = path.parent() {
            self.infra
                .create_dirs(parent)
                .await
                .with_context(|| format!("Failed to create directories: {}", path.display()))?;
        }
        let ending = old_content
            .as_deref()
            .map(detect_line_ending)
            .unwrap_or("\n");
        let normalized = normalize_line_endings(&content, ending);
        if let Err(error) = self
            .infra
            .write(path, Bytes::from(normalized.clone()))
            .await
        {
            self.infra
                .discard_snapshot(context.conversation_id(), path)
                .await?;
            return Err(error);
        }

        let content_hash = hash_bytes(normalized.as_bytes());
        ledger.with(|l| {
            if old_content.is_some() {
                // The model wrote every byte of the new content.
                l.replace(
                    &key(path),
                    &content_hash,
                    vec![],
                    line_count(&normalized),
                    true,
                );
            }
            l.remember_content(&key(path), &content_hash, normalized.clone().into_bytes());
        });
        let errors = introduced_errors(path, old_content.as_deref(), &normalized);

        Ok(FsWriteOutput {
            path: path.display().to_string(),
            before: old_content,
            errors,
            content_hash: compute_hash(&normalized),
            bytes: normalized.len(),
            notices,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use forge_domain::{ConversationId, Metrics, UndoOutcome};
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    pub(crate) struct Fixture {
        config: forge_config::ForgeConfig,
        snapshots: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl Fixture {
        /// A fixture with the given config.
        pub(crate) fn with_config(config: forge_config::ForgeConfig) -> Self {
            Self { config, ..Default::default() }
        }

        /// Number of snapshots taken.
        pub(crate) fn snapshot_count(&self) -> usize {
            self.snapshots.lock().unwrap().len()
        }
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
            Ok(self.config.clone())
        }
        async fn update_environment(
            &self,
            _ops: Vec<forge_domain::ConfigOperation>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl FileDirectoryInfra for Fixture {
        async fn create_dirs(&self, path: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(path)?)
        }
    }

    #[async_trait::async_trait]
    impl FileInfoInfra for Fixture {
        async fn is_binary(&self, _path: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn is_file(&self, path: &Path) -> anyhow::Result<bool> {
            Ok(path.is_file())
        }
        async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
            Ok(path.exists())
        }
        async fn file_size(&self, path: &Path) -> anyhow::Result<u64> {
            Ok(std::fs::metadata(path)?.len())
        }
    }

    #[async_trait::async_trait]
    impl FileReaderInfra for Fixture {
        async fn read_utf8(&self, path: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(path)?)
        }
        fn read_batch_utf8(
            &self,
            _batch_size: usize,
            _paths: Vec<PathBuf>,
        ) -> impl futures::Stream<Item = (PathBuf, anyhow::Result<String>)> + Send {
            futures::stream::empty()
        }
        async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(std::fs::read(path)?)
        }
        async fn range_read_utf8(
            &self,
            _path: &Path,
            _start_line: u64,
            _end_line: u64,
        ) -> anyhow::Result<(String, forge_domain::FileInfo)> {
            unimplemented!()
        }
    }

    #[async_trait::async_trait]
    impl FileWriterInfra for Fixture {
        async fn write(&self, path: &Path, contents: Bytes) -> anyhow::Result<()> {
            Ok(std::fs::write(path, contents)?)
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
        async fn discard_snapshot(
            &self,
            _conversation: &ConversationId,
            _path: &Path,
        ) -> anyhow::Result<()> {
            self.snapshots.lock().unwrap().pop();
            Ok(())
        }
        async fn undo_snapshot(
            &self,
            _conversation: &ConversationId,
            _path: &Path,
        ) -> anyhow::Result<UndoOutcome> {
            unimplemented!()
        }
        async fn has_snapshot(
            &self,
            _conversation: &ConversationId,
            _path: &Path,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    fn service() -> (ForgeFsWrite<Fixture>, Arc<Fixture>) {
        let config = forge_config::ForgeConfig {
            max_tool_result_inline_bytes: 51200,
            max_tool_result_bytes: 262144,
            ..Default::default()
        };
        let infra = Arc::new(Fixture { config, ..Default::default() });
        (ForgeFsWrite::new(infra.clone()), infra)
    }

    #[tokio::test]
    async fn test_new_file_created_with_parents_and_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a/b/new.txt");
        let (service, infra) = service();
        let context = ToolCallContext::new(Metrics::default());

        service
            .write_tool(
                file.display().to_string(),
                "hi\n".to_string(),
                false,
                &context,
            )
            .await
            .unwrap();

        let actual = (
            std::fs::read_to_string(&file).unwrap(),
            infra.snapshots.lock().unwrap().len(),
        );
        let expected = ("hi\n".to_string(), 1);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_overwrite_requires_read_then_succeeds_and_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("w.txt");
        std::fs::write(&file, "a\r\nb\r\n").unwrap();
        let (service, _infra) = service();
        let context = ToolCallContext::new(Metrics::default());

        let refused = service
            .write_tool(
                file.display().to_string(),
                "x\ny\n".to_string(),
                true,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();
        let current = std::fs::read(&file).unwrap();
        context.reads().unwrap().with(|l| {
            l.record_read(
                &key(&file),
                &hash_bytes(&current),
                Some((1, 1)),
                Some(2),
                false,
                None,
            )
        });
        service
            .write_tool(
                file.display().to_string(),
                "x\ny\n".to_string(),
                true,
                &context,
            )
            .await
            .unwrap();

        let actual = (refused, std::fs::read_to_string(&file).unwrap());
        let expected = (
            "You must read the file with the read tool before attempting to overwrite it."
                .to_string(),
            "x\r\ny\r\n".to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_overwrite_of_changed_file_returns_diff() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.txt");
        std::fs::write(&file, "old\n").unwrap();
        let (service, _infra) = service();
        let context = ToolCallContext::new(Metrics::default());
        let read = std::fs::read(&file).unwrap();
        context.reads().unwrap().with(|l| {
            l.record_read(&key(&file), &hash_bytes(&read), None, Some(1), true, None);
            l.remember_content(&key(&file), &hash_bytes(&read), read.clone());
        });
        std::fs::write(&file, "changed\n").unwrap();

        let actual = service
            .write_tool(
                file.display().to_string(),
                "new\n".to_string(),
                true,
                &context,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(
            actual.contains("changed on disk since it was read"),
            "{actual}"
        );
        assert!(actual.contains("-old\n+changed\n"), "{actual}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed\n");
    }

    #[tokio::test]
    async fn test_existing_without_overwrite_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("e.txt");
        std::fs::write(&file, "keep\n").unwrap();
        let (service, _infra) = service();
        let context = ToolCallContext::new(Metrics::default());

        let actual = service
            .write_tool(file.display().to_string(), "x".to_string(), false, &context)
            .await
            .unwrap_err()
            .to_string();

        let expected = format!(
            "{} already exists. To change part of it use patch; to replace it whole, read it first and pass overwrite=true.",
            file.display()
        );
        assert_eq!(actual, expected);
    }
}
