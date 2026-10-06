//! X11/X12: one bounding layer for every tool result, MCP included.
//!
//! A result whose text is larger than the inline limit is written whole to a
//! content-addressed file under the data directory and replaced by a head
//! preview plus a notice naming the file and the `read` range that continues
//! it. Any result is then held to the hard cap. Finally a whole batch is held
//! to a per-message budget by water-filling: small results pass whole and the
//! largest are clipped to an equal share. After clipping, the read coverage a
//! read call recorded is narrowed to the lines its delivered text still shows.

use std::path::{Path, PathBuf};

use forge_domain::{LineSpan, SharedLedger, ToolOutput, ToolResult, ToolValue};
use serde_json::json;

/// Byte limits applied to tool results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultLimits {
    /// Results above this are spilled to a file (0 disables spilling).
    pub inline: usize,
    /// Hard cap on one result (0 disables).
    pub hard: usize,
    /// Budget for all results of one assistant message (0 disables).
    pub batch: usize,
}

/// Text bytes of a result (images are not counted).
pub fn text_size(output: &ToolOutput) -> usize {
    output
        .values
        .iter()
        .map(|value| match value {
            ToolValue::Text(text) => text.len(),
            ToolValue::AI { value, .. } => value.len(),
            _ => 0,
        })
        .sum()
}

/// The longest prefix of `text` that is at most `limit` bytes and ends on a
/// character boundary.
fn byte_prefix(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Clips `text` to `cap` bytes including `marker`, when it is longer.
fn cap_text(text: &str, cap: usize, marker: &str) -> String {
    if cap == 0 || text.len() <= cap {
        return text.to_string();
    }
    let keep = cap.saturating_sub(marker.len());
    format!("{}{marker}", byte_prefix(text, keep))
}

/// Writes `text` to `<dir>/result-<sha16>.txt` (atomic, idempotent) and returns
/// the preview that replaces it. Returns `None` when the file cannot be
/// written; the hard cap then still applies.
pub fn spill(text: &str, inline: usize, dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    std::fs::create_dir_all(dir).ok()?;
    let digest = hex::encode(Sha256::digest(text.as_bytes()));
    let path = dir.join(format!("result-{}.txt", &digest[..16]));
    if !path.exists() {
        let partial = dir.join(format!(
            ".result-{}.{}.tmp",
            &digest[..16],
            std::process::id()
        ));
        std::fs::write(&partial, text).ok()?;
        std::fs::rename(&partial, &path).ok()?;
    }
    let total = text.len();
    let lines = text.lines().count();
    let notice = |next_byte: usize, next_line: usize| -> String {
        let by_byte = json!({"start_byte": next_byte});
        let how = if lines > 1 {
            format!(
                "read it on with range {by_byte}, or by line with range {}",
                json!({"start_line": next_line})
            )
        } else {
            format!(
                "read it on with range {by_byte}; it is one line, so only a byte range splits it"
            )
        };
        format!(
            "[result was {total} bytes, over max_tool_result_inline_bytes ({inline}); the full text is at {} — {how}]",
            path.display()
        )
    };
    let widest = notice(total, lines + 1);
    let budget = (inline / 2).min(inline.saturating_sub(widest.len() + 2));
    let head = byte_prefix(text, budget);
    let head_lines: Vec<&str> = head.split_inclusive('\n').collect();
    let next_line = match head_lines.last() {
        Some(last) if last.ends_with('\n') => head_lines.len() + 1,
        Some(_) => head_lines.len(),
        None => 1,
    };
    let notice = notice(head.len(), next_line.max(1));
    Some(if head.is_empty() {
        notice
    } else {
        format!("{head}\n\n{notice}")
    })
}

/// Applies the inline spill and the hard cap to one result's text values.
pub fn bound_result(output: ToolOutput, limits: ResultLimits, spill_dir: &Path) -> ToolOutput {
    let size = text_size(&output);
    if size == 0 {
        return output;
    }
    let needs_spill = limits.inline > 0 && size > limits.inline;
    let needs_cap = limits.hard > 0 && size > limits.hard;
    if !needs_spill && !needs_cap {
        return output;
    }
    let text = joined_text(&output);
    let mut bounded = if needs_spill {
        spill(&text, limits.inline, spill_dir).unwrap_or(text)
    } else {
        text
    };
    if limits.hard > 0 {
        let marker = format!(
            "\n[truncated: max_tool_result_bytes ({}) reached]",
            limits.hard
        );
        bounded = cap_text(&bounded, limits.hard, &marker);
    }
    replace_text(output, bounded)
}

fn joined_text(output: &ToolOutput) -> String {
    output
        .values
        .iter()
        .filter_map(ToolValue::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replaces all text values by one text value, keeping images and the error
/// flags.
fn replace_text(output: ToolOutput, text: String) -> ToolOutput {
    let mut values: Vec<ToolValue> = vec![ToolValue::Text(text)];
    values.extend(
        output
            .values
            .into_iter()
            .filter(|value| matches!(value, ToolValue::Image(_))),
    );
    ToolOutput {
        is_error: output.is_error,
        values,
        uncounted: output.uncounted,
    }
}

/// Largest allowance L with sum(min(size, L)) <= budget; `None` when all fit.
pub fn fair_share(sizes: &[usize], budget: usize) -> Option<usize> {
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let mut remaining = budget;
    for (index, size) in sorted.iter().enumerate() {
        let share = remaining / (sorted.len() - index);
        if *size > share {
            return Some(share);
        }
        remaining -= size;
    }
    None
}

/// X12: clips a batch of results to `budget` total bytes, largest first.
pub fn bound_batch(results: &mut [ToolResult], budget: usize) {
    if budget == 0 {
        return;
    }
    let sizes: Vec<usize> = results.iter().map(|r| text_size(&r.output)).collect();
    if sizes.iter().sum::<usize>() <= budget {
        return;
    }
    let Some(allowance) = fair_share(&sizes, budget) else {
        return;
    };
    let marker = format!("\n[truncated: per-message tool result budget ({budget}) reached]");
    for (result, size) in results.iter_mut().zip(sizes) {
        if size > allowance {
            let text = cap_text(&joined_text(&result.output), allowance, &marker);
            result.output = replace_text(std::mem::take(&mut result.output), text);
        }
    }
}

/// Lines a delivered numbered read result still shows whole: every line of
/// the form `N:text` (optionally right-aligned) before any truncation marker,
/// except a last line that was cut.
pub fn delivered_lines(original: &str, delivered: &str) -> Vec<LineSpan> {
    let original: std::collections::HashSet<&str> = original.lines().collect();
    let mut spans: Vec<LineSpan> = Vec::new();
    for line in delivered.lines() {
        if !original.contains(line) {
            continue;
        }
        let Some((number, _)) = line.trim_start().split_once(':') else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        match spans.last_mut() {
            Some(last) if last.1 + 1 == number => last.1 = number,
            _ => spans.push((number, number)),
        }
    }
    spans
}

/// Narrows the coverage a read call recorded when its result was clipped.
///
/// # Arguments
/// * `ledger` - The conversation's read ledger.
/// * `call_id` - The read call.
/// * `original` - The read's full result text.
/// * `delivered` - What the model receives after bounding.
pub fn narrow_read_coverage(ledger: &SharedLedger, call_id: &str, original: &str, delivered: &str) {
    if original == delivered {
        return;
    }
    ledger.with(|l| {
        if let Some(path) = l.offered_path(call_id).map(str::to_string) {
            l.record_delivered(&path, call_id, delivered_lines(original, delivered));
        }
    });
}

/// Directory under the data dir where spilled results are kept.
pub fn spill_dir(base_path: &Path) -> PathBuf {
    base_path.join("tool-results")
}

#[cfg(test)]
mod tests {
    use forge_domain::ToolName;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_spill_writes_full_text_and_previews_head() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let limits = ResultLimits { inline: 400, hard: 10_000, batch: 0 };

        let actual = bound_result(ToolOutput::text(&text), limits, dir.path());

        let preview = actual.as_str().unwrap().to_string();
        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert!(preview.starts_with("line 1\nline 2\n"), "{preview}");
        assert!(
            preview.contains(&format!("the full text is at {}", file.display())),
            "{preview}"
        );
        assert!(preview.contains("\"start_line\""), "{preview}");
        assert!(preview.len() <= 400, "{}", preview.len());
    }

    #[test]
    fn test_hard_cap_applies_without_spill() {
        let dir = tempfile::tempdir().unwrap();
        let limits = ResultLimits { inline: 0, hard: 50, batch: 0 };

        let actual = bound_result(ToolOutput::text("x".repeat(200)), limits, dir.path());

        let text = actual.as_str().unwrap();
        assert_eq!(text.len(), 50);
        assert!(text.ends_with("reached]"));
    }

    #[test]
    fn test_fair_share_pays_small_results_whole() {
        let actual = (fair_share(&[10, 10, 1000], 300), fair_share(&[10, 20], 100));
        let expected = (Some(280), None);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_bound_batch_clips_only_the_largest() {
        let mut fixture = vec![
            ToolResult::new(ToolName::new("a")).output(Ok(ToolOutput::text("x".repeat(10)))),
            ToolResult::new(ToolName::new("b")).output(Ok(ToolOutput::text("y".repeat(1000)))),
        ];
        bound_batch(&mut fixture, 300);
        let actual: Vec<usize> = fixture.iter().map(|r| text_size(&r.output)).collect();
        let expected = vec![10, 290];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_delivered_lines_drop_cut_last_line() {
        let original = "1:alpha\n2:beta\n3:gamma";
        let delivered = "1:alpha\n2:be\n[truncated]";
        let actual = delivered_lines(original, delivered);
        let expected = vec![(1, 1)];
        assert_eq!(actual, expected);
    }
}
