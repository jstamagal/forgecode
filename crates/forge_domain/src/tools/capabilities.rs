//! Inputs of the phase 7 capability tools (T11–T19, T25). Every one of them
//! is lazy: loaded through `tool_discovery`.

use eserde::Deserialize;
use forge_tool_macros::ToolDescription;
use schemars::JsonSchema;
use serde::Serialize;

use crate::ToolDescription;

fn is_false(value: &bool) -> bool {
    !*value
}

/// T15: drive a persistent terminal.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/terminal_session.md"]
pub struct TerminalSession {
    /// start, send, look, stop or list.
    pub action: String,
    /// Session name (default "main").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Command to run (start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Comma-separated keys to send (send).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,
    /// Working directory (start; default the current directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Milliseconds to wait for output before reading the screen (default
    /// 700, at most 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 10000))]
    pub wait_ms: Option<u64>,
    /// Columns (start only; default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 400))]
    pub cols: Option<u16>,
    /// Rows (start only; default 40).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    pub rows: Option<u16>,
}

/// T15: render a terminal session's screen as a PNG.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/terminal_snapshot.md"]
pub struct TerminalSnapshot {
    /// Session name (default "main").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Milliseconds to wait for output first (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 10000))]
    pub wait_ms: Option<u64>,
}

/// T18: language-server queries.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/lsp.md"]
pub struct Lsp {
    /// diagnostics, definition, references or hover.
    pub operation: String,
    /// File to query.
    pub file_path: String,
    /// 1-based line (definition, references, hover).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub line: Option<u32>,
    /// 1-based column (definition, references, hover).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub character: Option<u32>,
}

/// T16: persistent Python kernel.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/kernel.md"]
pub struct Kernel {
    /// run (default), poll, wait, interrupt, restart or kill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Python code to run (run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Seconds a run (or wait) blocks before returning a handle (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub timeout: Option<u64>,
    /// Handle of a run that is still going (poll, wait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

/// T17: the kernel's persistent function library.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/toolbox.md"]
pub struct Toolbox {
    /// list (default), show, save, load, history or delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Function name (show, save, load, history, delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Function source (save): one top-level `def`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What the revision changed (save).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// global (default) or project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Revision to show or load (default the latest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub revision: Option<u32>,
}

/// T13: render a page in a real browser engine (obscura).
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/browse.md"]
pub struct Browse {
    /// http(s):// or file:// URL.
    pub url: String,
    /// markdown (default), text, html, links, original, assets or cookies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dump: Option<String>,
    /// Write a PNG screenshot to this path instead of returning the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

/// T14: drive a page in headless Chromium and capture frames.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/browser_probe.md"]
pub struct BrowserProbe {
    /// URL or local HTML file to open.
    pub target: String,
    /// CSS selector to click after load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click: Option<String>,
    /// Comma-separated keys to press after load (Playwright key names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press: Option<String>,
    /// Directory for the frames (default a fresh directory under
    /// tool-results).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// Milliseconds to wait after load (default 1200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u64>,
    /// Milliseconds between the two frames after the action (default 1600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u64>,
    /// Viewport width (default 1280).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_width: Option<u32>,
    /// Viewport height (default 800).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_height: Option<u32>,
}

/// T11: web search through Serper (Google results).
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/serper_search.md"]
pub struct SerperSearch {
    pub query: String,
    /// Results (default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub num: Option<u32>,
}

/// T11: web search through Tavily.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/tavily_search.md"]
pub struct TavilySearch {
    pub query: String,
    /// Results (default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 20))]
    pub max_results: Option<u32>,
}

/// T11: semantic web search through Exa.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/exa_search.md"]
pub struct ExaSearch {
    /// Describe what you want in a full sentence.
    pub query: String,
    /// Results (default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub num: Option<u32>,
    /// Characters of page text per result (default 1500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 100))]
    pub text_chars: Option<u32>,
}

/// T12: library documentation through Context7.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/docs_search.md"]
pub struct DocsSearch {
    /// Library name, e.g. "tokio" or "react".
    pub library: String,
    /// What to look up in the docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Tokens of documentation to return (default 4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 500))]
    pub tokens: Option<u32>,
}

/// T19: edit one cell of a Jupyter notebook.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/notebook_edit.md"]
pub struct NotebookEdit {
    /// The `.ipynb` file.
    pub file_path: String,
    /// The cell to edit or insert after: its id, or `index:N` (0-based).
    /// Omit with `insert` to insert at the top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// New cell source (replace, insert).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_source: Option<String>,
    /// code or markdown (insert; replace keeps the type when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_type: Option<String>,
    /// replace (default), insert or delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_mode: Option<String>,
}

/// T25: convert one file for a wiki vault.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/wiki_convert.md"]
pub struct WikiConvert {
    /// File to convert.
    pub file_path: String,
    /// Vault directory media is copied into (`<vault>/assets/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
}

/// T25: create or replace a wiki page.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/wiki_write.md"]
pub struct WikiWrite {
    /// Existing vault directory.
    pub vault: String,
    /// source, entity, concept or synthesis.
    pub kind: String,
    /// Page title.
    pub title: String,
    /// Page body (markdown, wikilinks welcome).
    pub body: String,
    /// Tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Replace an existing page.
    #[serde(default, skip_serializing_if = "is_false")]
    pub overwrite: bool,
}

/// T25: archive an ingested unit, log it and commit the vault.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/wiki_finish_ingest.md"]
pub struct WikiFinishIngest {
    /// Existing vault directory.
    pub vault: String,
    /// The ingested file under `<vault>/raw/` to archive.
    pub source: String,
    /// What was added, or why the unit was skipped.
    pub note: String,
}
