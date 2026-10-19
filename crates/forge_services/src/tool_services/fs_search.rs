use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use forge_app::FsSearchService;
use forge_domain::{
    AstSearch, FSSearch, OutputMode, SnapshotCoverage, SnapshotRepository, ToolCallContext,
};

use crate::tool_services::ast_engine;
use crate::tool_services::provenance::{hash_bytes, key, line_count};
use crate::tool_services::search_engine::{Mode, SearchSpec, run};

/// The `fs_search` (T6) and `ast_search` (T8) tools. Both search in-process
/// on a blocking thread; `fs_search` is bounded by a 120 s timeout. An
/// applied structural rewrite snapshots every file it changes first.
pub struct ForgeFsSearch<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsSearch<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

/// Builds the engine request from tool input.
fn spec(params: FSSearch) -> anyhow::Result<SearchSpec> {
    let root = match params.path.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().context("Failed to get current working directory")?,
    };
    let context = params.context.map(|c| c as usize);
    Ok(SearchSpec {
        pattern: params.pattern,
        root,
        glob: params.glob.filter(|g| !g.is_empty()),
        file_type: params.file_type.filter(|t| !t.is_empty()),
        mode: match params.output_mode {
            Some(OutputMode::Content) => Mode::Content,
            Some(OutputMode::Count) => Mode::Count,
            _ => Mode::FilesWithMatches,
        },
        before: context
            .or(params.context_before.map(|c| c as usize))
            .unwrap_or(0),
        after: context
            .or(params.context_after.map(|c| c as usize))
            .unwrap_or(0),
        line_numbers: params.line_numbers.unwrap_or(true),
        case_insensitive: params.case_insensitive.unwrap_or(false),
        multiline: params.multiline.unwrap_or(false),
        head_limit: params.head_limit.map(|l| l as usize),
        offset: params.offset.unwrap_or(0) as usize,
    })
}

#[async_trait::async_trait]
impl<F: SnapshotRepository + Send + Sync + 'static> FsSearchService for ForgeFsSearch<F> {
    async fn search(&self, params: FSSearch) -> anyhow::Result<String> {
        let spec = spec(params)?;
        tokio::task::spawn_blocking(move || run(&spec)).await?
    }

    async fn ast_search(
        &self,
        params: AstSearch,
        context: &ToolCallContext,
    ) -> anyhow::Result<String> {
        if params.apply && params.rewrite.is_none() {
            bail!("apply=true requires rewrite");
        }
        let root = PathBuf::from(params.path.clone().unwrap_or_else(|| ".".to_string()));
        let limit = params
            .max_results
            .map(|n| n as usize)
            .unwrap_or(ast_engine::DEFAULT_MAX_RESULTS)
            .max(1);
        let (pattern, language, rewrite) = (
            params.pattern.clone(),
            params.language.clone(),
            params.rewrite.clone(),
        );
        let result = tokio::task::spawn_blocking(move || {
            ast_engine::search(
                &pattern,
                &root,
                language.as_deref(),
                rewrite.as_deref(),
                limit,
            )
        })
        .await??;

        if params.rewrite.is_none() {
            return Ok(ast_engine::render_matches(&result));
        }
        let diff = ast_engine::render_diff(&result.rewrites);
        if !params.apply {
            let suffix = if result.overflow {
                "\n[additional matches omitted; increase max_results to preview them]"
            } else {
                ""
            };
            return Ok(format!(
                "DRY RUN: no files changed. Pass apply=true to apply.\n{diff}{suffix}"
            ));
        }
        if result.overflow {
            bail!(
                "rewrite matched more than max_results={limit}; narrow path or increase max_results before applying"
            );
        }

        let ledger = context.reads()?;
        for path in result.rewrites.keys() {
            let coverage = ledger.with(|l| {
                l.coverage(&key(path)).map(|c| SnapshotCoverage {
                    hash: c.hash.clone(),
                    ranges: c.ranges.clone(),
                    whole: c.whole,
                })
            });
            self.infra
                .insert_snapshot(context.conversation_id(), path, coverage)
                .await?;
        }
        let rewrites = result.rewrites.clone();
        let changed = tokio::task::spawn_blocking(move || ast_engine::write_rewrites(&rewrites))
            .await?
            .map_err(|e| anyhow::anyhow!("{e:#}; snapshots are available through undo"))?;
        // Coverage after a rewrite: a file the model had read whole stays
        // whole (it saw every change in the diff); any other read file keeps
        // no line coverage, since the rewrite may have moved its lines. The
        // new content is remembered for changed-on-disk diffs.
        for (path, (_, after)) in &result.rewrites {
            let hash = hash_bytes(after.as_bytes());
            ledger.with(|l| {
                let whole = l.coverage(&key(path)).is_some_and(|c| c.whole);
                if l.is_read(&key(path)) {
                    l.replace(&key(path), &hash, vec![], line_count(after), whole);
                }
                l.remember_content(&key(path), &hash, after.clone().into_bytes());
            });
        }
        let matches = result.matches.len();
        Ok(format!(
            "rewrote {matches} match{} in {changed} file{}\n{diff}",
            if matches == 1 { "" } else { "es" },
            if changed == 1 { "" } else { "s" }
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use forge_domain::{ConversationId, Metrics, UndoOutcome};
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    struct Snapshots(Mutex<Vec<PathBuf>>);

    #[async_trait::async_trait]
    impl SnapshotRepository for Snapshots {
        async fn insert_snapshot(
            &self,
            _c: &ConversationId,
            path: &Path,
            _coverage: Option<SnapshotCoverage>,
        ) -> anyhow::Result<Vec<String>> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(vec![])
        }
        async fn discard_snapshot(&self, _c: &ConversationId, _p: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        async fn undo_snapshot(
            &self,
            _c: &ConversationId,
            _p: &Path,
        ) -> anyhow::Result<UndoOutcome> {
            unimplemented!()
        }
        async fn has_snapshot(&self, _c: &ConversationId, _p: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn test_search_runs_engine_with_rg_alias_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\nTwo\nthree\n").unwrap();
        let params: FSSearch = serde_json::from_value(serde_json::json!({
            "pattern": "two",
            "path": dir.path().display().to_string(),
            "output_mode": "content",
            "-i": true,
            "-B": 1
        }))
        .unwrap();

        let actual = ForgeFsSearch::new(Arc::new(Snapshots::default()))
            .search(params)
            .await
            .unwrap();

        let file = dir.path().join("a.txt").display().to_string();
        let expected = format!("{file}-1-one\n{file}:2:Two");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_ast_rewrite_dry_run_then_apply_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.py");
        std::fs::write(&file, "old(1)\nold(2)\n").unwrap();
        let infra = Arc::new(Snapshots::default());
        let service = ForgeFsSearch::new(infra.clone());
        let context = ToolCallContext::new(Metrics::default());
        let params = AstSearch {
            pattern: "old($A)".to_string(),
            path: Some(dir.path().display().to_string()),
            rewrite: Some("new($A)".to_string()),
            ..Default::default()
        };

        let dry = service.ast_search(params.clone(), &context).await.unwrap();
        let unchanged = std::fs::read_to_string(&file).unwrap();
        let applied = service
            .ast_search(AstSearch { apply: true, ..params }, &context)
            .await
            .unwrap();

        assert!(dry.starts_with("DRY RUN: no files changed."), "{dry}");
        assert_eq!(unchanged, "old(1)\nold(2)\n");
        assert!(
            applied.starts_with("rewrote 2 matches in 1 file"),
            "{applied}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new(1)\nnew(2)\n");
        assert_eq!(*infra.0.lock().unwrap(), vec![file]);
    }

    #[tokio::test]
    async fn test_ast_apply_refused_over_max_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.py"), "f(1)\nf(2)\n").unwrap();
        let service = ForgeFsSearch::new(Arc::new(Snapshots::default()));
        let context = ToolCallContext::new(Metrics::default());
        let params = AstSearch {
            pattern: "f($A)".to_string(),
            path: Some(dir.path().display().to_string()),
            rewrite: Some("g($A)".to_string()),
            apply: true,
            max_results: Some(1),
            ..Default::default()
        };

        let actual = service
            .ast_search(params, &context)
            .await
            .unwrap_err()
            .to_string();

        assert!(
            actual.starts_with("rewrite matched more than max_results=1"),
            "{actual}"
        );
    }
}
