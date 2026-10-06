//! Local text-replacement engine behind the `patch` tool.
//!
//! An edit first looks for `old_string` exactly. When there is no exact
//! occurrence it looks again in a normalised view of both texts, where smart
//! quotes, Unicode dashes, no-break and typographic spaces read as ASCII and
//! each line's trailing whitespace is dropped. A normalised match is applied
//! so that every byte the edit left unchanged keeps its original form. Nothing
//! here performs IO or calls a remote service.

use similar::{Algorithm, DiffTag, capture_diff_slices};
use thiserror::Error;

/// Line separators other than LF, CRLF and CR. Text containing them is
/// refused because line numbers and endings would become ambiguous.
const UNSUPPORTED_SEPARATORS: [char; 8] = [
    '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}',
];

/// Why a single edit could not be applied.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MatchError {
    #[error("unsupported line separator; patch requires LF, CRLF, or CR lines")]
    UnsupportedSeparator,
    #[error("old_string cannot be empty")]
    EmptyOld,
    #[error("old_string and new_string must be different")]
    SameText,
    #[error(
        "old_string and new_string are identical after line-ending normalization, so this edit would not change the file"
    )]
    IdenticalAfterNormalization,
    #[error("Could not find match for search text: {old:?}.{hint}")]
    NotFound { old: String, hint: String },
    #[error(
        "Multiple matches found for search text: {old:?}. Either provide a more specific search pattern or use replace_all."
    )]
    MultipleMatches { old: String },
    #[error(
        "old_string matched only after normalising quotes, dashes and whitespace, and new_string differs from it only there, so this edit would not change the file"
    )]
    FuzzyNoop,
}

/// Error for a `patch` call, naming the failing edit in batch form.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatchError {
    /// Failure of the only edit of a scalar call.
    #[error("{0}")]
    Single(MatchError),
    /// Failure of edit `index` (1-based) of a batch call.
    #[error("edit {index}: {source}")]
    Batch { index: usize, source: MatchError },
}

/// One replacement request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest<'a> {
    pub old_string: &'a str,
    pub new_string: &'a str,
    pub replace_all: bool,
}

/// Result of applying one edit to a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    /// The text after the edit.
    pub updated: String,
    /// Number of replaced occurrences.
    pub count: usize,
    /// Whether the match needed the normalised fallback.
    pub fuzzy: bool,
    /// 1-based inclusive line spans, in the text before this edit, of every
    /// replaced occurrence.
    pub line_spans: Vec<(usize, usize)>,
    /// Change in line-break count caused by each replacement.
    pub line_delta: isize,
}

/// Result of applying a whole batch of edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome {
    /// The text after every edit.
    pub updated: String,
    /// Replacement count: occurrences for `replace_all` edits, one otherwise.
    pub replacements: usize,
    /// Number of edits that needed the normalised fallback.
    pub fuzzy_edits: usize,
    /// Per edit, the outcome details in order.
    pub edits: Vec<EditOutcome>,
}

/// Applies `edits` in order to `source`, each seeing the previous result.
/// All-or-nothing: the first failing edit aborts the whole batch.
///
/// # Arguments
/// * `source` - The current file text.
/// * `edits` - Edits to apply in order.
/// * `batch` - Whether the call used the `edits` form (controls error labels).
///
/// # Errors
/// Returns [`PatchError`] naming the failing edit.
pub fn apply_edits(
    source: &str,
    edits: &[EditRequest<'_>],
    batch: bool,
) -> Result<BatchOutcome, PatchError> {
    let label = |index: usize, source: MatchError| {
        if batch {
            PatchError::Batch { index: index + 1, source }
        } else {
            PatchError::Single(source)
        }
    };

    for (index, edit) in edits.iter().enumerate() {
        if edit.old_string == edit.new_string {
            return Err(label(index, MatchError::SameText));
        }
    }

    let mut updated = source.to_string();
    let mut replacements = 0;
    let mut fuzzy_edits = 0;
    let mut outcomes = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let outcome = apply_edit(&updated, edit).map_err(|error| label(index, error))?;
        replacements += if edit.replace_all { outcome.count } else { 1 };
        fuzzy_edits += usize::from(outcome.fuzzy);
        updated = outcome.updated.clone();
        outcomes.push(outcome);
    }

    Ok(BatchOutcome { updated, replacements, fuzzy_edits, edits: outcomes })
}

/// Applies one edit to `text`.
///
/// # Errors
/// Returns [`MatchError`] when the edit cannot be applied unambiguously.
pub fn apply_edit(text: &str, edit: &EditRequest<'_>) -> Result<EditOutcome, MatchError> {
    if text
        .chars()
        .chain(edit.new_string.chars())
        .any(|c| UNSUPPORTED_SEPARATORS.contains(&c))
    {
        return Err(MatchError::UnsupportedSeparator);
    }
    let ending = detect_line_ending(text);
    let old = normalize_line_endings(edit.old_string, ending);
    let new = normalize_line_endings(edit.new_string, ending);
    if old.is_empty() {
        return Err(MatchError::EmptyOld);
    }
    if old == new {
        return Err(MatchError::IdenticalAfterNormalization);
    }

    let exact = count_overlapping(text, &old);
    let (positions, fuzzy) = if exact > 0 {
        if exact > 1 && !edit.replace_all {
            return Err(MatchError::MultipleMatches { old: edit.old_string.to_string() });
        }
        (find_positions(text, &old, edit.replace_all), false)
    } else {
        let view = FuzzyView::new(text);
        let needle = FuzzyView::new(&old).text;
        let found = count_overlapping(&view.text, &needle);
        if found == 0 {
            return Err(MatchError::NotFound {
                old: edit.old_string.to_string(),
                hint: nearest_hint(text, &old),
            });
        }
        if found > 1 && !edit.replace_all {
            return Err(MatchError::MultipleMatches { old: edit.old_string.to_string() });
        }
        let spans = find_positions(&view.text, &needle, edit.replace_all)
            .into_iter()
            .map(|(start, end)| (view.start[start], view.end[end - 1]))
            .collect::<Vec<_>>();
        (widen_over_whitespace(text, &old, spans), true)
    };

    let replacements = positions
        .iter()
        .map(|&(start, end)| {
            if fuzzy {
                keep_untouched(&text[start..end], &old, &new)
            } else {
                new.clone()
            }
        })
        .collect::<Vec<_>>();
    if fuzzy
        && positions
            .iter()
            .zip(&replacements)
            .all(|(&(start, end), replacement)| &text[start..end] == replacement)
    {
        return Err(MatchError::FuzzyNoop);
    }

    let index = LineIndex::new(text);
    let line_spans = positions
        .iter()
        .map(|&(start, end)| index.span(text, start, end))
        .collect();

    let mut updated = text.to_string();
    for (&(start, end), replacement) in positions.iter().zip(&replacements).rev() {
        updated.replace_range(start..end, replacement);
    }
    let line_delta = count_line_breaks(&new) as isize - count_line_breaks(&old) as isize;

    Ok(EditOutcome { updated, count: positions.len(), fuzzy, line_spans, line_delta })
}

/// Returns the dominant line ending of `text`: CRLF when present, CR when the
/// text uses only CR, LF otherwise.
pub fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else if text.contains('\r') && !text.contains('\n') {
        "\r"
    } else {
        "\n"
    }
}

/// Rewrites every LF, CRLF and CR in `text` to `target`.
pub fn normalize_line_endings(text: &str, target: &str) -> String {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    if target == "\n" {
        lf
    } else {
        lf.replace('\n', target)
    }
}

/// Counts line breaks, treating CRLF as one break and a lone CR as one.
pub fn count_line_breaks(text: &str) -> usize {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .filter(|&(i, b)| *b == b'\n' || (*b == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .count()
}

/// Splits `text` into lines, keeping each line's ending (LF, CRLF or CR).
pub fn split_lines_keep(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..=i]);
                i += 1;
                start = i;
            }
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                lines.push(&text[start..i + 2]);
                i += 2;
                start = i;
            }
            b'\r' => {
                lines.push(&text[start..=i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Byte offsets of line starts, for byte-offset to line-number lookups.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        let mut offset = 0;
        for line in split_lines_keep(text) {
            offset += line.len();
            starts.push(offset);
        }
        Self { starts }
    }

    /// 1-based line holding byte `offset`.
    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset).max(1)
    }

    /// 1-based inclusive lines touched by the byte range `start..end`. A
    /// trailing line ending belongs to the line it ends.
    fn span(&self, text: &str, start: usize, end: usize) -> (usize, usize) {
        let content = text[start..end].trim_end_matches(['\r', '\n']).len();
        let last = if content == 0 { start } else { start + content - 1 };
        let first_line = self.line_of(start);
        (first_line, self.line_of(last).max(first_line))
    }
}

/// Folds a character to the ASCII form the normalised view reads it as.
fn fold_char(c: char) -> Option<char> {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => Some('\''),
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => Some('"'),
        '\u{2010}'..='\u{2015}' | '\u{2212}' => Some('-'),
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => Some(' '),
        _ => None,
    }
}

/// A line split into its text, its trailing whitespace and its line ending.
fn line_parts(line: &str) -> (&str, &str, &str) {
    let content = line.trim_end_matches(['\r', '\n']);
    let body = content.trim_end();
    (body, &content[body.len()..], &line[content.len()..])
}

/// The normalised view of a text: trailing whitespace dropped from every
/// line and folded characters replaced by ASCII. For each byte of the view,
/// `start`/`end` give the byte range of the original text it stands for.
struct FuzzyView {
    text: String,
    start: Vec<usize>,
    end: Vec<usize>,
}

impl FuzzyView {
    fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut start = Vec::with_capacity(source.len());
        let mut end = Vec::with_capacity(source.len());
        let mut offset = 0;
        for line in split_lines_keep(source) {
            let (body, trailing, ending) = line_parts(line);
            for (i, c) in body.char_indices() {
                let at = offset + i;
                match fold_char(c) {
                    Some(folded) => {
                        text.push(folded);
                        start.push(at);
                        end.push(at + c.len_utf8());
                    }
                    None => {
                        text.push(c);
                        for k in 0..c.len_utf8() {
                            start.push(at + k);
                            end.push(at + k + 1);
                        }
                    }
                }
            }
            offset += body.len() + trailing.len();
            for (i, c) in ending.char_indices() {
                text.push(c);
                start.push(offset + i);
                end.push(offset + i + 1);
            }
            offset += ending.len();
        }
        Self { text, start, end }
    }
}

/// Counts occurrences of `needle`, overlapping ones included, so uniqueness
/// is judged on every possible match.
fn count_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut from = 0;
    while let Some(found) = haystack.get(from..).and_then(|rest| rest.find(needle)) {
        count += 1;
        let at = from + found;
        from = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
    }
    count
}

/// Non-overlapping occurrences of `needle`; only the first unless `all`.
fn find_positions(haystack: &str, needle: &str, all: bool) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut cursor = 0;
    while let Some(found) = haystack.get(cursor..).and_then(|rest| rest.find(needle)) {
        let start = cursor + found;
        let end = start + needle.len();
        positions.push((start, end));
        cursor = end;
        if !all {
            break;
        }
    }
    positions
}

/// Grows normalised match spans over the whitespace the view dropped from
/// `old`: the end over the trailing whitespace of an unterminated last line,
/// the start back over the whitespace of a whitespace-only first line. A span
/// never grows into its neighbour.
fn widen_over_whitespace(
    text: &str,
    old: &str,
    positions: Vec<(usize, usize)>,
) -> Vec<(usize, usize)> {
    let lines = split_lines_keep(old);
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return positions;
    };
    let (first_body, first_trailing, first_ending) = line_parts(first);
    let (_, last_trailing, last_ending) = line_parts(last);
    let back = if !first_ending.is_empty() && first_body.is_empty() {
        first_trailing.chars().count()
    } else {
        0
    };
    let forward = if last_ending.is_empty() {
        last_trailing.chars().count()
    } else {
        0
    };
    let blank = |c: char| c.is_whitespace() && c != '\r' && c != '\n';

    let mut widened: Vec<(usize, usize)> = Vec::with_capacity(positions.len());
    for (index, &(mut start, mut end)) in positions.iter().enumerate() {
        let floor = widened.last().map_or(0, |&(_, end)| end);
        let ceiling = positions.get(index + 1).map_or(text.len(), |&(start, _)| start);
        for _ in 0..back {
            match text[..start].chars().next_back() {
                Some(c) if start > floor && blank(c) => start -= c.len_utf8(),
                _ => break,
            }
        }
        for _ in 0..forward {
            match text[end..].chars().next() {
                Some(c) if end < ceiling && blank(c) => end += c.len_utf8(),
                _ => break,
            }
        }
        widened.push((start, end));
    }
    widened
}

/// The replacement for `original` (the file text a normalised match covered):
/// `new`, with every line and character the edit left as it was in `old`
/// taken from `original` byte for byte.
fn keep_untouched(original: &str, old: &str, new: &str) -> String {
    let mut original_lines = split_lines_keep(original);
    let old_lines = split_lines_keep(old);
    let new_lines = split_lines_keep(new);
    if original_lines.len() + 1 == old_lines.len() {
        // The whitespace-only last line of `old` matched no whitespace in the
        // file, so the match ends at the line ending before it.
        original_lines.push("");
    }
    if original_lines.len() != old_lines.len() {
        return new.to_string();
    }
    let mut out = String::with_capacity(new.len());
    for op in capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines) {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => original_lines[old_range]
                .iter()
                .for_each(|line| out.push_str(line)),
            DiffTag::Replace if old_range.len() == new_range.len() => {
                for (k, new_line) in new_lines[new_range].iter().enumerate() {
                    let i = old_range.start + k;
                    out.push_str(&keep_untouched_chars(
                        original_lines[i],
                        old_lines[i],
                        new_line,
                    ));
                }
            }
            _ => new_lines[new_range]
                .iter()
                .for_each(|line| out.push_str(line)),
        }
    }
    out
}

/// One line of `new`, with the characters it keeps from `old` taken from
/// `original`, the file line `old` matched. A kept character the file writes
/// in a non-ASCII form stays ASCII where the edit inserts that same ASCII
/// character right beside it, so lengthening an en dash to `--` gives `--`.
fn keep_untouched_chars(original: &str, old: &str, new: &str) -> String {
    let (orig_body, orig_trailing, orig_ending) = line_parts(original);
    let (old_body, old_trailing, old_ending) = line_parts(old);
    let (new_body, new_trailing, new_ending) = line_parts(new);
    let orig_chars: Vec<char> = orig_body.chars().collect();
    let old_chars: Vec<char> = old_body.chars().collect();
    let new_chars: Vec<char> = new_body.chars().collect();
    if orig_chars.len() != old_chars.len() {
        return new.to_string();
    }

    let ops: Vec<_> = capture_diff_slices(Algorithm::Myers, &old_chars, &new_chars)
        .iter()
        .map(|op| op.as_tag_tuple())
        .collect();
    let segment = |index: usize| -> String {
        ops.get(index)
            .map(|(_, _, range)| new_chars[range.clone()].iter().collect())
            .unwrap_or_default()
    };

    let mut body = String::with_capacity(new_body.len());
    for (index, (tag, old_range, new_range)) in ops.iter().enumerate() {
        if *tag != DiffTag::Equal {
            body.extend(&new_chars[new_range.clone()]);
            continue;
        }
        let mut kept: Vec<char> = orig_chars[old_range.clone()].to_vec();
        let before = if index > 0 {
            segment(index - 1)
        } else {
            String::new()
        };
        let after = segment(index + 1);
        let mut position = old_range.start;
        while position < old_range.end
            && orig_chars[position] != old_chars[position]
            && before.ends_with(old_chars[position])
        {
            kept[position - old_range.start] = old_chars[position];
            position += 1;
        }
        let mut position = old_range.end;
        while position > old_range.start
            && orig_chars[position - 1] != old_chars[position - 1]
            && after.starts_with(old_chars[position - 1])
        {
            kept[position - 1 - old_range.start] = old_chars[position - 1];
            position -= 1;
        }
        body.extend(kept);
    }

    let trailing = if old_trailing == new_trailing {
        orig_trailing
    } else {
        new_trailing
    };
    let ending = if old_ending == new_ending {
        orig_ending
    } else {
        new_ending
    };
    format!("{body}{trailing}{ending}")
}

/// The closest line of `text` to the first non-blank line of a missed
/// `old_string`, so a near-miss retry can anchor on the file as it is.
fn nearest_hint(text: &str, old: &str) -> String {
    let Some(first) = split_lines_keep(old)
        .into_iter()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return " Read the file again.".to_string();
    };
    let lines = split_lines_keep(text);
    let stripped: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
    let Some(best) = similar::get_close_matches(first, &stripped, 1, 0.6)
        .into_iter()
        .next()
    else {
        return " Nothing similar is in the file; read it again.".to_string();
    };
    match stripped.iter().position(|line| *line == best) {
        Some(index) => format!(
            " Closest line is {}: {:?}. Anchor old_string on the file as it is now.",
            index + 1,
            lines[index].trim_end_matches(['\r', '\n'])
        ),
        None => " Read the file again.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn edit<'a>(old: &'a str, new: &'a str) -> EditRequest<'a> {
        EditRequest { old_string: old, new_string: new, replace_all: false }
    }

    fn edit_all<'a>(old: &'a str, new: &'a str) -> EditRequest<'a> {
        EditRequest { old_string: old, new_string: new, replace_all: true }
    }

    #[test]
    fn test_exact_single_replacement() {
        let fixture = "alpha\nbeta\ngamma\n";
        let actual = apply_edit(fixture, &edit("beta", "delta")).unwrap();
        let expected = EditOutcome {
            updated: "alpha\ndelta\ngamma\n".to_string(),
            count: 1,
            fuzzy: false,
            line_spans: vec![(2, 2)],
            line_delta: 0,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_multiple_matches_refused_without_replace_all() {
        let fixture = "x = 1\nx = 1\n";
        let actual = apply_edit(fixture, &edit("x = 1", "x = 2")).unwrap_err();
        let expected = MatchError::MultipleMatches { old: "x = 1".to_string() };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_overlapping_occurrences_count_as_ambiguous() {
        let fixture = "ababa";
        let actual = apply_edit(fixture, &edit("aba", "x")).unwrap_err();
        let expected = MatchError::MultipleMatches { old: "aba".to_string() };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_replace_all_reports_every_line() {
        let fixture = "foo\nbar\nfoo\n";
        let actual = apply_edit(fixture, &edit_all("foo", "baz")).unwrap();
        let expected = EditOutcome {
            updated: "baz\nbar\nbaz\n".to_string(),
            count: 2,
            fuzzy: false,
            line_spans: vec![(1, 1), (3, 3)],
            line_delta: 0,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_crlf_file_matches_lf_old_string_and_keeps_crlf() {
        let fixture = "one\r\ntwo\r\nthree\r\n";
        let actual = apply_edit(fixture, &edit("one\ntwo", "uno\ndos\nextra"))
            .unwrap()
            .updated;
        let expected = "uno\r\ndos\r\nextra\r\nthree\r\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_smart_quotes_fallback_keeps_untouched_bytes() {
        let fixture = "let s = \u{201c}hello\u{201d}; // it\u{2019}s\n";
        let actual = apply_edit(
            fixture,
            &edit("let s = \"hello\"; // it's", "let s = \"world\"; // it's"),
        )
        .unwrap();
        let expected = EditOutcome {
            updated: "let s = \u{201c}world\u{201d}; // it\u{2019}s\n".to_string(),
            count: 1,
            fuzzy: true,
            line_spans: vec![(1, 1)],
            line_delta: 0,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_trailing_whitespace_and_nbsp_fallback() {
        let fixture = "fn a() {   \n    x\u{00a0}= 1;\n}\n";
        let actual = apply_edit(fixture, &edit("fn a() {\n    x = 1;", "fn a() {\n    x = 2;"))
            .unwrap()
            .updated;
        let expected = "fn a() {   \n    x\u{00a0}= 2;\n}\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dash_lengthened_to_ascii_stays_ascii() {
        let fixture = "range 1\u{2013}2\n";
        let actual = apply_edit(fixture, &edit("range 1-2", "range 1--2"))
            .unwrap()
            .updated;
        let expected = "range 1--2\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fuzzy_noop_is_refused() {
        let fixture = "say \u{201c}hi\u{201d}\n";
        let actual = apply_edit(fixture, &edit("say \"hi\"", "say \u{201c}hi\u{201d}")).unwrap_err();
        let expected = MatchError::FuzzyNoop;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_miss_reports_closest_line() {
        let fixture = "fn main() {\n    println!(\"hello world\");\n}\n";
        let actual = apply_edit(fixture, &edit("println!(\"hello wrld\");", "x"))
            .unwrap_err()
            .to_string();
        let expected = "Could not find match for search text: \"println!(\\\"hello wrld\\\");\". Closest line is 2: \"    println!(\\\"hello world\\\");\". Anchor old_string on the file as it is now.";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_miss_with_nothing_similar() {
        let fixture = "abc\n";
        let actual = apply_edit(fixture, &edit("zzzzzzzz", "y")).unwrap_err().to_string();
        let expected =
            "Could not find match for search text: \"zzzzzzzz\". Nothing similar is in the file; read it again.";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unsupported_separator_refused() {
        let fixture = "a\u{2028}b";
        let actual = apply_edit(fixture, &edit("a", "c")).unwrap_err();
        let expected = MatchError::UnsupportedSeparator;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_multi_line_span_and_delta() {
        let fixture = "1\n2\n3\n4\n";
        let actual = apply_edit(fixture, &edit("2\n3\n", "two\n")).unwrap();
        let expected = EditOutcome {
            updated: "1\ntwo\n4\n".to_string(),
            count: 1,
            fuzzy: false,
            line_spans: vec![(2, 3)],
            line_delta: -1,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_batch_applies_in_order() {
        let fixture = "a\nb\n";
        let actual = apply_edits(fixture, &[edit("a", "x"), edit("x\nb", "y")], true).unwrap();
        let expected = "y\n";
        assert_eq!(actual.updated, expected);
        assert_eq!((actual.replacements, actual.fuzzy_edits), (2, 0));
    }

    #[test]
    fn test_batch_failure_names_edit_index() {
        let fixture = "a\nb\n";
        let actual = apply_edits(fixture, &[edit("a", "x"), edit("missing", "y")], true)
            .unwrap_err()
            .to_string();
        let expected = "edit 2: Could not find match for search text: \"missing\". Nothing similar is in the file; read it again.";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_same_old_and_new_rejected_before_matching() {
        let fixture = "a\n";
        let actual = apply_edits(fixture, &[edit("a", "a")], false).unwrap_err();
        let expected = PatchError::Single(MatchError::SameText);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_split_lines_keep() {
        let fixture = "a\r\nb\rc\nd";
        let actual = split_lines_keep(fixture);
        let expected = vec!["a\r\n", "b\r", "c\n", "d"];
        assert_eq!(actual, expected);
    }
}
