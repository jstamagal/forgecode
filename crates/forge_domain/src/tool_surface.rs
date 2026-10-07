//! The tool surface an agent publishes to the model (T24, X21, X29).
//!
//! An agent's `tools:` list is a chain of entries resolved against every tool
//! the process knows (native, agent-as-tool, MCP):
//!
//! - `name` or a glob (`fs_*`, `mcp_*`): the tool's default mode;
//! - `name:eager` / `name:lazy`: published from the start, or loadable through
//!   `tool_discovery`;
//! - `name:ban`: never on this agent's surface;
//! - `tag:read_only[:modifier]`: every tool whose calls can be read-only.
//!
//! Exact names and tags beat globs; within each pass the first entry that
//! matches decides. A tool no entry matches is not on the surface. Exact names
//! that match no tool are reported, never dropped silently.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use glob::Pattern;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ToolDefinition, ToolName};

/// Name of the discovery tool.
pub const TOOL_DISCOVERY: &str = "tool_discovery";

/// Native tools published from the start unless an agent says otherwise;
/// every other tool defaults to lazy.
pub const DEFAULT_EAGER_TOOLS: [&str; 14] = [
    "read",
    "write",
    "patch",
    "remove",
    "undo",
    "fs_search",
    "shell",
    "fetch",
    "task",
    "skill",
    "plan",
    "todo_write",
    "todo_read",
    TOOL_DISCOVERY,
];

/// Names agents used before the current catalog; resolved to the current name.
pub fn deprecated_alias(name: &str) -> Option<&'static str> {
    match name {
        "search" => Some("fs_search"),
        "Read" => Some("read"),
        "Write" => Some("write"),
        "Task" => Some("task"),
        _ => None,
    }
}

/// How a tool is offered to the model.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    /// Published on every request.
    Eager,
    /// Listed in the `tool_discovery` catalog; published once loaded.
    Lazy,
    /// Not on this agent's surface.
    Ban,
}

impl std::fmt::Display for ToolMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ToolMode::Eager => "eager",
            ToolMode::Lazy => "lazy",
            ToolMode::Ban => "ban",
        })
    }
}

/// Where a tool comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "server")]
pub enum ToolSource {
    Native,
    Agent,
    Mcp(String),
}

impl ToolSource {
    /// The catalog id prefix and source label of this tool.
    pub fn catalog_kind(&self) -> &'static str {
        match self {
            ToolSource::Native | ToolSource::Agent => "native",
            ToolSource::Mcp(_) => "mcp",
        }
    }
}

/// What the policy needs to know about one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolMeta {
    pub name: ToolName,
    /// Other names agents may use for it (e.g. the legacy MCP form).
    pub alternate_names: Vec<String>,
    /// Whether calls to the tool can be read-only (`tag:read_only`).
    pub read_only: bool,
    /// The mode a bare entry gives it.
    pub default_mode: ToolMode,
}

impl ToolMeta {
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.alternate_names.iter().map(String::as_str))
    }
}

/// One parsed `tools:` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEntry {
    /// The entry as written.
    pub text: String,
    /// Tool name, glob, or intrinsic tag.
    pub noun: Noun,
    /// Explicit modifier; `None` means the tool's default mode.
    pub modifier: Option<ToolMode>,
}

/// What an entry names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Noun {
    Name(String),
    Glob(String),
    /// `tag:read_only`.
    ReadOnly,
}

impl PolicyEntry {
    /// Parses `name`, `glob`, `name:eager|lazy|ban` or `tag:read_only[:mode]`.
    ///
    /// # Errors
    /// Returns an error naming the entry for an unknown modifier or tag.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Err("empty tools entry".to_string());
        }
        let parse_mode = |m: &str| match m.trim() {
            "eager" => Ok(ToolMode::Eager),
            "lazy" => Ok(ToolMode::Lazy),
            "ban" => Ok(ToolMode::Ban),
            other => Err(format!(
                "tools entry {text:?}: unknown modifier {other:?} (expected eager, lazy or ban)"
            )),
        };
        if let Some(rest) = text.strip_prefix("tag:") {
            let (tag, modifier) = match rest.split_once(':') {
                Some((tag, modifier)) => (tag.trim(), Some(parse_mode(modifier)?)),
                None => (rest.trim(), None),
            };
            if tag != "read_only" {
                return Err(format!(
                    "tools entry {text:?}: unknown tag {tag:?} (known: read_only)"
                ));
            }
            return Ok(Self { text, noun: Noun::ReadOnly, modifier });
        }
        // MCP tool names contain no ':', so the last ':' separates a modifier.
        let (noun, modifier) = match text.rsplit_once(':') {
            Some((noun, modifier)) => (noun.trim().to_string(), Some(parse_mode(modifier)?)),
            None => (text.clone(), None),
        };
        let noun = deprecated_alias(&noun).map(str::to_string).unwrap_or(noun);
        let noun = if noun.contains(['*', '?', '[']) {
            Noun::Glob(noun)
        } else {
            Noun::Name(noun)
        };
        Ok(Self { text, noun, modifier })
    }

    fn is_glob(&self) -> bool {
        matches!(self.noun, Noun::Glob(_))
    }

    fn matches(&self, tool: &ToolMeta) -> bool {
        match &self.noun {
            Noun::Name(name) => tool.names().any(|n| n == name),
            Noun::Glob(glob) => Pattern::new(glob)
                .map(|pattern| tool.names().any(|n| pattern.matches(n)))
                .unwrap_or(false),
            Noun::ReadOnly => tool.read_only,
        }
    }
}

/// The mode a tool resolved to and the entry that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub tool: ToolName,
    pub mode: Option<ToolMode>,
    pub decided_by: Option<String>,
}

/// A resolved `tools:` chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub decisions: Vec<Decision>,
    /// Entries that could not be parsed or name no known tool.
    pub warnings: Vec<String>,
}

impl Selection {
    /// The mode of `name`, if it is on the surface (eager or lazy).
    pub fn mode(&self, name: &str) -> Option<ToolMode> {
        self.decisions
            .iter()
            .find(|d| d.tool.as_str() == name)
            .and_then(|d| d.mode)
            .filter(|mode| *mode != ToolMode::Ban)
    }
}

/// Resolves an agent's `tools:` entries against every known tool.
///
/// # Arguments
/// * `entries` - The agent's entries as written.
/// * `tools` - Every tool the process knows.
pub fn select_tools(entries: &[String], tools: &[ToolMeta]) -> Selection {
    let mut warnings = Vec::new();
    let mut parsed = Vec::new();
    for raw in entries {
        match PolicyEntry::parse(raw) {
            Ok(entry) => parsed.push(entry),
            Err(error) => warnings.push(error),
        }
    }
    for entry in &parsed {
        if let Noun::Name(name) = &entry.noun
            && !tools.iter().any(|tool| entry.matches(tool))
        {
            warnings.push(format!(
                "tools entry {:?} matches no tool ({name})",
                entry.text
            ));
        }
    }
    let named: Vec<&PolicyEntry> = parsed.iter().filter(|e| !e.is_glob()).collect();
    let globs: Vec<&PolicyEntry> = parsed.iter().filter(|e| e.is_glob()).collect();
    let decisions = tools
        .iter()
        .map(|tool| {
            let rule = named
                .iter()
                .chain(globs.iter())
                .find(|entry| entry.matches(tool));
            Decision {
                tool: tool.name.clone(),
                mode: rule.map(|entry| entry.modifier.unwrap_or(tool.default_mode)),
                decided_by: rule.map(|entry| entry.text.clone()),
            }
        })
        .collect();
    Selection { decisions, warnings }
}

/// Per-conversation surface state, persisted with the conversation (X29).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceState {
    /// Catalog ids loaded through `tool_discovery` or activated by a skill.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub loaded: BTreeSet<String>,
    /// Skills a path-scoped reminder already offered.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub offered_skills: BTreeSet<String>,
}

impl SurfaceState {
    /// Whether the tool `name` was loaded.
    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded
            .iter()
            .any(|id| id.split_once(':').is_some_and(|(_, n)| n == name))
    }
}

/// Kind of a catalog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Native,
    Mcp,
    Skill,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::Native => "native",
            EntryKind::Mcp => "mcp",
            EntryKind::Skill => "skill",
        }
    }
}

/// One `tool_discovery` catalog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Stable id: `native:<name>`, `mcp:<name>`, `skill:<name>`, or a status
    /// row (`mcp:server:<server>`, `mcp:server-error:<server>`).
    pub id: String,
    pub name: String,
    pub kind: EntryKind,
    /// `native`, the MCP server name, or the skill source.
    pub source: String,
    pub description: String,
    /// Parameter names and descriptions, for search only.
    pub search_text: String,
    /// Status rows are not loadable.
    pub loadable: bool,
}

impl CatalogEntry {
    /// A loadable row for a tool definition.
    pub fn for_tool(definition: &ToolDefinition, source: &ToolSource) -> Self {
        let kind = match source {
            ToolSource::Mcp(_) => EntryKind::Mcp,
            _ => EntryKind::Native,
        };
        let source_label = match source {
            ToolSource::Mcp(server) => server.clone(),
            _ => "native".to_string(),
        };
        Self {
            id: format!("{}:{}", kind.as_str(), definition.name),
            name: definition.name.to_string(),
            kind,
            source: source_label,
            description: summary(&definition.description),
            search_text: schema_search_text(
                &serde_json::to_value(&definition.input_schema).unwrap_or_default(),
            ),
            loadable: true,
        }
    }

    /// Whether this row passes a `tool_discovery` `source` filter: its own
    /// source (any case), or `mcp` for every MCP row.
    pub fn matches_source(&self, source: &str) -> bool {
        self.source.eq_ignore_ascii_case(source)
            || (self.kind == EntryKind::Mcp && source.eq_ignore_ascii_case("mcp"))
    }
}

/// The first sentence-ish of a description, collapsed to one line.
pub fn summary(description: &str) -> String {
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = collapsed
        .char_indices()
        .take_while(|(i, _)| *i < 240)
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    collapsed[..end].to_string()
}

/// Property names and descriptions of a JSON schema, recursively.
fn schema_search_text(schema: &serde_json::Value) -> String {
    fn walk(node: &serde_json::Value, parts: &mut Vec<String>) {
        let Some(map) = node.as_object() else {
            return;
        };
        if let Some(text) = map.get("description").and_then(|d| d.as_str()) {
            parts.push(text.to_string());
        }
        if let Some(properties) = map.get("properties").and_then(|p| p.as_object()) {
            for (name, child) in properties {
                parts.push(name.clone());
                walk(child, parts);
            }
        }
        if let Some(items) = map.get("items") {
            walk(items, parts);
        }
    }
    let mut parts = Vec::new();
    walk(schema, &mut parts);
    parts.join(" ")
}

const STOP_WORDS: [&str; 74] = [
    "a", "about", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by", "can", "could",
    "did", "do", "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "into", "is",
    "it", "its", "me", "my", "no", "not", "of", "on", "or", "our", "should", "so", "some", "such",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "to", "up", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
    "with", "would", "you", "your", "yours", "us", "am", "very", "just",
];

/// Intent words that supplement a native tool's name.
fn native_intent(name: &str) -> &'static str {
    match name {
        "fetch" => "download url http web",
        "kernel" => "jupyter notebook python ipython",
        "patch" => "edit file modify replace patch",
        "write" => "write file create save overwrite",
        "fs_search" => "find files filename glob search directory grep",
        "ast_search" => "find code syntax structural search refactor",
        "task" => "spawn agent delegate worker subagent",
        "terminal_session" => "terminal shell interactive process tui",
        _ => "",
    }
}

fn stem(term: &str) -> String {
    let n = term.len();
    if n > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..n - 3]);
    }
    if n > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| term.ends_with(s))
    {
        return term[..n - 2].to_string();
    }
    if n > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..n - 1].to_string();
    }
    term.to_string()
}

/// Stemmed terms of `text`, split at camelCase, `_` and punctuation, stop
/// words dropped.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let chars: Vec<char> = word.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            let boundary = i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars[i - 1].is_ascii_digit()
                    || chars.get(i + 1).is_some_and(|n| n.is_lowercase())
                        && chars[i - 1].is_uppercase());
            if boundary && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            current.extend(c.to_lowercase());
        }
        if !current.is_empty() {
            parts.push(current);
        }
        if parts.len() > 1 {
            parts.push(word.to_lowercase());
        }
        out.extend(
            parts
                .into_iter()
                .filter(|t| !STOP_WORDS.contains(&t.as_str()))
                .map(|t| stem(&t)),
        );
    }
    out
}

fn bm25(fields: &[Vec<String>], terms: &[String]) -> Vec<f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let counts: Vec<HashMap<&str, usize>> = fields
        .iter()
        .map(|field| {
            let mut counts = HashMap::new();
            for term in field {
                *counts.entry(term.as_str()).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let total: usize = fields.iter().map(Vec::len).sum();
    let average = if fields.is_empty() {
        1.0
    } else {
        (total as f64 / fields.len() as f64).max(1.0)
    };
    counts
        .iter()
        .zip(fields)
        .map(|(count, field)| {
            let norm = K1 * (1.0 - B + B * field.len() as f64 / average);
            terms
                .iter()
                .filter_map(|term| count.get(term.as_str()).map(|tf| (term, *tf as f64)))
                .map(|(term, tf)| {
                    let df = counts
                        .iter()
                        .filter(|c| c.contains_key(term.as_str()))
                        .count() as f64;
                    let idf = (1.0 + (fields.len() as f64 - df + 0.5) / (df + 0.5)).ln();
                    idf * tf * (K1 + 1.0) / (tf + norm)
                })
                .sum()
        })
        .collect()
}

/// Entries matching `query`, best first (ties by id); an empty query returns
/// every entry by id. Names score separately and count double.
pub fn rank_entries<'a>(entries: &'a [CatalogEntry], query: &str) -> Vec<&'a CatalogEntry> {
    let mut items: Vec<&CatalogEntry> = entries.iter().collect();
    if query.trim().is_empty() {
        items.sort_by(|a, b| a.id.cmp(&b.id));
        return items;
    }
    let mut terms: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|t| t.len() > 1)
        .collect();
    terms.sort();
    terms.dedup();
    if terms.is_empty() || items.is_empty() {
        return Vec::new();
    }
    let names: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            let intent = if item.kind == EntryKind::Native {
                native_intent(&item.name)
            } else {
                ""
            };
            tokenize(&format!("{} {intent}", item.name))
        })
        .collect();
    let bodies: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            tokenize(&format!(
                "{} {} {} {}",
                item.id, item.source, item.description, item.search_text
            ))
        })
        .collect();
    let name_scores = bm25(&names, &terms);
    let body_scores = bm25(&bodies, &terms);
    let mut scored: Vec<(f64, &CatalogEntry)> = items
        .into_iter()
        .zip(name_scores.into_iter().zip(body_scores))
        .filter(|(_, (n, b))| n + b > 0.0)
        .map(|(item, (n, b))| (2.0 * n + b, item))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, item)| item).collect()
}

#[derive(Serialize)]
struct Row<'a> {
    id: &'a str,
    name: &'a str,
    kind: &'a str,
    source: &'a str,
    loadable: bool,
    loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

#[derive(Serialize)]
struct Page<'a> {
    results: Vec<Row<'a>>,
    total: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

/// One page of catalog search results as compact JSON: at most 40 rows,
/// 4 KiB for an index (empty query) or 8 KiB for a search.
pub fn catalog_page(
    entries: &[CatalogEntry],
    query: &str,
    state: &SurfaceState,
    offset: usize,
) -> String {
    let matches = rank_entries(entries, query);
    let searching = !query.trim().is_empty();
    let cap = if searching { 8192 } else { 4096 } - 256;
    let mut page = Page {
        results: Vec::new(),
        total: matches.len(),
        truncated: false,
        next_offset: None,
        hint: matches
            .is_empty()
            .then_some("Browse with an empty query; narrow with kind or source."),
    };
    let mut cursor = offset.min(matches.len());
    while cursor < matches.len() {
        let item = matches[cursor];
        let limit = if item.loadable { 160 } else { 512 };
        let description = (searching || !item.loadable).then(|| {
            let end = item
                .description
                .char_indices()
                .take_while(|(i, _)| *i < limit)
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(0);
            &item.description[..end]
        });
        page.results.push(Row {
            id: &item.id,
            name: &item.name,
            kind: item.kind.as_str(),
            source: &item.source,
            loadable: item.loadable,
            loaded: state.loaded.contains(&item.id),
            description,
        });
        let size = serde_json::to_string(&page)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if size > cap || page.results.len() > 40 {
            page.results.pop();
            break;
        }
        cursor += 1;
    }
    if cursor < matches.len() {
        page.truncated = true;
        page.next_offset = Some(cursor);
    }
    serde_json::to_string(&page).unwrap_or_default()
}

/// Everything the current request's surface is built from; shared by the
/// orchestrator (which publishes it) and `tool_discovery` (which extends it
/// with MCP tools found by connecting a server).
#[derive(Debug, Default)]
pub struct SurfaceData {
    /// Tools on the agent's surface (eager or lazy), in agent order.
    pub tools: Vec<SurfaceTool>,
    /// Status rows: MCP servers not yet connected or that failed.
    pub status_rows: Vec<CatalogEntry>,
    /// Skills the model may load.
    pub skills: Vec<CatalogEntry>,
    /// Known tools this agent may not use (for "blocked" vs "unknown").
    pub blocked: BTreeSet<String>,
    /// Path globs per skill, for one-time offers when a file matches.
    pub skill_paths: BTreeMap<String, Vec<String>>,
    /// Tools each skill activates when loaded.
    pub skill_tools: BTreeMap<String, Vec<String>>,
}

/// A tool on the surface.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceTool {
    pub definition: ToolDefinition,
    pub mode: ToolMode,
    pub source: ToolSource,
}

impl SurfaceTool {
    /// Catalog id of the tool.
    pub fn catalog_id(&self) -> String {
        format!("{}:{}", self.source.catalog_kind(), self.definition.name)
    }
}

/// Shared handle on [`SurfaceData`].
#[derive(Debug, Clone, Default)]
pub struct SharedSurface(Arc<Mutex<SurfaceData>>);

impl SharedSurface {
    /// Wraps surface data.
    pub fn new(data: SurfaceData) -> Self {
        Self(Arc::new(Mutex::new(data)))
    }

    /// Runs `f` with the data locked.
    pub fn with<R>(&self, f: impl FnOnce(&mut SurfaceData) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// The catalog: every lazy tool, every status row, every skill.
    pub fn catalog(&self) -> Vec<CatalogEntry> {
        self.with(|data| {
            let mut rows: Vec<CatalogEntry> = data
                .tools
                .iter()
                .filter(|tool| tool.mode == ToolMode::Lazy)
                .map(|tool| CatalogEntry::for_tool(&tool.definition, &tool.source))
                .collect();
            rows.extend(data.status_rows.iter().cloned());
            rows.extend(data.skills.iter().cloned());
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            rows
        })
    }

    /// Definitions to publish: eager tools, loaded lazy tools, and
    /// `tool_discovery` when anything is left to discover.
    pub fn published(&self, state: &SurfaceState) -> Vec<ToolDefinition> {
        self.with(|data| {
            let lazy_left = data.tools.iter().any(|t| t.mode == ToolMode::Lazy)
                || !data.skills.is_empty()
                || !data.status_rows.is_empty();
            data.tools
                .iter()
                .filter(|tool| match tool.mode {
                    ToolMode::Eager => tool.definition.name.as_str() != TOOL_DISCOVERY || lazy_left,
                    ToolMode::Lazy => state.loaded.contains(&tool.catalog_id()),
                    ToolMode::Ban => false,
                })
                .map(|tool| tool.definition.clone())
                .collect()
        })
    }

    /// The catalog id of a lazy tool that is not loaded yet, for the
    /// unavailable-tool message.
    pub fn loadable_id(&self, name: &str, state: &SurfaceState) -> Option<String> {
        self.with(|data| {
            data.tools
                .iter()
                .find(|t| t.mode == ToolMode::Lazy && t.definition.name.as_str() == name)
                .map(SurfaceTool::catalog_id)
                .filter(|id| !state.loaded.contains(id))
        })
    }

    /// The catalog id a `tool_discovery` `load` names: the id itself, or the
    /// id of the lazy tool with that bare name.
    pub fn load_id(&self, load: &str) -> String {
        self.with(|data| {
            data.tools
                .iter()
                .find(|t| t.mode == ToolMode::Lazy && t.definition.name.as_str() == load)
                .map(SurfaceTool::catalog_id)
        })
        .unwrap_or_else(|| load.to_string())
    }

    /// Why `name` is not callable on this request.
    pub fn absent(&self, name: &ToolName, state: &SurfaceState) -> crate::Absent {
        if let Some(id) = self.loadable_id(name.as_str(), state) {
            return crate::Absent::NotLoaded(id);
        }
        let legacy = name.to_legacy_mcp_name().map(|n| n.to_string());
        let blocked = self.with(|data| {
            data.blocked.contains(name.as_str())
                || legacy.as_ref().is_some_and(|l| data.blocked.contains(l))
        });
        if blocked || crate::ToolCatalog::contains(name) {
            crate::Absent::Blocked
        } else {
            crate::Absent::Unknown
        }
    }

    /// Adds MCP tools found by connecting a server; a tool already present is
    /// replaced (its schema may have changed).
    pub fn add_tools(&self, tools: Vec<SurfaceTool>, server: &str) {
        self.with(|data| {
            data.status_rows.retain(|row| row.source != server);
            for tool in tools {
                data.tools
                    .retain(|t| t.definition.name != tool.definition.name);
                data.tools.push(tool);
            }
        })
    }

    /// Replaces the status row of `server`.
    pub fn set_status(&self, server: &str, row: CatalogEntry) {
        self.with(|data| {
            data.status_rows.retain(|r| r.source != server);
            data.status_rows.push(row);
        })
    }
}

/// Alias profile resolved for one model: canonical name → model-facing name.
pub type AliasMap = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn meta(name: &str, read_only: bool, mode: ToolMode) -> ToolMeta {
        ToolMeta {
            name: ToolName::new(name),
            alternate_names: vec![],
            read_only,
            default_mode: mode,
        }
    }

    fn fixture_tools() -> Vec<ToolMeta> {
        vec![
            meta("read", true, ToolMode::Eager),
            meta("shell", false, ToolMode::Eager),
            meta("lsp", true, ToolMode::Lazy),
            meta("kernel", false, ToolMode::Lazy),
            ToolMeta {
                alternate_names: vec!["mcp_github_tool_list_issues".to_string()],
                ..meta("github__list_issues", false, ToolMode::Lazy)
            },
        ]
    }

    fn modes(selection: &Selection) -> Vec<(String, Option<ToolMode>)> {
        selection
            .decisions
            .iter()
            .map(|d| (d.tool.to_string(), d.mode))
            .collect()
    }

    #[test]
    fn test_bare_names_take_default_modes_and_exact_beats_glob() {
        let entries: Vec<String> = ["read", "*:ban", "kernel:eager", "mcp_*"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let actual = modes(&select_tools(&entries, &fixture_tools()));
        let expected = vec![
            ("read".to_string(), Some(ToolMode::Eager)),
            ("shell".to_string(), Some(ToolMode::Ban)),
            ("lsp".to_string(), Some(ToolMode::Ban)),
            ("kernel".to_string(), Some(ToolMode::Eager)),
            // `*:ban` comes first among the globs.
            ("github__list_issues".to_string(), Some(ToolMode::Ban)),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_tag_read_only_and_legacy_mcp_glob() {
        let entries: Vec<String> = ["tag:read_only:lazy", "mcp_*:eager"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let actual = modes(&select_tools(&entries, &fixture_tools()));
        let expected = vec![
            ("read".to_string(), Some(ToolMode::Lazy)),
            ("shell".to_string(), None),
            ("lsp".to_string(), Some(ToolMode::Lazy)),
            ("kernel".to_string(), None),
            ("github__list_issues".to_string(), Some(ToolMode::Eager)),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unmatched_and_malformed_entries_warn() {
        let entries: Vec<String> = ["reed", "read:sometimes", "tag:fast", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let actual = select_tools(&entries, &fixture_tools()).warnings;
        let expected = vec![
            "tools entry \"read:sometimes\": unknown modifier \"sometimes\" (expected eager, lazy or ban)".to_string(),
            "tools entry \"tag:fast\": unknown tag \"fast\" (known: read_only)".to_string(),
            "tools entry \"reed\" matches no tool (reed)".to_string(),
            "tools entry \"search\" matches no tool (fs_search)".to_string(),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_tokenize_splits_camel_and_stems() {
        let actual = tokenize("browserProbe searches GitHub issues");
        let expected = vec![
            "browser",
            "probe",
            "browserprobe",
            "search",
            "git",
            "hub",
            "github",
            "issue",
        ];
        assert_eq!(actual, expected);
    }

    fn entry(id: &str, name: &str, description: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            name: name.to_string(),
            kind: EntryKind::Native,
            source: "native".to_string(),
            description: description.to_string(),
            search_text: String::new(),
            loadable: true,
        }
    }

    #[test]
    fn test_rank_prefers_name_match_and_drops_misses() {
        let entries = vec![
            entry(
                "native:kernel",
                "kernel",
                "Persistent Python for the session.",
            ),
            entry(
                "native:lsp",
                "lsp",
                "Language server diagnostics, definition and references.",
            ),
            entry(
                "native:browse",
                "browse",
                "Render a page; python is not needed.",
            ),
        ];
        let actual: Vec<&str> = rank_entries(&entries, "jupyter python")
            .into_iter()
            .map(|e| e.id.as_str())
            .collect();
        let expected = vec!["native:kernel", "native:browse"];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_catalog_page_index_omits_descriptions_and_marks_loaded() {
        let entries = vec![entry("native:lsp", "lsp", "Language server.")];
        let state = SurfaceState {
            loaded: ["native:lsp".to_string()].into(),
            ..Default::default()
        };
        let actual = catalog_page(&entries, "", &state, 0);
        let expected = r#"{"results":[{"id":"native:lsp","name":"lsp","kind":"native","source":"native","loadable":true,"loaded":true}],"total":1,"truncated":false}"#;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_catalog_page_paginates() {
        let entries: Vec<CatalogEntry> = (0..60)
            .map(|i| entry(&format!("native:t{i:02}"), &format!("t{i:02}"), "x"))
            .collect();
        let actual: serde_json::Value =
            serde_json::from_str(&catalog_page(&entries, "", &SurfaceState::default(), 0)).unwrap();
        assert_eq!(actual["truncated"], true);
        let next = actual["next_offset"].as_u64().unwrap() as usize;
        assert_eq!(actual["results"].as_array().unwrap().len(), next);
        let second: serde_json::Value =
            serde_json::from_str(&catalog_page(&entries, "", &SurfaceState::default(), next))
                .unwrap();
        assert_eq!(second["results"][0]["id"], format!("native:t{next:02}"));
    }

    #[test]
    fn test_published_adds_loaded_lazy_and_discovery_only_when_needed() {
        let tool = |name: &str, mode| SurfaceTool {
            definition: ToolDefinition::new(name),
            mode,
            source: ToolSource::Native,
        };
        let surface = SharedSurface::new(SurfaceData {
            tools: vec![
                tool("read", ToolMode::Eager),
                tool("lsp", ToolMode::Lazy),
                tool(TOOL_DISCOVERY, ToolMode::Eager),
            ],
            ..Default::default()
        });
        let names = |defs: Vec<ToolDefinition>| {
            defs.into_iter()
                .map(|d| d.name.to_string())
                .collect::<Vec<_>>()
        };

        let before = names(surface.published(&SurfaceState::default()));
        let loaded = SurfaceState {
            loaded: ["native:lsp".to_string()].into(),
            ..Default::default()
        };
        let after = names(surface.published(&loaded));

        assert_eq!(before, vec!["read", TOOL_DISCOVERY]);
        assert_eq!(after, vec!["read", "lsp", TOOL_DISCOVERY]);
        assert_eq!(
            surface.loadable_id("lsp", &SurfaceState::default()),
            Some("native:lsp".to_string())
        );
    }

    #[test]
    fn test_load_id_resolves_a_lazy_tool_name() {
        let tool = |name: &str, mode| SurfaceTool {
            definition: ToolDefinition::new(name),
            mode,
            source: ToolSource::Native,
        };
        let fixture = SharedSurface::new(SurfaceData {
            tools: vec![tool("lsp", ToolMode::Lazy), tool("read", ToolMode::Eager)],
            ..Default::default()
        });

        let actual =
            ["lsp", "native:lsp", "read", "skill:review"].map(|load| fixture.load_id(load));

        let expected = ["native:lsp", "native:lsp", "read", "skill:review"].map(String::from);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_source_mcp_matches_every_mcp_row_and_a_server_name_in_any_case() {
        let fixture = CatalogEntry::for_tool(
            &ToolDefinition::new("github__list_issues"),
            &ToolSource::Mcp("github".to_string()),
        );
        let native = entry("native:lsp", "lsp", "Language server.");

        let actual = [
            fixture.matches_source("mcp"),
            fixture.matches_source("GitHub"),
            fixture.matches_source("native"),
            native.matches_source("mcp"),
            native.matches_source("native"),
        ];

        let expected = [true, true, false, false, true];
        assert_eq!(actual, expected);
    }
}
