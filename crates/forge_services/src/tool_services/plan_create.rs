use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use chrono::Local;
use forge_app::{
    EnvironmentInfra, FileDirectoryInfra, FileInfoInfra, FileReaderInfra, FileWriterInfra,
    PlanCreateOutput, PlanCreateService,
};
use forge_domain::{SnapshotRepository, ToolCallContext};

/// The `plan` tool (T20): writes `{plans_dir}/{date}-{name}-{version}.md`.
/// Name and version are sanitised; an existing plan is replaced only with
/// `overwrite`, after an undo snapshot.
pub struct ForgePlanCreate<F>(Arc<F>);

impl<F> ForgePlanCreate<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self(infra)
    }
}

/// A filename component: alphanumerics, `-`, `_` and `.` kept, anything
/// else becomes `-`; leading/trailing `-` and `.` trimmed (so `..` and path
/// separators can never escape the plans directory).
fn sanitize(component: &str, fallback: &str) -> String {
    let mapped: String = component
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

#[async_trait::async_trait]
impl<
    F: FileDirectoryInfra
        + FileInfoInfra
        + FileReaderInfra
        + FileWriterInfra
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + SnapshotRepository
        + Send
        + Sync,
> PlanCreateService for ForgePlanCreate<F>
{
    async fn create_plan(
        &self,
        plan_name: String,
        version: String,
        content: String,
        overwrite: bool,
        context: &ToolCallContext,
    ) -> anyhow::Result<PlanCreateOutput> {
        let current_date = Local::now().format("%Y-%m-%d");
        let filename = format!(
            "{current_date}-{}-{}.md",
            sanitize(&plan_name, "plan"),
            sanitize(&version, "v1")
        );

        let env = self.0.get_environment();
        let plans_dir = match self.0.get_config().ok().and_then(|c| c.plans_dir) {
            Some(dir) if dir.is_absolute() => dir,
            Some(dir) => env.cwd.join(dir),
            None => env.plans_path(),
        };
        let file_path: PathBuf = plans_dir.join(&filename);

        self.0
            .create_dirs(plans_dir.as_path())
            .await
            .with_context(|| {
                format!("Failed to create plans directory: {}", plans_dir.display())
            })?;

        let before = if self.0.is_file(&file_path).await? {
            if !overwrite {
                anyhow::bail!(
                    "Plan file already exists at {}; pass overwrite=true to replace it, or use another name or version.",
                    file_path.display()
                );
            }
            Some(self.0.read_utf8(&file_path).await?)
        } else {
            None
        };
        // Snapshot before writing, so undo restores the old plan (or removes
        // a new one).
        self.0
            .insert_snapshot(context.conversation_id(), &file_path, None)
            .await?;

        self.0
            .write(&file_path, Bytes::from(content))
            .await
            .with_context(|| format!("Failed to write plan file: {}", file_path.display()))?;

        Ok(PlanCreateOutput { path: file_path, before })
    }
}

#[cfg(test)]
mod tests {
    use forge_domain::Metrics;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::tool_services::fs_write::tests::Fixture;

    #[tokio::test]
    async fn test_create_refuse_then_overwrite_with_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let infra = Arc::new(Fixture::with_config(forge_config::ForgeConfig {
            plans_dir: Some(dir.path().join("plans")),
            ..Default::default()
        }));
        let service = ForgePlanCreate::new(infra.clone());
        let context = ToolCallContext::new(Metrics::default());
        let create = |overwrite: bool, content: &str| {
            service.create_plan(
                "auth refactor".to_string(),
                "v1".to_string(),
                content.to_string(),
                overwrite,
                &context,
            )
        };

        let first = create(false, "one").await.unwrap();
        let refused = create(false, "two").await.unwrap_err().to_string();
        let replaced = create(true, "three").await.unwrap();

        let name = first
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.ends_with("-auth-refactor-v1.md"), "{name}");
        assert_eq!(first.before, None);
        assert!(refused.contains("pass overwrite=true"), "{refused}");
        assert_eq!(replaced.before, Some("one".to_string()));
        assert_eq!(std::fs::read_to_string(&replaced.path).unwrap(), "three");
        assert_eq!(infra.snapshot_count(), 2);
    }

    #[test]
    fn test_sanitize_keeps_safe_characters_and_blocks_traversal() {
        let actual = (
            sanitize("../../etc/passwd", "plan"),
            sanitize("my plan: v2!", "plan"),
            sanitize("...", "v1"),
            sanitize("auth_refactor-1.2", "plan"),
        );
        let expected = (
            "etc-passwd".to_string(),
            "my-plan--v2".to_string(),
            "v1".to_string(),
            "auth_refactor-1.2".to_string(),
        );
        assert_eq!(actual, expected);
    }
}
