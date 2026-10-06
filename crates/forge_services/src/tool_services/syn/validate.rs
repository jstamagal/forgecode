//! Local post-edit syntax validation.
//!
//! The edited file is parsed with the tree-sitter grammar for its extension
//! (grammars come from `ast-grep-language`, compiled into the binary). Only
//! parse errors the edit introduced are reported: an error in the new text is
//! treated as pre-existing when the old text had an error of the same kind on
//! a line with identical content. Nothing here performs IO or calls a remote
//! service.

use std::collections::HashMap;
use std::path::Path;

use ast_grep_core::{Doc, Language, Node};
use ast_grep_language::{LanguageExt, SupportLang};
use forge_domain::SyntaxError;

/// Upper bound on reported errors, so a badly broken file does not flood the
/// tool result.
const MAX_REPORTED: usize = 20;

/// One parse error with the text of the line it sits on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParseError {
    line: usize,
    column: usize,
    kind: String,
    message: String,
    line_text: String,
}

/// Returns the grammar used to validate `path`, if any. Markdown and HTML are
/// excluded: their grammars accept nearly anything or flag template syntax.
fn language_for(path: &Path) -> Option<SupportLang> {
    let lang = SupportLang::from_path(path)?;
    match lang {
        SupportLang::Markdown | SupportLang::Html => None,
        lang => Some(lang),
    }
}

/// Collects error and missing nodes from a parse of `source`.
fn parse_errors(lang: SupportLang, source: &str) -> Vec<ParseError> {
    let root = lang.ast_grep(source);
    let lines: Vec<&str> = source.lines().collect();
    let mut errors = Vec::new();
    collect(&root.root(), &lines, &mut errors);
    errors
}

fn collect<D: Doc>(node: &Node<'_, D>, lines: &[&str], out: &mut Vec<ParseError>) {
    for node in node.dfs() {
        let missing = node.is_missing();
        if !node.is_error() && !missing {
            continue;
        }
        let position = node.start_pos();
        let line = position.line();
        let kind = node.kind().to_string();
        let message = if missing {
            format!("missing `{kind}`")
        } else {
            let snippet: String = node.text().chars().take(40).collect();
            let snippet = snippet.lines().next().unwrap_or_default().to_string();
            if snippet.trim().is_empty() {
                "syntax error".to_string()
            } else {
                format!("syntax error near `{}`", snippet.trim())
            }
        };
        out.push(ParseError {
            line: line + 1,
            column: position.column(&node) + 1,
            kind,
            message,
            line_text: lines.get(line).map(|l| l.trim().to_string()).unwrap_or_default(),
        });
    }
}

/// Returns the parse errors of `after` that are not already present in
/// `before`. A new file (`before` is `None`) reports all of its errors. Files
/// without a supported grammar report nothing.
///
/// # Arguments
/// * `path` - Path of the edited file; its extension selects the grammar.
/// * `before` - File text before the edit, when the file existed.
/// * `after` - File text after the edit.
pub fn introduced_errors(path: &Path, before: Option<&str>, after: &str) -> Vec<SyntaxError> {
    let Some(lang) = language_for(path) else {
        return Vec::new();
    };
    let mut known: HashMap<(String, String), usize> = HashMap::new();
    if let Some(before) = before {
        for error in parse_errors(lang, before) {
            *known.entry((error.kind, error.line_text)).or_default() += 1;
        }
    }
    parse_errors(lang, after)
        .into_iter()
        .filter(|error| {
            match known.get_mut(&(error.kind.clone(), error.line_text.clone())) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    false
                }
                _ => true,
            }
        })
        .take(MAX_REPORTED)
        .map(|error| SyntaxError {
            line: error.line as u32,
            column: error.column as u32,
            message: error.message,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_valid_rust_reports_nothing() {
        let actual = introduced_errors(Path::new("a.rs"), None, "fn main() { let x = 1; }\n");
        let expected: Vec<SyntaxError> = vec![];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_new_python_error_is_reported() {
        let actual = introduced_errors(
            Path::new("a.py"),
            Some("def f():\n    pass\n"),
            "def f(:\n    pass\n",
        );
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].line, 1);
    }

    #[test]
    fn test_pre_existing_error_is_not_reported_after_line_shift() {
        let before = "fn a() {}\nfn b( {}\n";
        let after = "// header\nfn a() {}\nfn b( {}\n";
        let actual = introduced_errors(Path::new("x.rs"), Some(before), after);
        let expected: Vec<SyntaxError> = vec![];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unsupported_extension_reports_nothing() {
        let actual = introduced_errors(Path::new("notes.txt"), None, "fn ( {{{");
        let expected: Vec<SyntaxError> = vec![];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_json_error_reported() {
        let actual = introduced_errors(Path::new("c.json"), Some("{\"a\": 1}\n"), "{\"a\": }\n");
        assert!(!actual.is_empty());
    }
}
