//! Shared provenance helpers for the file tools: content hashing, line
//! counting, the read-before-edit gate (X13) and the changed-on-disk response
//! (X14).

use std::path::Path;

use forge_domain::{CoverageError, LineSpan, SharedLedger, merge_ranges, render_spans, short_hash};
use similar::{Algorithm, DiffTag, TextDiff, capture_diff_slices};

use crate::tool_services::patch_engine::split_lines_keep;

/// SHA-256 of `bytes` as lowercase hex; the ledger's content hash.
pub fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Number of lines in `text` (a final line without a line ending counts).
pub fn line_count(text: &str) -> u64 {
    split_lines_keep(text).len() as u64
}

/// The ledger key for a path.
pub fn key(path: &Path) -> String {
    path.display().to_string()
}

/// Unified diff from `old` to `new` with file labels.
pub fn unified_diff(old: &str, new: &str, from: &str, to: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(from, to)
        .to_string()
}

/// Outcome of comparing the current file with the content the model read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// The file is unchanged since it was read, or was never read.
    Fresh,
    /// The file changed; the message is the error the model gets.
    Changed(String),
}

/// X14: when `path` was read and has changed on disk since, builds the
/// refusal with a unified diff from the content read to the content now, and
/// moves read coverage to the current content: seen lines the change left
/// alone stay seen, and the lines of the hunks shown count as seen when the
/// diff fits in `budget` bytes. A diff over budget lists the changed spans
/// instead.
///
/// # Arguments
/// * `ledger` - The conversation's read ledger.
/// * `path` - The file.
/// * `known` - The content the model read (`None` when it is not cached).
/// * `current` - The bytes on disk now.
/// * `action` - Verb phrase for the message, e.g. `edit it`.
/// * `edits_lines` - True for patch (changed lines must be read before a retry
///   when the diff was not shown); false for an overwrite.
/// * `budget` - Most diff bytes the result can carry.
pub fn changed_since_read(
    ledger: &SharedLedger,
    path: &Path,
    known: Option<&[u8]>,
    current: &[u8],
    action: &str,
    edits_lines: bool,
    budget: usize,
) -> Staleness {
    let path_key = key(path);
    let current_hash = hash_bytes(current);
    let Some(coverage) = ledger.with(|l| l.coverage(&path_key).cloned()) else {
        return Staleness::Fresh;
    };
    if coverage.hash == current_hash {
        return Staleness::Fresh;
    }
    let (Some(known), Ok(new_text)) = (known, std::str::from_utf8(current)) else {
        return Staleness::Fresh;
    };
    if hash_bytes(known) != coverage.hash {
        return Staleness::Fresh;
    }
    let Ok(old_text) = std::str::from_utf8(known) else {
        return Staleness::Fresh;
    };

    let old_lines = split_lines_keep(old_text);
    let new_lines = split_lines_keep(new_text);
    let prior = coverage.seen_ranges();
    let mut carried: Vec<LineSpan> = Vec::new();
    let mut changed: Vec<LineSpan> = Vec::new();
    let ops = capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines);
    for op in &ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            for &(seen_start, seen_end) in &prior {
                let low = seen_start.max(old_range.start as u64 + 1);
                let high = seen_end.min(old_range.end as u64);
                if low <= high {
                    let shift = new_range.start as i64 - old_range.start as i64;
                    carried.push(((low as i64 + shift) as u64, (high as i64 + shift) as u64));
                }
            }
        } else if new_range.end > new_range.start {
            changed.push((new_range.start as u64 + 1, new_range.end as u64));
        }
    }
    let diff = TextDiff::from_lines(old_text, new_text);
    let shown: Vec<LineSpan> = diff
        .grouped_ops(3)
        .iter()
        .filter_map(|group| {
            let first = group.first()?.new_range();
            let last = group.last()?.new_range();
            (last.end > first.start).then_some((first.start as u64 + 1, last.end as u64))
        })
        .collect();
    let rendered = unified_diff(
        old_text,
        new_text,
        &format!("{} (as read)", path.display()),
        &format!("{} (now)", path.display()),
    );
    let delivered = budget == 0 || rendered.len() <= budget;
    let mut ranges = carried;
    if delivered {
        ranges.extend(shown);
    }
    ledger.with(|l| {
        l.replace(
            &path_key,
            &current_hash,
            merge_ranges(ranges),
            new_lines.len() as u64,
            coverage.whole && delivered,
        )
    });

    let head = format!(
        "{} changed on disk since it was read (hash {} when read, {} now); nothing was written.",
        path.display(),
        short_hash(&coverage.hash),
        short_hash(&current_hash)
    );
    if delivered {
        return Staleness::Changed(format!(
            "{head} Diff from what you read to what is there now:\n{rendered}Retry to {action} against the current text."
        ));
    }
    let spans = if changed.is_empty() {
        "none (lines were only removed)".to_string()
    } else {
        render_spans(&changed)
    };
    if edits_lines {
        Staleness::Changed(format!(
            "{head} The diff is too large to show; the changed lines are now {spans}. Read those, then retry to {action}."
        ))
    } else {
        Staleness::Changed(format!(
            "{head} The diff is too large to show; the changed lines are now {spans}. A retry will {action} and discard them (undo restores them); read them first if you need what changed."
        ))
    }
}

/// X13 gate: `path` must have been read, its hash must be `current_hash`, and
/// every span must be covered.
///
/// # Errors
/// Returns the refusal the model should see.
pub fn require_read(
    ledger: &SharedLedger,
    path: &Path,
    action: &str,
    current_hash: &str,
    spans: &[LineSpan],
    seen: Option<&[LineSpan]>,
) -> anyhow::Result<()> {
    ledger
        .with(|l| l.check(&key(path), action, Some(current_hash), spans, seen))
        .map_err(|error: CoverageError| anyhow::anyhow!(error))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_changed_since_read_moves_coverage_and_shows_diff() {
        let ledger = SharedLedger::default();
        let path = Path::new("/w/a.txt");
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        ledger.with(|l| {
            l.record_read(
                "/w/a.txt",
                &hash_bytes(old.as_bytes()),
                Some((1, 3)),
                Some(10),
                false,
                None,
            )
        });

        let actual = changed_since_read(
            &ledger,
            path,
            Some(old.as_bytes()),
            new.as_bytes(),
            "edit it",
            true,
            10_000,
        );

        let Staleness::Changed(message) = actual else {
            panic!("expected change")
        };
        assert!(message.contains("-j\n+J\n"), "{message}");
        let coverage = ledger.with(|l| l.coverage("/w/a.txt").cloned()).unwrap();
        assert_eq!(coverage.hash, hash_bytes(new.as_bytes()));
        assert_eq!(coverage.ranges, vec![(1, 3), (7, 10)]);
    }

    #[test]
    fn test_changed_since_read_over_budget_lists_spans_only() {
        let ledger = SharedLedger::default();
        let path = Path::new("/w/b.txt");
        let old = "x\ny\n";
        let new = "x\nY\n";
        ledger.with(|l| {
            l.record_read(
                "/w/b.txt",
                &hash_bytes(old.as_bytes()),
                Some((1, 2)),
                Some(2),
                false,
                None,
            )
        });

        let actual = changed_since_read(
            &ledger,
            path,
            Some(old.as_bytes()),
            new.as_bytes(),
            "edit it",
            true,
            1,
        );

        let expected = Staleness::Changed(format!(
            "/w/b.txt changed on disk since it was read (hash {} when read, {} now); nothing was written. The diff is too large to show; the changed lines are now 2. Read those, then retry to edit it.",
            short_hash(&hash_bytes(old.as_bytes())),
            short_hash(&hash_bytes(new.as_bytes()))
        ));
        assert_eq!(actual, expected);
        let coverage = ledger.with(|l| l.coverage("/w/b.txt").cloned()).unwrap();
        assert_eq!(coverage.ranges, vec![(1, 1)]);
    }

    #[test]
    fn test_unchanged_is_fresh() {
        let ledger = SharedLedger::default();
        let text = "same\n";
        ledger.with(|l| {
            l.record_read(
                "/w/c",
                &hash_bytes(text.as_bytes()),
                None,
                Some(1),
                true,
                None,
            )
        });
        let actual = changed_since_read(
            &ledger,
            Path::new("/w/c"),
            Some(text.as_bytes()),
            text.as_bytes(),
            "edit it",
            true,
            100,
        );
        assert_eq!(actual, Staleness::Fresh);
    }
}
