//! Structural search and rewrite for the `ast_search` tool (T8), in-process
//! through `ast-grep-core` and the tree-sitter grammars of
//! `ast-grep-language` (no `ast-grep` binary).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, bail};
use ast_grep_core::{Doc, Language, Node, Pattern};
use ast_grep_language::{LanguageExt, SupportLang};
use bstr::ByteSlice;

/// Default and upper bound on reported matches.
pub const DEFAULT_MAX_RESULTS: usize = 200;

/// One match with its metavariable captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstMatch {
    pub path: PathBuf,
    /// 1-based line of the match start.
    pub line: usize,
    /// Source lines the match spans.
    pub text: String,
    /// `$NAME` captures and `$$$NAME` multi-captures, by name.
    pub captures: BTreeMap<String, String>,
}

/// The result of a search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AstResult {
    pub matches: Vec<AstMatch>,
    /// More matches existed than `max_results`.
    pub overflow: bool,
    /// For a rewrite: per file, the text before and after.
    pub rewrites: BTreeMap<PathBuf, (String, String)>,
}

/// Resolves a language name (`rust`, `Rust`, `rs`, `py`, `C`, `Cpp`, …).
///
/// # Errors
/// Returns an error naming the unsupported language.
pub fn language(name: &str) -> anyhow::Result<SupportLang> {
    SupportLang::from_str(&name.to_lowercase())
        .map_err(|_| anyhow::anyhow!("unsupported ast-grep language: {name}"))
}

/// Files under `root` (or `root` itself) whose grammar is `lang`, or any
/// supported grammar when `lang` is `None`. Honours gitignore and skips
/// hidden paths.
fn files(root: &Path, lang: Option<SupportLang>) -> Vec<(PathBuf, SupportLang)> {
    if root.is_file() {
        let detected = lang.or_else(|| SupportLang::from_path(root));
        return detected
            .map(|l| vec![(root.to_path_buf(), l)])
            .unwrap_or_default();
    }
    let mut walk = ignore::WalkBuilder::new(root);
    walk.require_git(false).sort_by_file_path(|a, b| a.cmp(b));
    walk.build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let detected = SupportLang::from_path(entry.path())?;
            match lang {
                Some(wanted) if wanted != detected => None,
                _ => Some((entry.into_path(), detected)),
            }
        })
        .collect()
}

fn captures<D: Doc>(
    node: &ast_grep_core::NodeMatch<'_, D>,
    pattern: &str,
) -> BTreeMap<String, String> {
    let env = node.get_env();
    let mut out = BTreeMap::new();
    for name in metavariable_names(pattern) {
        if let Some(single) = env.get_match(&name) {
            out.insert(format!("${name}"), single.text().to_string());
        } else {
            let multi: Vec<String> = env
                .get_multiple_matches(&name)
                .iter()
                .map(|n: &Node<'_, D>| n.text().to_string())
                .collect();
            if !multi.is_empty() {
                out.insert(format!("$$${name}"), multi.join(", "));
            }
        }
    }
    out
}

/// Metavariable names a pattern uses (`$A`, `$$$ARGS`).
fn metavariable_names(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut names = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i;
            while j < bytes.len() && bytes[j] == b'$' {
                j += 1;
            }
            let start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit() || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > start {
                let name = pattern[start..j].to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    names
}

/// Searches (and, with `rewrite`, computes the rewritten files).
///
/// # Arguments
/// * `pattern` - ast-grep pattern.
/// * `root` - File or directory.
/// * `lang` - Language override; inferred per file when `None`.
/// * `rewrite` - Replacement template.
/// * `max_results` - Matches reported (one extra is looked for to detect
///   overflow).
///
/// # Errors
/// Returns an error for a missing root, an unknown language, or a pattern
/// that does not parse as the language.
pub fn search(
    pattern: &str,
    root: &Path,
    lang: Option<&str>,
    rewrite: Option<&str>,
    max_results: usize,
) -> anyhow::Result<AstResult> {
    if pattern.trim().is_empty() {
        bail!("pattern is required");
    }
    if !root.exists() {
        bail!("Path does not exist: {}", root.display());
    }
    let lang = lang.map(language).transpose()?;
    let mut result = AstResult::default();
    let mut patterns: BTreeMap<String, Pattern> = BTreeMap::new();
    for (path, file_lang) in files(root, lang) {
        let key = file_lang.to_string();
        if !patterns.contains_key(&key) {
            let compiled = Pattern::try_new(pattern, file_lang)
                .map_err(|e| anyhow::anyhow!("pattern does not parse as {file_lang}: {e}"))?;
            patterns.insert(key.clone(), compiled);
        }
        let compiled = &patterns[&key];
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let grep = file_lang.ast_grep(&source);
        let lines: Vec<&str> = source.lines().collect();
        let found: Vec<_> = grep.root().find_all(compiled).collect();
        if found.is_empty() {
            continue;
        }
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for node in &found {
            if result.matches.len() >= max_results {
                result.overflow = true;
                break;
            }
            let start = node.start_pos().line();
            let end = node.end_pos().line();
            result.matches.push(AstMatch {
                path: path.clone(),
                line: start + 1,
                text: lines
                    .get(start..=end.min(lines.len().saturating_sub(1)))
                    .map(|l| l.join("\n"))
                    .unwrap_or_default(),
                captures: captures(node, pattern),
            });
            if let Some(template) = rewrite {
                let edit = node.replace_by(template);
                edits.push((
                    edit.position,
                    edit.deleted_length,
                    edit.inserted_text.to_str_lossy().into_owned(),
                ));
            }
        }
        if !edits.is_empty() {
            let mut updated = source.clone();
            for (position, deleted, inserted) in edits.iter().rev() {
                updated.replace_range(*position..position + deleted, inserted);
            }
            result
                .rewrites
                .insert(path.clone(), (source.clone(), updated));
        }
        if result.overflow {
            break;
        }
    }
    Ok(result)
}

/// Renders matches as `path:line` headings followed by numbered source lines
/// and capture lines.
pub fn render_matches(result: &AstResult) -> String {
    if result.matches.is_empty() {
        return "(no matches)".to_string();
    }
    let mut blocks = Vec::new();
    for m in &result.matches {
        let width = (m.line + m.text.lines().count()).to_string().len();
        let mut block = format!("{}:{}", m.path.display(), m.line);
        for (offset, line) in m.text.lines().enumerate() {
            block.push_str(&format!("\n{:>width$}|{line}", m.line + offset));
        }
        for (name, value) in &m.captures {
            block.push_str(&format!("\n  {name} = {value}"));
        }
        blocks.push(block);
    }
    let mut out = blocks.join("\n");
    if result.overflow {
        out.push_str("\n[additional matches omitted; increase max_results to see them]");
    }
    out
}

/// Unified diff of every rewritten file.
pub fn render_diff(rewrites: &BTreeMap<PathBuf, (String, String)>) -> String {
    let diff: String = rewrites
        .iter()
        .filter(|(_, (before, after))| before != after)
        .map(|(path, (before, after))| {
            similar::TextDiff::from_lines(before, after)
                .unified_diff()
                .context_radius(3)
                .header(&path.display().to_string(), &path.display().to_string())
                .to_string()
        })
        .collect();
    if diff.is_empty() {
        "(no changes)".to_string()
    } else {
        diff
    }
}

/// Writes rewritten files.
///
/// # Errors
/// Returns the first write error.
pub fn write_rewrites(rewrites: &BTreeMap<PathBuf, (String, String)>) -> anyhow::Result<usize> {
    let mut changed = 0;
    for (path, (before, after)) in rewrites {
        if before == after {
            continue;
        }
        std::fs::write(path, after)
            .with_context(|| format!("could not write {}", path.display()))?;
        changed += 1;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn main() {\n    foo(1, 2);\n    // foo(9)\n    let s = \"foo(3)\";\n    foo(\n        4,\n    );\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.py"), "foo(5)\n").unwrap();
        dir
    }

    #[test]
    fn test_matches_skip_comments_and_strings_and_capture() {
        let dir = fixture();
        let actual = search("foo($A, $$$REST)", dir.path(), Some("rust"), None, 50).unwrap();
        // `$$$REST` also matches zero arguments, so the multi-line `foo(4,)`
        // at line 5 matches; the comment (3) and the string (4) do not.
        let lines: Vec<usize> = actual.matches.iter().map(|m| m.line).collect();
        assert_eq!(lines, vec![2, 5]);
        assert_eq!(actual.matches[0].captures.get("$A"), Some(&"1".to_string()));
        assert_eq!(
            actual.matches[0].captures.get("$$$REST"),
            Some(&"2".to_string())
        );
    }

    #[test]
    fn test_multiline_match_and_language_inference() {
        let dir = fixture();
        let actual = search("foo($A)", dir.path(), None, None, 50).unwrap();
        let found: Vec<(String, usize)> = actual
            .matches
            .iter()
            .map(|m| {
                (
                    m.path.file_name().unwrap().to_string_lossy().into_owned(),
                    m.line,
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![("a.rs".to_string(), 5), ("b.py".to_string(), 1)]
        );
        let rendered = render_matches(&actual);
        assert!(
            rendered.contains("5|    foo(\n6|        4,\n7|    );"),
            "{rendered}"
        );
    }

    #[test]
    fn test_rewrite_dry_run_then_write() {
        let dir = fixture();
        let actual = search(
            "foo($A, $B)",
            dir.path(),
            Some("rust"),
            Some("bar($B, $A)"),
            50,
        )
        .unwrap();
        let diff = render_diff(&actual.rewrites);
        assert!(diff.contains("-    foo(1, 2);\n+    bar(2, 1);"), "{diff}");
        assert!(
            std::fs::read_to_string(dir.path().join("a.rs"))
                .unwrap()
                .contains("foo(1, 2)")
        );

        let changed = write_rewrites(&actual.rewrites).unwrap();
        assert_eq!(changed, 1);
        assert!(
            std::fs::read_to_string(dir.path().join("a.rs"))
                .unwrap()
                .contains("bar(2, 1)")
        );
    }

    #[test]
    fn test_overflow_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.py"), "f(1)\nf(2)\nf(3)\n").unwrap();
        let actual = search("f($A)", dir.path(), None, None, 2).unwrap();
        assert_eq!((actual.matches.len(), actual.overflow), (2, true));
    }

    #[test]
    fn test_unknown_language_is_an_error() {
        let dir = fixture();
        let actual = search("x", dir.path(), Some("cobol"), None, 5)
            .unwrap_err()
            .to_string();
        assert_eq!(actual, "unsupported ast-grep language: cobol");
    }
}
