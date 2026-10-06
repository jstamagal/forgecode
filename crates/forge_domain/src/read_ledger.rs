//! Read coverage ledger: which lines of which file content the model has
//! actually been shown in this conversation.
//!
//! Edits are gated on it: a file must have been read, its content must not
//! have changed since (same hash), and every line an edit touches must be
//! inside the delivered coverage. The ledger also remembers which `read` call
//! showed which lines, so an exact repeat read can be answered with a stub
//! while that earlier result is still in the conversation.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A 1-based inclusive line span.
pub type LineSpan = (u64, u64);

/// Upper bound on the bytes of file content the ledger keeps for
/// changed-on-disk diffs; the oldest entries go first.
const KNOWN_CONTENT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// What the model has seen of one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverage {
    /// Content hash of the file text the coverage describes.
    pub hash: String,
    /// Merged line spans shown to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<LineSpan>,
    /// Line count of the content when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<u64>,
    /// Whether every line has been shown.
    #[serde(default)]
    pub whole: bool,
}

impl FileCoverage {
    /// The spans an edit may touch: the whole file when fully seen, otherwise
    /// the recorded spans.
    pub fn seen_ranges(&self) -> Vec<LineSpan> {
        match (self.whole, self.total_lines) {
            (true, Some(total)) if total > 0 => vec![(1, total)],
            _ => self.ranges.clone(),
        }
    }
}

/// Identity of one rendered read: same content, same window, same rendering.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReadKey {
    pub hash: String,
    pub start: u64,
    pub end: u64,
    pub numbered: bool,
}

/// A read result the model was shown, identified by the call that returned it
/// and a digest of the result text as it sits in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownRead {
    pub key: ReadKey,
    pub call_id: String,
    pub digest: String,
}

/// Per-call contributions to one file's coverage within a batch, kept so a
/// read whose result is later clipped can be narrowed without revoking lines
/// another read delivered whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReadWindow {
    hash: String,
    base_ranges: Vec<LineSpan>,
    base_whole: bool,
    reads: HashMap<String, (Vec<LineSpan>, bool)>,
}

/// Why an edit is not allowed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoverageError {
    #[error("You must read the file with the read tool before attempting to {action}.")]
    NotRead { action: String },
    #[error(
        "{path} changed since it was read (hash {known} when read, {current} now); read it again before attempting to {action}."
    )]
    Changed {
        path: String,
        known: String,
        current: String,
        action: String,
    },
    #[error("You must read the target {noun} ({spans}) before attempting to {action}.")]
    Unseen {
        noun: String,
        spans: String,
        action: String,
    },
}

/// The per-conversation read ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadLedger {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    files: HashMap<String, FileCoverage>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    shown: HashMap<String, Vec<ShownRead>>,
    #[serde(skip)]
    windows: HashMap<String, ReadWindow>,
    #[serde(skip)]
    offered: HashMap<String, (String, ReadKey)>,
    /// Content the model last knew each path to hold: path -> (hash, bytes).
    #[serde(skip)]
    known: IndexMap<String, (String, Vec<u8>)>,
}

impl ReadLedger {
    /// Keeps `data` as the content of `path` that `hash` names, evicting the
    /// oldest entries beyond the size bound.
    pub fn remember_content(&mut self, path: &str, hash: &str, data: Vec<u8>) {
        self.known.shift_remove(path);
        self.known
            .insert(path.to_string(), (hash.to_string(), data));
        let mut total: usize = self.known.values().map(|(_, data)| data.len()).sum();
        while total > KNOWN_CONTENT_MAX_BYTES && self.known.len() > 1 {
            if let Some((_, (_, data))) = self.known.shift_remove_index(0) {
                total -= data.len();
            }
        }
    }

    /// The bytes the model last knew `path` to hold, when they hash to the
    /// recorded coverage hash.
    pub fn known_content(&self, path: &str) -> Option<&[u8]> {
        let (hash, data) = self.known.get(path)?;
        let coverage = self.files.get(path)?;
        (coverage.hash == *hash).then_some(data.as_slice())
    }

    /// Coverage recorded for `path`, if it was read.
    pub fn coverage(&self, path: &str) -> Option<&FileCoverage> {
        self.files.get(path)
    }

    /// Whether `path` has been read in this conversation.
    pub fn is_read(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Records that the model was shown `span` (or the whole file) of the
    /// content with `hash`. A changed hash drops all earlier coverage of the
    /// path first.
    ///
    /// # Arguments
    /// * `path` - Ledger key of the file.
    /// * `hash` - Content hash of the bytes read.
    /// * `span` - Lines shown, when the read showed a line window.
    /// * `total_lines` - Line count of the content, when known.
    /// * `whole` - Whether every line was shown.
    /// * `call_id` - The read call, so a later clip can narrow its share.
    pub fn record_read(
        &mut self,
        path: &str,
        hash: &str,
        span: Option<LineSpan>,
        total_lines: Option<u64>,
        whole: bool,
        call_id: Option<&str>,
    ) {
        if self
            .files
            .get(path)
            .is_some_and(|coverage| coverage.hash != hash)
        {
            self.files.remove(path);
            self.windows.remove(path);
            self.shown.remove(path);
        }
        let previous = self.files.get(path).cloned().unwrap_or_default();
        let window = self
            .windows
            .entry(path.to_string())
            .or_insert_with(|| ReadWindow {
                hash: hash.to_string(),
                base_ranges: previous.ranges.clone(),
                base_whole: previous.whole,
                reads: HashMap::new(),
            });
        if window.hash != hash {
            *window = ReadWindow {
                hash: hash.to_string(),
                base_ranges: previous.ranges.clone(),
                base_whole: previous.whole,
                reads: HashMap::new(),
            };
        }
        let call = call_id.unwrap_or("anonymous").to_string();

        let coverage = self.files.entry(path.to_string()).or_default();
        coverage.hash = hash.to_string();
        if total_lines.is_some() {
            coverage.total_lines = total_lines;
        }
        if whole {
            let full = match total_lines.or(coverage.total_lines) {
                Some(total) if total > 0 => vec![(1, total)],
                _ => Vec::new(),
            };
            window.reads.insert(call, (full.clone(), true));
            coverage.whole = true;
            if !full.is_empty() {
                coverage.ranges = full;
            }
            return;
        }
        let Some((start, end)) = span.filter(|(start, end)| start <= end) else {
            window.reads.insert(call, (Vec::new(), false));
            return;
        };
        window.reads.insert(call, (vec![(start, end)], false));
        let mut ranges = coverage.ranges.clone();
        ranges.push((start, end));
        coverage.ranges = merge_ranges(ranges);
        match coverage.total_lines {
            Some(0) => coverage.whole = true,
            Some(total) if covers(&coverage.ranges, 1, total) => coverage.whole = true,
            _ => {}
        }
    }

    /// Replaces the coverage of `path` outright, e.g. after an edit moved
    /// lines or after an undo. Forgets shown reads of the old content.
    ///
    /// # Arguments
    /// * `path` - Ledger key of the file.
    /// * `hash` - Content hash the new coverage describes.
    /// * `ranges` - Lines the model has seen of that content.
    /// * `total_lines` - Line count of that content.
    /// * `whole` - Whether every line counts as seen.
    pub fn replace(
        &mut self,
        path: &str,
        hash: &str,
        ranges: Vec<LineSpan>,
        total_lines: u64,
        whole: bool,
    ) {
        self.windows.remove(path);
        self.shown.remove(path);
        self.offered
            .retain(|_, (offered_path, _)| offered_path != path);
        self.files.insert(
            path.to_string(),
            FileCoverage {
                hash: hash.to_string(),
                ranges: merge_ranges(ranges),
                total_lines: Some(total_lines),
                whole,
            },
        );
    }

    /// Checks that `path` may be edited: it was read, its hash is `current`
    /// (when given), and every span lies within `seen` (or the recorded
    /// coverage when `seen` is `None`).
    ///
    /// # Errors
    /// Returns the [`CoverageError`] the model should see.
    pub fn check(
        &self,
        path: &str,
        action: &str,
        current: Option<&str>,
        spans: &[LineSpan],
        seen: Option<&[LineSpan]>,
    ) -> Result<(), CoverageError> {
        let Some(coverage) = self.files.get(path) else {
            return Err(CoverageError::NotRead { action: action.to_string() });
        };
        if let Some(current) = current
            && coverage.hash != current
        {
            return Err(CoverageError::Changed {
                path: path.to_string(),
                known: short_hash(&coverage.hash).to_string(),
                current: short_hash(current).to_string(),
                action: action.to_string(),
            });
        }
        let seen = seen
            .map(<[LineSpan]>::to_vec)
            .unwrap_or_else(|| coverage.seen_ranges());
        let unseen: Vec<LineSpan> = spans
            .iter()
            .copied()
            .filter(|&(start, end)| !covers(&seen, start, end))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        let plural = unseen.len() != 1 || unseen[0].0 != unseen[0].1;
        Err(CoverageError::Unseen {
            noun: if plural { "lines" } else { "line" }.to_string(),
            spans: render_spans(&unseen),
            action: action.to_string(),
        })
    }

    /// The call id whose still-present result showed exactly `key` of
    /// `path`, if any.
    pub fn shown_read(&self, path: &str, key: &ReadKey) -> Option<&str> {
        self.shown
            .get(path)?
            .iter()
            .find(|shown| &shown.key == key)
            .map(|shown| shown.call_id.as_str())
    }

    /// Records that call `call_id` returned the read `key` of `path`; it counts
    /// as shown once settled with the digest of the delivered text.
    pub fn offer_read(&mut self, call_id: &str, path: &str, key: ReadKey) {
        self.offered
            .insert(call_id.to_string(), (path.to_string(), key));
    }

    /// The path an offered read call targeted, if `call_id` made one.
    pub fn offered_path(&self, call_id: &str) -> Option<&str> {
        self.offered.get(call_id).map(|(path, _)| path.as_str())
    }

    /// Withdraws an offered read, e.g. because its result was clipped.
    pub fn withdraw_offer(&mut self, call_id: &str) {
        self.offered.remove(call_id);
    }

    /// Turns the offered read of `call_id` into a shown read whose result
    /// text has `digest`.
    pub fn settle_read(&mut self, call_id: &str, digest: &str) {
        if let Some((path, key)) = self.offered.remove(call_id) {
            let entries = self.shown.entry(path).or_default();
            entries.retain(|shown| shown.key != key);
            entries.push(ShownRead {
                key,
                call_id: call_id.to_string(),
                digest: digest.to_string(),
            });
        }
    }

    /// Ends a batch: forgets per-call windows.
    pub fn settle_windows(&mut self) {
        self.windows.clear();
    }

    /// Keeps only the shown reads whose result is still in the conversation
    /// with the same text. `results` maps call ids to the digest of the result
    /// text the conversation holds.
    pub fn keep_shown(&mut self, results: &HashMap<String, String>) {
        self.shown.retain(|_, entries| {
            entries.retain(|shown| results.get(&shown.call_id) == Some(&shown.digest));
            !entries.is_empty()
        });
    }

    /// Narrows the coverage a read call contributed to the lines its
    /// delivered (possibly clipped) result still shows whole. Coverage that
    /// predates the batch and other calls' shares are kept.
    ///
    /// # Arguments
    /// * `path` - Ledger key of the file the call read.
    /// * `call_id` - The read call whose result was clipped.
    /// * `delivered` - Lines its delivered result still shows whole.
    pub fn record_delivered(&mut self, path: &str, call_id: &str, delivered: Vec<LineSpan>) {
        self.offered.remove(call_id);
        let Some(window) = self.windows.get_mut(path) else {
            return;
        };
        if !window.reads.contains_key(call_id) {
            return;
        }
        window
            .reads
            .insert(call_id.to_string(), (merge_ranges(delivered), false));
        let mut all = window.base_ranges.clone();
        for (ranges, _) in window.reads.values() {
            all.extend(ranges.iter().copied());
        }
        let whole = window.base_whole || window.reads.values().any(|(_, whole)| *whole);
        if let Some(coverage) = self.files.get_mut(path) {
            coverage.ranges = merge_ranges(all);
            coverage.whole = whole;
        }
    }
}

/// Merges overlapping and adjacent spans; drops inverted ones.
pub fn merge_ranges(mut ranges: Vec<LineSpan>) -> Vec<LineSpan> {
    ranges.retain(|(start, end)| start <= end);
    ranges.sort_unstable();
    let mut merged: Vec<LineSpan> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Whether `ranges` (merged) cover every line from `start` to `end`.
pub fn covers(ranges: &[LineSpan], start: u64, end: u64) -> bool {
    let mut cursor = start;
    for &(seen_start, seen_end) in ranges {
        if seen_end < cursor {
            continue;
        }
        if seen_start > cursor {
            return false;
        }
        cursor = cursor.max(seen_end + 1);
        if cursor > end {
            return true;
        }
    }
    cursor > end
}

/// Moves `ranges` through an edit that replaced lines `start..=end` and
/// changed the line count by `delta`. Lines before the edit keep their
/// numbers, lines after it shift by `delta`, and the edited region counts as
/// seen.
pub fn transform_ranges(ranges: &[LineSpan], start: u64, end: u64, delta: i64) -> Vec<LineSpan> {
    let shift = |line: u64| (line as i64 + delta).max(1) as u64;
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for &(seen_start, seen_end) in ranges {
        if seen_start < start {
            out.push((seen_start, seen_end.min(start - 1)));
        }
        if seen_end > end {
            out.push((shift(seen_start.max(end + 1)), shift(seen_end)));
        }
    }
    let region_end = (end as i64 + delta).max(start as i64) as u64;
    out.push((start, region_end));
    merge_ranges(out)
}

/// Renders spans as `a-b, c`.
pub fn render_spans(spans: &[LineSpan]) -> String {
    spans
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first 16 characters of a content hash, as shown in messages.
pub fn short_hash(hash: &str) -> &str {
    hash.get(..16).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn key(hash: &str, start: u64, end: u64) -> ReadKey {
        ReadKey { hash: hash.to_string(), start, end, numbered: true }
    }

    #[test]
    fn test_partial_reads_merge_and_promote_to_whole() {
        let mut fixture = ReadLedger::default();
        fixture.record_read("/a", "h1", Some((1, 5)), Some(10), false, Some("c1"));
        fixture.record_read("/a", "h1", Some((6, 10)), Some(10), false, Some("c2"));
        let actual = fixture.coverage("/a").cloned().unwrap();
        let expected = FileCoverage {
            hash: "h1".to_string(),
            ranges: vec![(1, 10)],
            total_lines: Some(10),
            whole: true,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_hash_change_drops_old_coverage() {
        let mut fixture = ReadLedger::default();
        fixture.record_read("/a", "h1", Some((1, 5)), Some(10), false, None);
        fixture.record_read("/a", "h2", Some((7, 8)), Some(10), false, None);
        let actual = fixture.coverage("/a").unwrap().ranges.clone();
        let expected = vec![(7, 8)];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_check_messages() {
        let mut fixture = ReadLedger::default();
        fixture.record_read(
            "/a",
            "0123456789abcdefXX",
            Some((1, 3)),
            Some(9),
            false,
            None,
        );
        let actual = (
            fixture
                .check("/b", "edit it", None, &[], None)
                .unwrap_err()
                .to_string(),
            fixture
                .check("/a", "edit it", Some("ffffffffffffffffZZ"), &[], None)
                .unwrap_err()
                .to_string(),
            fixture
                .check("/a", "edit it", None, &[(2, 2), (5, 6), (8, 8)], None)
                .unwrap_err()
                .to_string(),
            fixture
                .check("/a", "edit it", None, &[(2, 3)], None)
                .is_ok(),
        );
        let expected = (
            "You must read the file with the read tool before attempting to edit it.".to_string(),
            "/a changed since it was read (hash 0123456789abcdef when read, ffffffffffffffff now); read it again before attempting to edit it.".to_string(),
            "You must read the target lines (5-6, 8) before attempting to edit it.".to_string(),
            true,
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_transform_ranges_shifts_after_and_marks_region() {
        let actual = transform_ranges(&[(1, 10)], 3, 3, 2);
        let expected = vec![(1, 12)];
        assert_eq!(actual, expected);
        let actual = transform_ranges(&[(1, 2), (8, 9)], 4, 5, -1);
        let expected = vec![(1, 2), (4, 4), (7, 8)];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_shown_reads_settle_and_keep() {
        let mut fixture = ReadLedger::default();
        fixture.offer_read("c1", "/a", key("h", 1, 3));
        fixture.settle_read("c1", "d1");
        assert_eq!(fixture.shown_read("/a", &key("h", 1, 3)), Some("c1"));

        let mut results = HashMap::new();
        results.insert("c1".to_string(), "other".to_string());
        fixture.keep_shown(&results);
        let actual = fixture.shown_read("/a", &key("h", 1, 3));
        assert_eq!(actual, None);
    }

    #[test]
    fn test_record_delivered_narrows_only_that_call() {
        let mut fixture = ReadLedger::default();
        fixture.record_read("/a", "h", Some((1, 100)), Some(200), false, Some("c1"));
        fixture.record_read("/a", "h", Some((150, 160)), Some(200), false, Some("c2"));
        fixture.record_delivered("/a", "c1", vec![(1, 40)]);
        let actual = fixture.coverage("/a").unwrap().ranges.clone();
        let expected = vec![(1, 40), (150, 160)];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ledger_round_trips_through_json() {
        let mut fixture = ReadLedger::default();
        fixture.record_read("/a", "h", Some((1, 2)), Some(5), false, Some("c1"));
        fixture.offer_read("c1", "/a", key("h", 1, 2));
        fixture.settle_read("c1", "d");
        let json = serde_json::to_string(&fixture).unwrap();
        let mut actual: ReadLedger = serde_json::from_str(&json).unwrap();
        actual.settle_windows();
        fixture.settle_windows();
        assert_eq!(actual, fixture);
    }
}
