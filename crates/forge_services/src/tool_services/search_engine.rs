//! In-process ripgrep-style search for the `fs_search` tool (T6).
//!
//! The walk uses the `ignore` crate (gitignore, `.ignore`, hidden files
//! skipped unless asked for, sorted by path, never leaving the filesystem the
//! root is on) and matching uses `grep-searcher`/`grep-regex`, the libraries
//! ripgrep is built from. Results are absolute paths; content lines are
//! `path:line:text`, with bytes that are not valid UTF-8 shown as `\xNN`.
//! There is no lossy cap here: a large result is spilled by the executor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};

/// Longest a search may run.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Entries returned when the call names no `head_limit`.
const DEFAULT_LIMIT: usize = 10_000;

/// What a search prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Matching lines.
    Content,
    /// Paths of files with a match.
    FilesWithMatches,
    /// Match counts per file.
    Count,
}

/// A resolved search request.
#[derive(Debug, Clone)]
pub struct SearchSpec {
    pub pattern: String,
    pub root: PathBuf,
    pub glob: Option<String>,
    pub file_type: Option<String>,
    pub mode: Mode,
    pub before: usize,
    pub after: usize,
    pub line_numbers: bool,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub head_limit: Option<usize>,
    pub offset: usize,
}

/// Escapes bytes that are not valid UTF-8 as `\xNN`.
pub fn escape_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bstr::ByteSlice::utf8_chunks(bytes) {
        out.push_str(chunk.valid());
        for byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02X}"));
        }
    }
    out
}

/// Whether a glob names a dot-leading component, i.e. asks for hidden paths.
fn asks_for_hidden(glob: Option<&str>) -> bool {
    glob.is_some_and(|glob| {
        glob.trim_start_matches('!')
            .split('/')
            .any(|part| part.starts_with('.') && part != "." && part != "..")
    })
}

/// Runs the search.
///
/// # Errors
/// Returns an error for a bad regex/glob/type, a missing root, a positive
/// glob combined with `file_type`, or a search running past
/// [`SEARCH_TIMEOUT`].
pub fn run(spec: &SearchSpec) -> anyhow::Result<String> {
    let meta = std::fs::metadata(&spec.root)
        .with_context(|| format!("Path does not exist: {}", spec.root.display()))?;
    if !meta.is_dir() && !meta.is_file() {
        bail!("not a regular file or directory: {}", spec.root.display());
    }
    let positive_glob = spec
        .glob
        .as_deref()
        .is_some_and(|g| !g.trim_start().starts_with('!'));
    if spec.file_type.is_some() && positive_glob {
        bail!(
            "file_type cannot be combined with a positive glob; the glob would apply and drop file_type. Pass one filter."
        );
    }
    if spec.head_limit == Some(0) {
        bail!("head_limit must be at least 1");
    }

    let mut matcher = RegexMatcherBuilder::new();
    matcher.case_insensitive(spec.case_insensitive);
    if spec.multiline {
        matcher.multi_line(true).dot_matches_new_line(true);
    }
    let matcher = matcher
        .build(&spec.pattern)
        .with_context(|| format!("Invalid regex pattern: {}", spec.pattern))?;

    let mut walk = ignore::WalkBuilder::new(&spec.root);
    walk.same_file_system(true)
        .hidden(!asks_for_hidden(spec.glob.as_deref()))
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    if let Some(glob) = &spec.glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&spec.root);
        overrides
            .add(glob)
            .with_context(|| format!("Invalid glob pattern: {glob}"))?;
        walk.overrides(overrides.build()?);
    }
    if let Some(file_type) = spec.file_type.as_deref().filter(|t| !t.trim().is_empty()) {
        let name = file_type.trim().trim_start_matches('.');
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        if types.definitions().iter().any(|def| def.name() == name) {
            types.select(name);
        } else {
            // A bare extension with no ripgrep type still filters by it.
            types.add(name, &format!("*.{name}"))?;
            types.select(name);
        }
        walk.types(types.build()?);
    }

    let mut searcher = SearcherBuilder::new();
    searcher
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .multi_line(spec.multiline);
    if spec.mode == Mode::Content {
        searcher
            .before_context(spec.before)
            .after_context(spec.after);
    }
    let mut searcher = searcher.build();

    let limit = spec.head_limit.unwrap_or(DEFAULT_LIMIT);
    let wanted = spec.offset + limit + 1;
    let started = Instant::now();
    let mut lines: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for entry in walk.build() {
        if started.elapsed() > SEARCH_TIMEOUT {
            bail!("search timed out after {}s", SEARCH_TIMEOUT.as_secs());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                unreadable.push(error.to_string());
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = absolute(entry.path());
        let shown = path.display().to_string();
        match spec.mode {
            Mode::FilesWithMatches => {
                let mut found = false;
                let result =
                    searcher.search_path(&matcher, entry.path(), FirstMatch { found: &mut found });
                if let Err(error) = result {
                    unreadable.push(format!("{shown}: {error}"));
                } else if found {
                    lines.push(shown);
                }
            }
            Mode::Count => {
                let mut count = 0usize;
                let result =
                    searcher.search_path(&matcher, entry.path(), Counter { count: &mut count });
                if let Err(error) = result {
                    unreadable.push(format!("{shown}: {error}"));
                } else if count > 0 {
                    lines.push(format!("{shown}:{count}"));
                }
            }
            Mode::Content => {
                let mut sink = Lines {
                    path: &shown,
                    line_numbers: spec.line_numbers,
                    out: &mut lines,
                    last_line: None,
                    context: spec.before + spec.after > 0,
                };
                if let Err(error) = searcher.search_path(&matcher, entry.path(), &mut sink) {
                    unreadable.push(format!("{shown}: {error}"));
                }
            }
        }
        if lines.len() >= wanted {
            break;
        }
    }

    let truncated = lines.len() > spec.offset + limit;
    let page: Vec<String> = lines.into_iter().skip(spec.offset).take(limit).collect();
    let mut out = if page.is_empty() {
        "(no matches)".to_string()
    } else {
        page.join("\n")
    };
    if truncated {
        out.push_str(&format!(
            "\n[more matches than head_limit={limit}; continue with offset={}]",
            spec.offset + limit
        ));
    }
    if !unreadable.is_empty() {
        let shown: Vec<&str> = unreadable.iter().take(5).map(String::as_str).collect();
        let more = unreadable.len().saturating_sub(5);
        out.push_str(&format!(
            "\n[not searched, could not read {} path(s): {}{}]",
            unreadable.len(),
            shown.join("; "),
            if more > 0 {
                format!("; and {more} more")
            } else {
                String::new()
            }
        ));
    }
    Ok(out)
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

struct FirstMatch<'a> {
    found: &'a mut bool,
}

impl Sink for FirstMatch<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.found = true;
        Ok(false)
    }
}

struct Counter<'a> {
    count: &'a mut usize,
}

impl Sink for Counter<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.count += 1;
        Ok(true)
    }
}

/// Content lines as ripgrep prints them: `path:N:text` for matches,
/// `path-N-text` for context, `--` between non-adjacent groups.
struct Lines<'a> {
    path: &'a str,
    line_numbers: bool,
    out: &'a mut Vec<String>,
    last_line: Option<u64>,
    context: bool,
}

impl Lines<'_> {
    fn push(&mut self, number: Option<u64>, bytes: &[u8], separator: char) {
        let text = escape_lossy(bytes.strip_suffix(b"\n").unwrap_or(bytes));
        let text = text.strip_suffix('\r').unwrap_or(&text).to_string();
        if self.context
            && let (Some(previous), Some(current)) = (self.last_line, number)
            && current > previous + 1
        {
            self.out.push("--".to_string());
        }
        self.last_line = number.or(self.last_line);
        let line = match number.filter(|_| self.line_numbers) {
            Some(n) => format!("{}{separator}{n}{separator}{text}", self.path),
            None => format!("{}{separator}{text}", self.path),
        };
        self.out.push(line);
    }
}

impl Sink for &mut Lines<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let start = mat.line_number();
        for (offset, line) in mat.bytes().split_inclusive(|b| *b == b'\n').enumerate() {
            self.push(start.map(|n| n + offset as u64), line, ':');
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.push(ctx.line_number(), ctx.bytes(), '-');
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn spec(root: &Path, pattern: &str, mode: Mode) -> SearchSpec {
        SearchSpec {
            pattern: pattern.to_string(),
            root: root.to_path_buf(),
            glob: None,
            file_type: None,
            mode,
            before: 0,
            after: 0,
            line_numbers: true,
            case_insensitive: false,
            multiline: false,
            head_limit: None,
            offset: 0,
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("b.py"), "def alpha():\n    pass\n").unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join(".hidden/c.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"alpha\x00\x01").unwrap();
        std::fs::write(dir.path().join("latin.txt"), b"alpha caf\xe9\n").unwrap();
        dir
    }

    #[test]
    fn test_content_lines_are_absolute_and_escaped() {
        let dir = fixture();
        let root = dir.path();
        let actual = run(&spec(root, "alpha", Mode::Content)).unwrap();
        let expected = [
            format!("{}:1:fn alpha() {{}}", root.join("a.rs").display()),
            format!("{}:1:def alpha():", root.join("b.py").display()),
            format!("{}:1:alpha caf\\xE9", root.join("latin.txt").display()),
        ]
        .join("\n");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_files_mode_skips_hidden_ignored_and_binary() {
        let dir = fixture();
        let actual = run(&spec(dir.path(), "alpha", Mode::FilesWithMatches)).unwrap();
        let expected = ["a.rs", "b.py", "latin.txt"]
            .iter()
            .map(|f| dir.path().join(f).display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dot_glob_asks_for_hidden() {
        let dir = fixture();
        let mut fixture = spec(dir.path(), "alpha", Mode::FilesWithMatches);
        fixture.glob = Some(".hidden/*.rs".to_string());
        let actual = run(&fixture).unwrap();
        let expected = dir.path().join(".hidden/c.rs").display().to_string();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_file_type_and_count_mode() {
        let dir = fixture();
        let mut fixture = spec(dir.path(), "fn", Mode::Count);
        fixture.file_type = Some("rust".to_string());
        let actual = run(&fixture).unwrap();
        let expected = format!("{}:2", dir.path().join("a.rs").display());
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_paging_footer_names_next_offset() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (1..=5).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.path().join("f.txt"), text).unwrap();
        let mut fixture = spec(dir.path(), "hit", Mode::Content);
        fixture.head_limit = Some(2);
        fixture.offset = 1;
        let actual = run(&fixture).unwrap();
        let file = dir.path().join("f.txt").display().to_string();
        let expected = format!(
            "{file}:2:hit 2\n{file}:3:hit 3\n[more matches than head_limit=2; continue with offset=3]"
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_context_lines_and_group_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nX\nb\nc\nd\ne\nX\n").unwrap();
        let mut fixture = spec(dir.path(), "X", Mode::Content);
        fixture.before = 1;
        let actual = run(&fixture).unwrap();
        let file = dir.path().join("f.txt").display().to_string();
        let expected = format!("{file}-1-a\n{file}:2:X\n--\n{file}-6-e\n{file}:7:X");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_positive_glob_with_file_type_is_rejected() {
        let dir = fixture();
        let mut fixture = spec(dir.path(), "alpha", Mode::Content);
        fixture.glob = Some("*.rs".to_string());
        fixture.file_type = Some("rust".to_string());
        let actual = run(&fixture).unwrap_err().to_string();
        assert!(
            actual.starts_with("file_type cannot be combined with a positive glob"),
            "{actual}"
        );
    }
}
