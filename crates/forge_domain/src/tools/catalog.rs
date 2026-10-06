#![allow(clippy::enum_variant_names)]
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use convert_case::{Case, Casing};
use derive_more::From;
use eserde::Deserialize;
use forge_tool_macros::ToolDescription;
use schemars::{JsonSchema, Schema};
use serde::Serialize;
use serde_json::Map;
use strum::IntoEnumIterator;
use strum_macros::{AsRefStr, Display, EnumDiscriminants, EnumIter};

use crate::{ToolCallArguments, ToolCallFull, ToolDefinition, ToolDescription, ToolName};

/// Enum representing all possible tool input types.
///
/// This enum contains variants for each type of input that can be passed to
/// tools in the application. Each variant corresponds to the input type for a
/// specific tool.
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    JsonSchema,
    From,
    EnumIter,
    Display,
    PartialEq,
    EnumDiscriminants,
)]
#[strum_discriminants(derive(Display, Serialize, Deserialize, Hash))]
#[strum_discriminants(serde(rename_all = "snake_case"))]
#[serde(tag = "name", content = "arguments", rename_all = "snake_case")]
#[strum_discriminants(name(ToolKind))]
#[strum(serialize_all = "snake_case")]
pub enum ToolCatalog {
    #[serde(alias = "Read")]
    Read(FSRead),
    #[serde(alias = "Write")]
    Write(FSWrite),
    FsSearch(FSSearch),
    AstSearch(AstSearch),
    Remove(FSRemove),
    Patch(FSPatch),
    Undo(FSUndo),
    Shell(Shell),
    Fetch(NetFetch),
    Plan(PlanCreate),
    Skill(SkillFetch),
    TodoWrite(TodoWrite),
    TodoRead(TodoRead),
    #[serde(alias = "Task")]
    Task(TaskInput),
    ToolDiscovery(ToolDiscovery),
    #[from(ignore)]
    McpResourceList(McpServerArg),
    #[from(ignore)]
    McpResourceTemplates(McpServerArg),
    #[from(ignore)]
    McpResourceRead(McpResourceArg),
    #[from(ignore)]
    McpResourceSubscribe(McpResourceArg),
    #[from(ignore)]
    McpResourceUnsubscribe(McpResourceArg),
    #[from(ignore)]
    McpPromptList(McpServerArg),
    McpPromptGet(McpPromptArg),
}

/// Input for MCP controls that act on a whole server (M3).
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct McpServerArg {
    /// Exact MCP server name (from a `tool_discovery` mcp status row).
    pub server: String,
}

/// Input for MCP controls that act on one resource (M3).
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct McpResourceArg {
    /// Exact MCP server name (from a `tool_discovery` mcp status row).
    pub server: String,
    /// Exact resource URI, as returned by `mcp_resource_list` or
    /// `mcp_resource_templates`.
    pub uri: String,
}

/// Input for `mcp_prompt_get` (M3).
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct McpPromptArg {
    /// Exact MCP server name (from a `tool_discovery` mcp status row).
    pub server: String,
    /// Exact prompt name, as returned by `mcp_prompt_list`.
    pub name: String,
    /// Prompt arguments, name to string value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<std::collections::BTreeMap<String, String>>,
}

/// Shared setup sentence of the MCP control descriptions.
const MCP_CONTROL_SETUP: &str = " Before using it, run tool_discovery with kind=\"mcp\" (or an MCP server name as source) to connect the servers, then pass the exact server name of an mcp status row.";

impl ToolCatalog {
    /// Descriptions of the MCP controls (M3), which have no description
    /// file of their own.
    fn mcp_control_description(&self) -> Option<String> {
        let (what, extra) = match self {
            ToolCatalog::McpResourceList(_) => ("List the resources of an MCP server.", ""),
            ToolCatalog::McpResourceTemplates(_) => {
                ("List the resource templates of an MCP server.", "")
            }
            ToolCatalog::McpResourceRead(_) => (
                "Read an MCP resource by URI.",
                " Copy an exact URI from mcp_resource_list or mcp_resource_templates.",
            ),
            ToolCatalog::McpResourceSubscribe(_) => (
                "Subscribe to update notifications for an MCP resource.",
                " Copy an exact URI from mcp_resource_list.",
            ),
            ToolCatalog::McpResourceUnsubscribe(_) => (
                "Stop update notifications for an MCP resource.",
                " Copy an exact URI from mcp_resource_list.",
            ),
            ToolCatalog::McpPromptList(_) => ("List the prompts of an MCP server.", ""),
            ToolCatalog::McpPromptGet(_) => (
                "Get an MCP prompt and its messages.",
                " Copy an exact prompt name from mcp_prompt_list.",
            ),
            _ => return None,
        };
        Some(format!(
            "{what}{MCP_CONTROL_SETUP}{extra} Configured secrets are redacted from the result."
        ))
    }
}

/// Input for `tool_discovery`: search the catalog of loadable tools and
/// skills, or load one by id.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/tool_discovery.md"]
pub struct ToolDiscovery {
    /// Intent or words to search for; empty returns a compact index.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query: String,

    /// Catalog id to load, such as `native:lsp`, `mcp:github__list_issues`
    /// or `skill:review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<String>,

    /// Only rows of this kind: native, mcp or skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DiscoveryKind>,

    /// Only rows with exactly this source (an MCP server name, `native`, or
    /// a skill source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Continue a truncated result at its `next_offset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

/// Catalog kind filter for `tool_discovery`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, AsRefStr, EnumIter)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DiscoveryKind {
    Native,
    Mcp,
    Skill,
}

impl JsonSchema for DiscoveryKind {
    fn schema_name() -> Cow<'static, str> {
        <Self as SimpleEnumSchema>::simple_enum_schema_name()
    }

    fn json_schema(r#gen: &mut schemars::generate::SchemaGenerator) -> Schema {
        <Self as SimpleEnumSchema>::simple_enum_schema(r#gen)
    }
}

/// Input structure for agent tool calls. This serves as the generic schema
/// for dynamically registered agent tools, allowing users to specify tasks
/// for specific agents.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct AgentInput {
    /// A list of clear and detailed descriptions of the tasks to be performed
    /// by the agent in parallel. Provide sufficient context and specific
    /// requirements to enable the agent to understand and execute the work
    /// accurately.
    pub tasks: Vec<String>,
}

/// Input structure for the Task tool - delegates work to specialized agents
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/task.md"]
pub struct TaskInput {
    /// Self-contained prompts, one per worker; they run in parallel (required
    /// to start workers).
    #[serde(default)]
    pub tasks: Vec<String>,

    /// The agent the workers run as (required to start workers).
    #[serde(default)]
    pub agent_id: String,

    /// Resume an earlier worker session (one task only). An unknown id is an
    /// error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Run the workers in the background and return a handle at once.
    #[serde(default, skip_serializing_if = "is_default")]
    pub background: bool,

    /// Model the workers run on instead of the agent's configured model. Leave
    /// unset unless the user asks for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// run (default), poll, wait or kill; poll/wait/kill act on a background
    /// task's handle.
    #[serde(default, skip_serializing_if = "is_default")]
    pub action: ShellAction,

    /// Handle of a background task (default: your most recent running one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,

    /// Seconds a wait blocks (default: until the task ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub timeout: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// Status of a todo item
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Display, AsRefStr, EnumIter, Default,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TodoStatus {
    /// Task is pending and not yet started
    #[default]
    Pending,
    /// Task is currently in progress
    InProgress,
    /// Task has been completed
    Completed,
    /// Task is cancelled and should be removed from the list
    Cancelled,
}

impl JsonSchema for TodoStatus {
    fn schema_name() -> Cow<'static, str> {
        <Self as SimpleEnumSchema>::simple_enum_schema_name()
    }

    fn json_schema(r#gen: &mut schemars::generate::SchemaGenerator) -> Schema {
        <Self as SimpleEnumSchema>::simple_enum_schema(r#gen)
    }
}

/// A todo item
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, derive_setters::Setters)]
#[setters(strip_option, into)]
pub struct Todo {
    /// Unique identifier for the todo item
    pub id: String,
    /// Content/description of the todo item
    pub content: String,
    /// Current status of the todo
    pub status: TodoStatus,
}

impl Todo {
    /// Creates a new todo with the given content
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            id: String::new(), // Will be generated by service if empty
            content: content.into(),
            status: TodoStatus::default(),
        }
    }

    /// Creates a test fixture todo
    #[cfg(test)]
    pub fn test() -> Self {
        Self {
            id: "test-id".to_string(),
            content: "test content".to_string(),
            status: TodoStatus::default(),
        }
    }

    /// Validates the todo item
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Content is empty
    /// - Content exceeds maximum length (1000 characters)
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.content.trim().is_empty() {
            anyhow::bail!("Todo content cannot be empty");
        }

        if self.content.len() > 1000 {
            anyhow::bail!("Todo content exceeds maximum length of 1000 characters");
        }

        Ok(())
    }

    /// Checks if the todo is completed
    pub fn is_completed(&self) -> bool {
        self.status == TodoStatus::Completed
    }

    /// Checks if the todo is incomplete (pending or in progress)
    pub fn is_incomplete(&self) -> bool {
        matches!(self.status, TodoStatus::Pending | TodoStatus::InProgress)
    }
}

/// Optional range for partial file reads: a line range or a byte range, not
/// both.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct FSReadRange {
    /// 1-based first line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i32>,

    /// Inclusive 1-based last line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i32>,

    /// 0-based byte offset of the first byte to read (byte range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_byte: Option<u64>,

    /// 0-based offset one past the last byte to read (byte range).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_byte: Option<u64>,
}

impl FSReadRange {
    /// Whether a line bound is given.
    pub fn is_line_range(&self) -> bool {
        self.start_line.is_some() || self.end_line.is_some()
    }

    /// Whether a byte bound is given.
    pub fn is_byte_range(&self) -> bool {
        self.start_byte.is_some() || self.end_byte.is_some()
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_read.md"]
#[schemars(deny_unknown_fields)]
pub struct FSRead {
    /// Path of the file to read.
    #[serde(alias = "path")]
    pub file_path: String,

    /// Optional line range (`start_line`/`end_line`) or byte range
    /// (`start_byte`/`end_byte`) for partial reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<FSReadRange>,

    /// If true, prefixes each line with its line number. Defaults to true.
    #[serde(default = "default_true")]
    pub show_line_numbers: bool,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_write.md"]
#[schemars(deny_unknown_fields)]
pub struct FSWrite {
    /// Path of the file to create or replace.
    #[serde(alias = "path")]
    pub file_path: String,

    /// The complete file content to write.
    pub content: String,

    /// Required to replace an existing file; the file must have been read
    /// first and must not have changed since.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_search.md"]
#[derive(Default)]
pub struct FSSearch {
    /// Regex to search file contents for.
    pub pattern: String,

    /// File or directory to search (default: the current directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Glob filter on paths (e.g. "*.rs", "*.{ts,tsx}", "!*.min.js").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    /// "content" (matching lines), "files_with_matches" (paths, default) or
    /// "count" (matches per file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<OutputMode>,

    /// Lines of context before each match (content mode).
    #[serde(alias = "-B", skip_serializing_if = "Option::is_none")]
    pub context_before: Option<u32>,

    /// Lines of context after each match (content mode).
    #[serde(alias = "-A", skip_serializing_if = "Option::is_none")]
    pub context_after: Option<u32>,

    /// Lines of context before and after each match (content mode).
    #[serde(alias = "-C", skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,

    /// Prefix content lines with their line numbers (default true).
    #[serde(alias = "-n", skip_serializing_if = "Option::is_none")]
    pub line_numbers: Option<bool>,

    /// Match without case sensitivity.
    #[serde(alias = "-i", skip_serializing_if = "Option::is_none")]
    pub case_insensitive: Option<bool>,

    /// File type, e.g. rust, py, js (ripgrep type names).
    #[serde(alias = "type", skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,

    /// Maximum result entries after `offset`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub head_limit: Option<u32>,

    /// Result entries to skip before `head_limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,

    /// Let the regex span lines (`.` matches newlines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiline: Option<bool>,
}

impl FSSearch {
    /// ripgrep spellings accepted on input and their schema names. They stay
    /// out of the schema; the executor rewrites them before validation.
    pub const ARGUMENT_ALIASES: [(&'static str, &'static str); 6] = [
        ("-A", "context_after"),
        ("-B", "context_before"),
        ("-C", "context"),
        ("-n", "line_numbers"),
        ("-i", "case_insensitive"),
        ("type", "file_type"),
    ];
}

/// Output mode for search results
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, AsRefStr, EnumIter)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// Show matching lines with content
    Content,
    /// Show only file paths with matches
    FilesWithMatches,
    /// Show match counts per file
    Count,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_remove.md"]
#[schemars(deny_unknown_fields)]
pub struct FSRemove {
    /// Path of the file, directory or symlink to remove.
    pub path: String,

    /// Delete directly instead of moving to the trash (default false).
    #[serde(default, skip_serializing_if = "is_default")]
    pub permanent: bool,
}

impl FSRemove {
    /// Creates a trash-mode remove input for `path`.
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into(), permanent: false }
    }
}

/// Helper trait to generate simple string enum schemas for unit enums.
///
/// This trait is automatically implemented for enums that derive both
/// `AsRefStr` and `EnumIter`. It provides a consistent way to generate
/// JSON schemas that represent enums as simple string enumerations
/// rather than complex oneOf structures.
trait SimpleEnumSchema: AsRef<str> + IntoEnumIterator {
    fn simple_enum_schema_name() -> Cow<'static, str> {
        std::any::type_name::<Self>()
            .split("::")
            .last()
            .unwrap_or("Enum")
            .to_string()
            .into()
    }

    fn simple_enum_schema(_gen: &mut schemars::generate::SchemaGenerator) -> Schema {
        use schemars::json_schema;
        let variants: Vec<serde_json::Value> = Self::iter()
            .map(|variant| variant.as_ref().to_case(Case::Snake).into())
            .collect();

        json_schema!({
            "type": "string",
            "enum": variants
        })
    }
}

// Blanket implementation for all types that implement AsRef<str> and
// IntoEnumIterator
impl<T> SimpleEnumSchema for T where T: AsRef<str> + IntoEnumIterator {}

impl JsonSchema for OutputMode {
    fn schema_name() -> Cow<'static, str> {
        <Self as SimpleEnumSchema>::simple_enum_schema_name()
    }

    fn json_schema(r#gen: &mut schemars::generate::SchemaGenerator) -> Schema {
        <Self as SimpleEnumSchema>::simple_enum_schema(r#gen)
    }
}

/// Input for the `patch` tool: either the scalar form (`old_string`,
/// `new_string`, `replace_all`) or the batch form (`edits`), never both.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_patch.md"]
#[schemars(deny_unknown_fields)]
pub struct FSPatch {
    /// Path of the file to modify.
    #[serde(alias = "path")]
    pub file_path: String,

    /// Exact text to replace (scalar form).
    #[serde(default, alias = "search", skip_serializing_if = "Option::is_none")]
    pub old_string: Option<String>,

    /// Replacement text; must differ from old_string (scalar form).
    #[serde(default, alias = "content", skip_serializing_if = "Option::is_none")]
    pub new_string: Option<String>,

    /// Replace every occurrence of old_string instead of requiring exactly
    /// one (scalar form, default false).
    #[serde(default, skip_serializing_if = "is_default")]
    #[schemars(default)]
    pub replace_all: bool,

    /// Several replacements applied in order, each seeing the previous
    /// result. Never combined with the scalar form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub edits: Option<Vec<PatchEdit>>,
}

impl FSPatch {
    /// Creates a scalar-form patch input.
    ///
    /// # Arguments
    /// * `file_path` - Path of the file to modify.
    /// * `old_string` - Exact text to replace.
    /// * `new_string` - Replacement text.
    pub fn new(
        file_path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Self {
        Self {
            file_path: file_path.into(),
            old_string: Some(old_string.into()),
            new_string: Some(new_string.into()),
            replace_all: false,
            edits: None,
        }
    }

    /// Creates a batch-form patch input.
    ///
    /// # Arguments
    /// * `file_path` - Path of the file to modify.
    /// * `edits` - Edits applied in order.
    pub fn batch(file_path: impl Into<String>, edits: Vec<PatchEdit>) -> Self {
        Self {
            file_path: file_path.into(),
            old_string: None,
            new_string: None,
            replace_all: false,
            edits: Some(edits),
        }
    }

    /// Sets the scalar `replace_all` flag.
    pub fn replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = replace_all;
        self
    }

    /// Resolves the input into the ordered list of edits to apply.
    ///
    /// # Errors
    /// Returns an error when both forms are given, when neither form is
    /// complete, or when `edits` is empty.
    pub fn resolve_edits(&self) -> anyhow::Result<Vec<PatchEdit>> {
        match &self.edits {
            Some(_)
                if self.old_string.is_some() || self.new_string.is_some() || self.replace_all =>
            {
                anyhow::bail!(
                    "pass either the scalar form (old_string/new_string/replace_all) or edits, not both"
                )
            }
            Some(edits) if edits.is_empty() => {
                anyhow::bail!(
                    "edits must be a non-empty list of {{old_string, new_string}} objects"
                )
            }
            Some(edits) => Ok(edits.clone()),
            None => match (&self.old_string, &self.new_string) {
                (Some(old_string), Some(new_string)) => Ok(vec![PatchEdit {
                    old_string: old_string.clone(),
                    new_string: new_string.clone(),
                    replace_all: self.replace_all,
                }]),
                _ => anyhow::bail!("old_string and new_string are required (or pass edits)"),
            },
        }
    }

    /// Whether the call uses the batch (`edits`) form.
    pub fn is_batch(&self) -> bool {
        self.edits.is_some()
    }
}

/// A single replacement inside a batch `patch` call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct PatchEdit {
    /// Exact text to replace.
    #[serde(alias = "search")]
    pub old_string: String,

    /// Replacement text; must differ from old_string.
    #[serde(alias = "content")]
    pub new_string: String,

    /// Replace every occurrence of old_string instead of requiring exactly
    /// one (default false).
    #[serde(default)]
    #[schemars(default)]
    pub replace_all: bool,
}

impl PatchEdit {
    /// Creates a single-occurrence edit.
    ///
    /// # Arguments
    /// * `old_string` - Exact text to replace.
    /// * `new_string` - Replacement text.
    pub fn new(old_string: impl Into<String>, new_string: impl Into<String>) -> Self {
        Self {
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    /// Sets the `replace_all` flag.
    pub fn replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = replace_all;
        self
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/fs_undo.md"]
#[schemars(deny_unknown_fields)]
pub struct FSUndo {
    /// Path whose latest snapshot should be restored.
    pub path: String,
}

/// What a `shell` call does.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellAction {
    /// Start `command` and wait up to `timeout` seconds for it.
    #[default]
    Run,
    /// Report a running job's new output without waiting.
    Poll,
    /// Wait up to `timeout` more seconds for a running job.
    Wait,
    /// Stop a running job (SIGTERM, then SIGKILL, to its process group).
    Kill,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/shell.md"]
pub struct Shell {
    /// The command to run (required for action "run").
    #[serde(default)]
    pub command: String,

    /// Working directory for the command (default: the current directory).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    /// Keep ANSI escape codes in the output (default false).
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub keep_ansi: bool,

    /// Extra environment variable names to pass through, on top of the
    /// configured allow-list.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,

    /// Short description of what the command does.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// run (default), poll, wait or kill.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub action: ShellAction,

    /// Job id from a `HANDLE n RUNNING` line, for poll/wait/kill (default:
    /// the most recent running job).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,

    /// Seconds to wait before returning a handle (run) or to keep waiting
    /// (wait); default 30. The command is never killed when the window ends.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub timeout: Option<u64>,
}

/// Input for the `ast_search` tool: structural search and rewrite.
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/ast_search.md"]
pub struct AstSearch {
    /// ast-grep pattern; `$NAME` matches one node, `$$$NAME` zero or more.
    pub pattern: String,

    /// Language (e.g. rust, python, typescript, tsx, go, c, cpp); inferred
    /// from each file's extension when omitted.
    #[serde(default, alias = "lang", skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// File or directory to search (default: the current directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Replacement template using the pattern's metavariables; without
    /// `apply` the result is a diff and nothing is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,

    /// Write the rewrite (default false).
    #[serde(default, skip_serializing_if = "is_default")]
    pub apply: bool,

    /// Most matches reported (default 200); an apply with more matches is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_results: Option<u32>,
}

/// Input type for the net fetch tool
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/net_fetch.md"]
pub struct NetFetch {
    /// URL to fetch: http(s):// or file://.
    pub url: String,

    /// Return the body as is, without HTML-to-markdown conversion (default
    /// false).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<bool>,

    /// HTTP method (default GET).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Request headers, name to value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,

    /// Request body text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    /// Request body as JSON (sets Content-Type: application/json); not with
    /// `body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")]
    pub json_body: Option<serde_json::Value>,

    /// Stream the response body to this path instead of returning it; no size
    /// limit, resumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<String>,
}

impl NetFetch {
    /// Creates a GET request for `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Default::default() }
    }

    /// The request method, upper-cased (default GET).
    pub fn method(&self) -> String {
        self.method
            .as_deref()
            .map(|m| m.trim().to_uppercase())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "GET".to_string())
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/plan_create.md"]
pub struct PlanCreate {
    /// The name of the plan (sanitised into the filename).
    pub plan_name: String,

    /// The version of the plan (e.g., "v1", "v2", "1.0"; sanitised into the
    /// filename).
    pub version: String,

    /// The complete plan content in markdown, written verbatim.
    pub content: String,

    /// Replace an existing plan with the same name and version (default
    /// false).
    #[serde(default, skip_serializing_if = "is_default")]
    pub overwrite: bool,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/skill_fetch.md"]
pub struct SkillFetch {
    /// The name of the skill to fetch (e.g., "pdf", "code_review")
    pub name: String,
}

/// A single todo item sent by the model.
///
/// The model always provides `content` and `status`. The server uses `content`
/// as the key: if an item with the same content already exists it is updated,
/// otherwise a new item is added. Setting `status` to `cancelled` removes the
/// item from the list entirely. IDs are managed by the server and never
/// exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct TodoItem {
    /// Description of the task. Used as the unique key to match existing todos.
    pub content: String,
    /// Current status of the task. Use `cancelled` to remove the item.
    pub status: crate::TodoStatus,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/todo_write.md"]
pub struct TodoWrite {
    /// List of todo items to create or update. Each item must have `content`
    /// and `status`. The server matches on `content` — if an item with the
    /// same content exists it is updated; otherwise a new item is added.
    /// Set `status` to `cancelled` to remove an item.
    #[eserde(compat)]
    pub todos: Vec<TodoItem>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, ToolDescription, PartialEq)]
#[tool_description_file = "crates/forge_domain/src/tools/descriptions/todo_read.md"]
pub struct TodoRead {}

fn default_raw() -> Option<bool> {
    Some(false)
}

/// Retrieves content from URLs as markdown or raw text. Enables access to
/// current online information including websites, APIs and documentation. Use
/// for obtaining up-to-date information beyond training data, verifying facts,
/// or retrieving specific online content. Handles HTTP/HTTPS and converts HTML
/// to readable markdown by default. Cannot access private/restricted resources
/// requiring authentication. Respects robots.txt and may be blocked by
/// anti-scraping measures. For large pages, returns the first 40,000 characters
/// and stores the complete content in a temporary file for subsequent access.
#[derive(Default, Deserialize, JsonSchema, ToolDescription, PartialEq)]
pub struct FetchInput {
    /// URL to fetch
    pub url: String,
    /// Get raw content without any markdown conversion (default: false)
    #[serde(default = "default_raw")]
    pub raw: Option<bool>,
}
/// Request to list files and directories within the specified directory. If
/// recursive is true, it will list all files and directories recursively. If
/// recursive is false or not provided, it will only list the top-level
/// contents. The path must be absolute. Do not use this tool to confirm the
/// existence of files you may have created, as the user will let you know if
/// the files were created successfully or not.
#[derive(Default, Deserialize, JsonSchema, ToolDescription, PartialEq)]
pub struct FSListInput {
    /// The path of the directory to list contents for (absolute path required)
    pub path: String,
    /// Whether to list files recursively. Use true for recursive listing, false
    /// or omit for top-level only.
    pub recursive: Option<bool>,
}

/// Request to retrieve detailed metadata about a file or directory at the
/// specified path. Returns comprehensive information including size, creation
/// time, last modified time, permissions, and type. Path must be absolute. Use
/// this when you need to understand file characteristics without reading the
/// actual content.
#[derive(Default, Deserialize, JsonSchema, ToolDescription, PartialEq)]
pub struct FSFileInfoInput {
    /// The path of the file or directory to inspect (absolute path required)
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct UndoInput {
    /// The absolute path of the file to revert to its previous state. Must be
    /// the exact path that was previously modified, created, or deleted by
    /// a Forge file operation. If the file was deleted, provide the
    /// original path it had before deletion. The system requires a prior
    /// snapshot for this path.
    pub path: String,
}

/// Input for the select tool
#[derive(Deserialize, JsonSchema)]
pub struct SelectInput {
    /// Question to ask the user
    pub question: String,

    /// First option to choose from
    pub option1: Option<String>,

    /// Second option to choose from
    pub option2: Option<String>,

    /// Third option to choose from
    pub option3: Option<String>,

    /// Fourth option to choose from
    pub option4: Option<String>,

    /// Fifth option to choose from
    pub option5: Option<String>,

    /// If true, allows selecting multiple options; if false (default), only one
    /// option can be selected
    #[schemars(default)]
    pub multiple: Option<bool>,
}

/// Helper function to check if a value equals its default value
fn is_default<T: Default + PartialEq>(t: &T) -> bool {
    t == &T::default()
}

impl ToolDescription for ToolCatalog {
    fn description(&self) -> String {
        match self {
            ToolCatalog::Patch(v) => v.description(),
            ToolCatalog::Shell(v) => v.description(),
            ToolCatalog::Fetch(v) => v.description(),
            ToolCatalog::FsSearch(v) => v.description(),
            ToolCatalog::AstSearch(v) => v.description(),
            ToolCatalog::Read(v) => v.description(),
            ToolCatalog::Remove(v) => v.description(),
            ToolCatalog::Undo(v) => v.description(),
            ToolCatalog::Write(v) => v.description(),
            ToolCatalog::Plan(v) => v.description(),
            ToolCatalog::Skill(v) => v.description(),
            ToolCatalog::TodoWrite(v) => v.description(),
            ToolCatalog::TodoRead(v) => v.description(),
            ToolCatalog::Task(v) => v.description(),
            ToolCatalog::ToolDiscovery(v) => v.description(),
            ToolCatalog::McpResourceList(_)
            | ToolCatalog::McpResourceTemplates(_)
            | ToolCatalog::McpResourceRead(_)
            | ToolCatalog::McpResourceSubscribe(_)
            | ToolCatalog::McpResourceUnsubscribe(_)
            | ToolCatalog::McpPromptList(_)
            | ToolCatalog::McpPromptGet(_) => self.mcp_control_description().unwrap_or_default(),
        }
    }
}
// Cache of all tool names
static FORGE_TOOLS: LazyLock<HashSet<ToolName>> =
    LazyLock::new(|| ToolCatalog::iter().map(ToolName::new).collect());

// Case-insensitive lookup map: lowercase tool name -> canonical tool name
static FORGE_TOOLS_LOWER: LazyLock<HashMap<String, ToolName>> = LazyLock::new(|| {
    ToolCatalog::iter()
        .map(|tool| {
            let name = ToolName::new(tool.to_string());
            (name.as_str().to_lowercase(), name)
        })
        .collect()
});

/// Normalizes a tool name received in a response before catalog matching.
/// Trims surrounding whitespace and performs a case-insensitive lookup
/// against all known catalog tool names, returning the canonical form when
/// a match is found.
fn normalize_tool_name(name: &ToolName) -> ToolName {
    let trimmed = name.as_str().trim();
    let lower = trimmed.to_lowercase();
    FORGE_TOOLS_LOWER
        .get(&lower)
        .cloned()
        .unwrap_or_else(|| ToolName::new(trimmed))
}

/// Sets `additionalProperties: false` on every object schema that declares
/// `properties` and does not already say otherwise.
fn close_object_schemas(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("properties") && !map.contains_key("additionalProperties") {
                map.insert("additionalProperties".to_string(), false.into());
            }
            map.values_mut().for_each(close_object_schemas);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(close_object_schemas),
        _ => {}
    }
}

impl ToolCatalog {
    pub fn schema(&self) -> Schema {
        use schemars::generate::SchemaSettings;
        use schemars::transform::{AddNullable, Transform};

        let r#gen = SchemaSettings::default()
            .with(|s| {
                s.meta_schema = None;
                s.inline_subschemas = true;
                s.transforms.push(Box::new(crate::RemoveSchemaTitles));
            })
            .into_generator();

        let mut schema = match self {
            ToolCatalog::Patch(_) => r#gen.into_root_schema_for::<FSPatch>(),
            ToolCatalog::Shell(_) => r#gen.into_root_schema_for::<Shell>(),
            ToolCatalog::Fetch(_) => r#gen.into_root_schema_for::<NetFetch>(),
            ToolCatalog::FsSearch(_) => r#gen.into_root_schema_for::<FSSearch>(),
            ToolCatalog::AstSearch(_) => r#gen.into_root_schema_for::<AstSearch>(),
            ToolCatalog::Read(_) => r#gen.into_root_schema_for::<FSRead>(),
            ToolCatalog::Remove(_) => r#gen.into_root_schema_for::<FSRemove>(),
            ToolCatalog::Undo(_) => r#gen.into_root_schema_for::<FSUndo>(),
            ToolCatalog::Write(_) => r#gen.into_root_schema_for::<FSWrite>(),
            ToolCatalog::Plan(_) => r#gen.into_root_schema_for::<PlanCreate>(),
            ToolCatalog::Skill(_) => r#gen.into_root_schema_for::<SkillFetch>(),
            ToolCatalog::Task(_) => r#gen.into_root_schema_for::<TaskInput>(),
            ToolCatalog::TodoWrite(_) => r#gen.into_root_schema_for::<TodoWrite>(),
            ToolCatalog::TodoRead(_) => r#gen.into_root_schema_for::<TodoRead>(),
            ToolCatalog::ToolDiscovery(_) => r#gen.into_root_schema_for::<ToolDiscovery>(),
            ToolCatalog::McpResourceList(_)
            | ToolCatalog::McpResourceTemplates(_)
            | ToolCatalog::McpPromptList(_) => r#gen.into_root_schema_for::<McpServerArg>(),
            ToolCatalog::McpResourceRead(_)
            | ToolCatalog::McpResourceSubscribe(_)
            | ToolCatalog::McpResourceUnsubscribe(_) => {
                r#gen.into_root_schema_for::<McpResourceArg>()
            }
            ToolCatalog::McpPromptGet(_) => r#gen.into_root_schema_for::<McpPromptArg>(),
        };

        // Apply transform to add nullable property and remove null from type
        AddNullable::default().transform(&mut schema);

        // X2: every native tool schema is closed. Serde aliases (`path`,
        // `search`, `content`, rg flags) stay out of the schema; the executor
        // validates against it before deserialising.
        if let Some(root) = schema.as_object_mut() {
            if root.contains_key("properties") && !root.contains_key("additionalProperties") {
                root.insert("additionalProperties".to_string(), false.into());
            }
            root.values_mut().for_each(close_object_schemas);
        }

        schema
    }

    /// Tool definitions of every native tool.
    pub fn iter_definitions() -> Vec<ToolDefinition> {
        ToolCatalog::iter().map(|tool| tool.definition()).collect()
    }

    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(self)
            .description(self.description())
            .input_schema(self.schema())
    }
    pub fn contains(tool_name: &ToolName) -> bool {
        let normalized = normalize_tool_name(tool_name);
        FORGE_TOOLS.contains(&normalized)
    }

    pub fn requires_stdout(tool_name: &ToolName) -> bool {
        // Tools that require direct stdout/stderr access
        let normalized = normalize_tool_name(tool_name);
        [ToolKind::Shell]
            .iter()
            .any(|v| v.to_string().to_case(Case::Snake).eq(normalized.as_str()))
    }

    /// X6: whether this call writes nothing, so it may run in parallel with
    /// other read-only calls. Anything that mutates files, runs processes or
    /// changes session state is a barrier. A fetch is read-only only as a GET
    /// or HEAD that saves nothing.
    pub fn is_read_only(&self) -> bool {
        match self {
            ToolCatalog::Fetch(input) => {
                matches!(input.method().as_str(), "GET" | "HEAD") && input.save.is_none()
            }
            ToolCatalog::AstSearch(input) => !input.apply,
            ToolCatalog::Read(_)
            | ToolCatalog::FsSearch(_)
            | ToolCatalog::Skill(_)
            | ToolCatalog::TodoRead(_)
            | ToolCatalog::McpResourceList(_)
            | ToolCatalog::McpResourceTemplates(_)
            | ToolCatalog::McpResourceRead(_)
            | ToolCatalog::McpPromptList(_)
            | ToolCatalog::McpPromptGet(_) => true,
            _ => false,
        }
    }

    /// X7: whether the tool bounds its own run time (window, timeout or
    /// protocol limit), so the executor's blanket backstop does not apply.
    pub fn has_own_timeout(&self) -> bool {
        matches!(
            self,
            ToolCatalog::Task(_)
                | ToolCatalog::Shell(_)
                | ToolCatalog::Fetch(_)
                | ToolCatalog::McpResourceList(_)
                | ToolCatalog::McpResourceTemplates(_)
                | ToolCatalog::McpResourceRead(_)
                | ToolCatalog::McpResourceSubscribe(_)
                | ToolCatalog::McpResourceUnsubscribe(_)
                | ToolCatalog::McpPromptList(_)
                | ToolCatalog::McpPromptGet(_)
        )
    }

    /// Convert a tool input to its corresponding domain operation for policy
    /// checking. Returns None for tools that don't require permission
    /// checks.
    pub fn to_policy_operation(
        &self,
        cwd: PathBuf,
    ) -> Option<crate::policies::PermissionOperation> {
        let cwd_path = cwd.clone();
        let display_path_for = |path: &str| {
            format!(
                "`{}`",
                format_display_path(Path::new(path), cwd_path.as_path())
            )
        };

        match self {
            ToolCatalog::Read(input) => Some(crate::policies::PermissionOperation::Read {
                path: std::path::PathBuf::from(&input.file_path),
                cwd,
                message: format!("Read file: {}", display_path_for(&input.file_path)),
            }),
            ToolCatalog::Write(input) => Some(crate::policies::PermissionOperation::Write {
                path: std::path::PathBuf::from(&input.file_path),
                cwd,
                message: format!(
                    "Create/overwrite file: {}",
                    display_path_for(&input.file_path)
                ),
            }),
            ToolCatalog::FsSearch(input) => {
                let path_str = input.path.as_deref().unwrap_or(".");
                let base_message =
                    format!("Search in directory/file: {}", display_path_for(path_str));
                let message = match (&input.glob, &input.file_type) {
                    (Some(glob), _) => {
                        format!(
                            "{base_message} for pattern: '{}' in '{glob}' files",
                            input.pattern
                        )
                    }
                    (None, Some(file_type)) => {
                        format!(
                            "{base_message} for pattern: '{}' in {file_type} files",
                            input.pattern
                        )
                    }
                    (None, None) => {
                        format!("{base_message} for pattern: {}", input.pattern)
                    }
                };
                Some(crate::policies::PermissionOperation::Read {
                    path: std::path::PathBuf::from(path_str),
                    cwd,
                    message,
                })
            }
            ToolCatalog::AstSearch(input) => {
                let path = input.path.as_deref().unwrap_or(".");
                let message = format!(
                    "Structural {} in {}: {}",
                    if input.apply { "rewrite" } else { "search" },
                    display_path_for(path),
                    input.pattern
                );
                let path = std::path::PathBuf::from(path);
                Some(if input.apply {
                    crate::policies::PermissionOperation::Write { path, cwd, message }
                } else {
                    crate::policies::PermissionOperation::Read { path, cwd, message }
                })
            }
            ToolCatalog::Remove(input) => Some(crate::policies::PermissionOperation::Write {
                path: std::path::PathBuf::from(&input.path),
                cwd,
                message: format!("Remove file: {}", display_path_for(&input.path)),
            }),
            ToolCatalog::Patch(input) => {
                let message = match &input.edits {
                    Some(edits) => format!(
                        "Modify file with {} edits: {}",
                        edits.len(),
                        display_path_for(&input.file_path)
                    ),
                    None => format!("Modify file: {}", display_path_for(&input.file_path)),
                };
                Some(crate::policies::PermissionOperation::Write {
                    path: std::path::PathBuf::from(&input.file_path),
                    cwd,
                    message,
                })
            }
            ToolCatalog::Shell(input) => Some(crate::policies::PermissionOperation::Execute {
                command: input.command.clone(),
                cwd,
            }),
            ToolCatalog::Fetch(input) => Some(crate::policies::PermissionOperation::Fetch {
                url: input.url.clone(),
                cwd,
                message: format!("Fetch content from URL: {}", input.url),
            }),
            // Operations that don't require permission checks
            ToolCatalog::Undo(_)
            | ToolCatalog::Plan(_)
            | ToolCatalog::Skill(_)
            | ToolCatalog::TodoWrite(_)
            | ToolCatalog::TodoRead(_)
            | ToolCatalog::ToolDiscovery(_)
            | ToolCatalog::McpResourceList(_)
            | ToolCatalog::McpResourceTemplates(_)
            | ToolCatalog::McpResourceRead(_)
            | ToolCatalog::McpResourceSubscribe(_)
            | ToolCatalog::McpResourceUnsubscribe(_)
            | ToolCatalog::McpPromptList(_)
            | ToolCatalog::McpPromptGet(_)
            | ToolCatalog::Task(_) => None,
        }
    }

    /// Creates a Read tool call with the specified path
    pub fn tool_call_read(path: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Read(FSRead {
            file_path: path.to_string(),
            ..Default::default()
        }))
    }

    /// Creates a Write tool call with the specified path and content
    pub fn tool_call_write(path: &str, content: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Write(FSWrite {
            file_path: path.to_string(),
            content: content.to_string(),
            ..Default::default()
        }))
    }

    /// Creates a Patch tool call with the specified parameters
    pub fn tool_call_patch(
        path: &str,
        content: &str,
        search: &str,
        replace_all: bool,
    ) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Patch(
            FSPatch::new(path, search, content).replace_all(replace_all),
        ))
    }

    /// Creates a Remove tool call with the specified path
    pub fn tool_call_remove(path: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Remove(FSRemove::new(path)))
    }

    /// Creates a Shell tool call with the specified command and working
    /// directory
    pub fn tool_call_shell(command: &str, cwd: impl Into<PathBuf>) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Shell(Shell {
            command: command.to_string(),
            cwd: Some(cwd.into()),
            ..Default::default()
        }))
    }

    /// Creates a Search tool call with the specified path and pattern
    pub fn tool_call_search(path: &str, pattern: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::FsSearch(FSSearch {
            pattern: pattern.to_string(),
            path: Some(path.to_string()),
            ..Default::default()
        }))
    }

    /// Creates an Undo tool call with the specified path
    pub fn tool_call_undo(path: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Undo(FSUndo { path: path.to_string() }))
    }

    /// Creates a Fetch tool call with the specified url
    pub fn tool_call_fetch(url: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Fetch(NetFetch {
            url: url.to_string(),
            ..Default::default()
        }))
    }

    /// Creates a Plan tool call with the specified plan name, version, and
    /// content
    pub fn tool_call_plan(plan_name: &str, version: &str, content: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Plan(PlanCreate {
            plan_name: plan_name.to_string(),
            version: version.to_string(),
            content: content.to_string(),
            overwrite: false,
        }))
    }

    /// Creates a Skill tool call with the specified skill name
    pub fn tool_call_skill(skill_name: &str) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::Skill(SkillFetch {
            name: skill_name.to_string(),
        }))
    }

    /// Creates a TodoWrite tool call with the specified todo items
    pub fn tool_call_todo_write(todos: Vec<crate::TodoItem>) -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::TodoWrite(TodoWrite { todos }))
    }

    /// Creates a TodoRead tool call
    pub fn tool_call_todo_read() -> ToolCallFull {
        ToolCallFull::from(ToolCatalog::TodoRead(TodoRead::default()))
    }

    /// Identifies the kind of the built-in Tools
    pub fn kind(&self) -> ToolKind {
        self.clone().into()
    }
}

fn format_display_path(path: &Path, cwd: &Path) -> String {
    // Try to create a relative path for display if possible
    let display_path = if path.starts_with(cwd) {
        match path.strip_prefix(cwd) {
            Ok(rel_path) => rel_path.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    } else {
        path.display().to_string()
    };

    if display_path.is_empty() {
        ".".to_string()
    } else {
        display_path
    }
}

impl TryFrom<ToolCallFull> for ToolCatalog {
    type Error = crate::Error;

    fn try_from(value: ToolCallFull) -> Result<Self, Self::Error> {
        // Normalize the tool name: trim whitespace and perform case-insensitive
        // catalog match so the serde deserialization receives the canonical
        // name.
        let normalized_name = normalize_tool_name(&value.name);

        let mut map = Map::new();
        map.insert("name".into(), normalized_name.as_str().into());

        // Parse the arguments
        let parsed_args = value.arguments.parse()?;

        // Try to find the tool definition and coerce types based on schema
        let coerced_args = ToolCatalog::iter()
            .find(|tool| tool.definition().name == normalized_name)
            .map(|tool| {
                let schema = tool.definition().input_schema;
                forge_json_repair::coerce_to_schema(parsed_args.clone(), &schema)
            })
            .unwrap_or(parsed_args);

        map.insert("arguments".into(), coerced_args);

        serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|error| crate::Error::AgentCallArgument { error })
    }
}

impl ToolKind {
    pub fn name(&self) -> ToolName {
        ToolName::new(self.to_string().to_case(Case::Snake))
    }

    // TODO: This is an extremely slow operation
    pub fn definition(&self) -> ToolDefinition {
        ToolCatalog::iter()
            .find(|tool| tool.definition().name == self.name())
            .map(|tool| tool.definition())
            .expect("Forge tool definition not found")
    }
}

impl TryFrom<&ToolCallFull> for AgentInput {
    type Error = crate::Error;
    fn try_from(value: &ToolCallFull) -> Result<Self, Self::Error> {
        let value = value.arguments.parse()?;
        serde_json::from_value(value).map_err(|error| crate::Error::AgentCallArgument { error })
    }
}

impl From<ToolCatalog> for ToolCallFull {
    fn from(tool: ToolCatalog) -> Self {
        let name = ToolName::new(tool.to_string());
        // Serialize the tool to get the tagged enum structure
        let value = serde_json::to_value(&tool).expect("Failed to serialize tool");

        // Extract just the "arguments" part from the tagged enum
        let arguments = if let Some(args) = value.get("arguments") {
            ToolCallArguments::from(args.clone())
        } else {
            ToolCallArguments::default()
        };

        ToolCallFull { name, call_id: None, arguments, thought_signature: None }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use strum::IntoEnumIterator;

    use super::Shell;
    use crate::{ToolCatalog, ToolKind, ToolName};

    #[test]
    fn test_tool_definition() {
        let actual = ToolKind::Remove.name();
        let expected = ToolName::new("remove");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_requires_stdout_for_shell() {
        let fixture = ToolName::new("shell");
        assert!(ToolCatalog::requires_stdout(&fixture));
    }

    #[test]
    fn test_requires_stdout_for_non_shell() {
        let fixture = ToolName::new("read");
        assert!(!ToolCatalog::requires_stdout(&fixture));
    }

    #[test]
    fn test_tool_definition_json() {
        let tools = ToolCatalog::iter()
            .map(|tool| {
                let definition = tool.definition().input_schema;
                serde_json::to_string_pretty(&definition)
                    .expect("Failed to serialize tool definition to JSON")
            })
            .collect::<Vec<_>>()
            .join("\n");

        insta::assert_snapshot!(tools);
    }

    #[test]
    fn test_coerce_string_integers_to_i32() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Simulate the exact error case: read tool with string integers instead
        // of i32
        let tool_call = ToolCallFull {
            name: ToolName::new("read"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/path.rs", "range": {"start_line": 10, "end_line": 20}}"#,
            ),
            thought_signature: None,
        };

        // This should not panic - it should coerce strings to integers
        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse with coerced types"
        );

        if let Ok(ToolCatalog::Read(fs_read)) = actual {
            assert_eq!(fs_read.file_path, "/test/path.rs");
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.start_line), Some(10));
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.end_line), Some(20));
        } else {
            panic!("Expected FSRead variant");
        }
    }

    #[test]
    fn test_coerce_preserves_correct_types() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Verify that already-correct types are preserved
        let tool_call = ToolCallFull {
            name: ToolName::new("read"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/path.rs", "range": {"start_line": 10, "end_line": 20}}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse with correct types"
        );

        if let Ok(ToolCatalog::Read(fs_read)) = actual {
            assert_eq!(fs_read.file_path, "/test/path.rs");
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.start_line), Some(10));
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.end_line), Some(20));
        } else {
            panic!("Expected FSRead variant");
        }
    }

    #[test]
    fn test_capitalized_read_alias() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Test that "Read" (capitalized) is normalized to "read"
        let tool_call = ToolCallFull {
            name: ToolName::new("Read"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/path.rs", "range": {"start_line": 10, "end_line": 20}}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse capitalized 'Read' tool name"
        );

        if let Ok(ToolCatalog::Read(fs_read)) = actual {
            assert_eq!(fs_read.file_path, "/test/path.rs");
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.start_line), Some(10));
            assert_eq!(fs_read.range.as_ref().and_then(|r| r.end_line), Some(20));
        } else {
            panic!("Expected FSRead variant");
        }
    }

    #[test]
    fn test_capitalized_write_alias() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Test that "Write" (capitalized) is normalized to "write"
        let tool_call = ToolCallFull {
            name: ToolName::new("Write"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/path.rs", "content": "test content"}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse capitalized 'Write' tool name"
        );

        if let Ok(ToolCatalog::Write(fs_write)) = actual {
            assert_eq!(fs_write.file_path, "/test/path.rs");
            assert_eq!(fs_write.content, "test content");
        } else {
            panic!("Expected FSWrite variant");
        }
    }

    #[test]
    fn test_lowercase_read_still_works() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Ensure lowercase still works (backward compatibility)
        let tool_call = ToolCallFull {
            name: ToolName::new("read"),
            call_id: None,
            arguments: ToolCallArguments::from_json(r#"{"path": "/test/path.rs"}"#),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse lowercase 'read' tool name"
        );

        matches!(actual.unwrap(), ToolCatalog::Read(_));
    }

    #[test]
    fn test_lowercase_write_still_works() {
        use crate::{ToolCallArguments, ToolCallFull};

        // Ensure lowercase still works (backward compatibility)
        let tool_call = ToolCallFull {
            name: ToolName::new("write"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/path.rs", "content": "test"}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should successfully parse lowercase 'write' tool name"
        );

        matches!(actual.unwrap(), ToolCatalog::Write(_));
    }

    #[test]
    fn test_contains_with_lowercase() {
        assert!(ToolCatalog::contains(&ToolName::new("read")));
        assert!(ToolCatalog::contains(&ToolName::new("write")));
        assert!(!ToolCatalog::contains(&ToolName::new("nonexistent")));
    }

    #[test]
    fn test_contains_with_capitalized() {
        // Test that capitalized versions are also found
        assert!(
            ToolCatalog::contains(&ToolName::new("Read")),
            "Should contain capitalized 'Read'"
        );
        assert!(
            ToolCatalog::contains(&ToolName::new("Write")),
            "Should contain capitalized 'Write'"
        );
    }

    #[test]
    fn test_fs_search_message_with_regex() {
        use std::path::PathBuf;

        use crate::FSSearch;
        use crate::policies::PermissionOperation;

        let search_with_regex = ToolCatalog::FsSearch(FSSearch {
            path: Some("/home/user/project".to_string()),
            pattern: "fn main".to_string(),
            ..Default::default()
        });

        let operation = search_with_regex
            .to_policy_operation(PathBuf::from("/test/cwd"))
            .unwrap();

        match operation {
            PermissionOperation::Read { message, .. } => {
                assert_eq!(
                    message,
                    "Search in directory/file: `/home/user/project` for pattern: fn main"
                );
            }
            _ => panic!("Expected Read operation"),
        }
    }

    #[test]
    fn test_fs_search_message_without_regex() {
        use std::path::PathBuf;

        use crate::FSSearch;
        use crate::policies::PermissionOperation;

        let search_without_regex = ToolCatalog::FsSearch(FSSearch {
            path: Some("/home/user/project".to_string()),
            pattern: ".*".to_string(), // Match all content
            ..Default::default()
        });

        let operation = search_without_regex
            .to_policy_operation(PathBuf::from("/test/cwd"))
            .unwrap();

        match operation {
            PermissionOperation::Read { message, .. } => {
                assert_eq!(
                    message,
                    "Search in directory/file: `/home/user/project` for pattern: .*"
                );
            }
            _ => panic!("Expected Read operation"),
        }
    }

    #[test]
    fn test_fs_search_message_with_file_pattern_only() {
        use std::path::PathBuf;

        use crate::FSSearch;
        use crate::policies::PermissionOperation;

        let search_with_pattern = ToolCatalog::FsSearch(FSSearch {
            path: Some("/home/user/project".to_string()),
            pattern: ".*".to_string(),
            glob: Some("*.rs".to_string()),
            ..Default::default()
        });

        let operation = search_with_pattern
            .to_policy_operation(PathBuf::from("/test/cwd"))
            .unwrap();

        match operation {
            PermissionOperation::Read { message, .. } => {
                assert_eq!(
                    message,
                    "Search in directory/file: `/home/user/project` for pattern: '.*' in '*.rs' files"
                );
            }
            _ => panic!("Expected Read operation"),
        }
    }

    #[test]
    fn test_fs_search_message_with_regex_and_file_pattern() {
        use std::path::PathBuf;

        use crate::FSSearch;
        use crate::policies::PermissionOperation;

        let search_with_both = ToolCatalog::FsSearch(FSSearch {
            path: Some("/home/user/project".to_string()),
            pattern: "fn main".to_string(),
            glob: Some("*.rs".to_string()),
            ..Default::default()
        });

        let operation = search_with_both
            .to_policy_operation(PathBuf::from("/test/cwd"))
            .unwrap();

        match operation {
            PermissionOperation::Read { message, .. } => {
                assert_eq!(
                    message,
                    "Search in directory/file: `/home/user/project` for pattern: 'fn main' in '*.rs' files"
                );
            }
            _ => panic!("Expected Read operation"),
        }
    }

    #[test]
    fn test_fs_patch_backward_compatibility_path() {
        use crate::{ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("patch"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/file.rs", "new_string": "new", "old_string": "old"}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call).unwrap();

        let expected = ToolCatalog::Patch(crate::FSPatch::new("/test/file.rs", "old", "new"));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_backward_compatibility_all_old_fields() {
        use crate::{ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("patch"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"path": "/test/file.rs", "content": "new content", "search": "old text"}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call).unwrap();

        let expected = ToolCatalog::Patch(crate::FSPatch::new(
            "/test/file.rs",
            "old text",
            "new content",
        ));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_with_replace_all() {
        use crate::{ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("patch"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"file_path": "/test/file.rs", "new_string": "new", "old_string": "old", "replace_all": true}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call).unwrap();

        let expected = ToolCatalog::Patch(
            crate::FSPatch::new("/test/file.rs", "old", "new").replace_all(true),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_edits_form() {
        use crate::{PatchEdit, ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("patch"),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"file_path": "/a.rs", "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d", "replace_all": true}]}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call).unwrap();

        let expected = ToolCatalog::Patch(crate::FSPatch::batch(
            "/a.rs",
            vec![
                PatchEdit::new("a", "b"),
                PatchEdit::new("c", "d").replace_all(true),
            ],
        ));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_resolve_edits_scalar() {
        let fixture = crate::FSPatch::new("/a.rs", "x", "y").replace_all(true);
        let actual = fixture.resolve_edits().unwrap();
        let expected = vec![crate::PatchEdit::new("x", "y").replace_all(true)];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_resolve_edits_rejects_both_forms() {
        let mut fixture = crate::FSPatch::new("/a.rs", "x", "y");
        fixture.edits = Some(vec![crate::PatchEdit::new("a", "b")]);
        let actual = fixture.resolve_edits().unwrap_err().to_string();
        let expected =
            "pass either the scalar form (old_string/new_string/replace_all) or edits, not both";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_fs_patch_resolve_edits_rejects_empty_and_missing() {
        let empty = crate::FSPatch::batch("/a.rs", vec![]);
        let missing = crate::FSPatch { file_path: "/a.rs".to_string(), ..Default::default() };
        let actual = (
            empty.resolve_edits().unwrap_err().to_string(),
            missing.resolve_edits().unwrap_err().to_string(),
        );
        let expected = (
            "edits must be a non-empty list of {old_string, new_string} objects".to_string(),
            "old_string and new_string are required (or pass edits)".to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unit_enum_schema_generation() {
        use schemars::generate::SchemaSettings;

        use crate::OutputMode;

        let settings = SchemaSettings::default().into_generator();
        let output_schema = settings.into_root_schema_for::<OutputMode>();

        // Verify it also generates a simple string enum
        let schema_value = output_schema.as_value();
        assert_eq!(schema_value.get("type"), Some(&serde_json::json!("string")));

        let enum_values = schema_value.get("enum").and_then(|v| v.as_array()).unwrap();
        assert_eq!(enum_values.len(), 3);
        assert_eq!(enum_values[0], serde_json::json!("content"));
        assert_eq!(enum_values[1], serde_json::json!("files_with_matches"));
        assert_eq!(enum_values[2], serde_json::json!("count"));
    }

    #[test]
    fn test_shell_with_description_serialization() {
        use pretty_assertions::assert_eq;

        let fixture = Shell {
            command: "git status".to_string(),
            cwd: Some(PathBuf::from("/test")),
            description: Some("Shows working tree status".to_string()),
            ..Default::default()
        };

        let actual = serde_json::to_value(&fixture).unwrap();

        let expected = serde_json::json!({
            "command": "git status",
            "cwd": "/test",
            "description": "Shows working tree status"
        });

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_shell_without_description_serialization() {
        use pretty_assertions::assert_eq;

        let fixture = Shell {
            command: "ls -la".to_string(),
            cwd: Some(PathBuf::from("/home")),
            ..Default::default()
        };

        let actual = serde_json::to_value(&fixture).unwrap();

        let expected = serde_json::json!({
            "command": "ls -la",
            "cwd": "/home"
        });

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_shell_without_cwd_serialization() {
        use pretty_assertions::assert_eq;

        let fixture = Shell { command: "pwd".to_string(), ..Default::default() };

        let actual = serde_json::to_value(&fixture).unwrap();

        let expected = serde_json::json!({
            "command": "pwd"
        });

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_normalize_tool_name_trims_whitespace() {
        let actual = super::normalize_tool_name(&ToolName::new("  read  "));
        let expected = ToolName::new("read");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_normalize_tool_name_case_insensitive_uppercase() {
        let actual = super::normalize_tool_name(&ToolName::new("READ"));
        let expected = ToolName::new("read");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_normalize_tool_name_case_insensitive_mixed() {
        let actual = super::normalize_tool_name(&ToolName::new("FS_SEARCH"));
        let expected = ToolName::new("fs_search");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_normalize_tool_name_trim_and_case_insensitive() {
        let actual = super::normalize_tool_name(&ToolName::new("  SHELL  "));
        let expected = ToolName::new("shell");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_normalize_tool_name_unknown_returns_trimmed() {
        let actual = super::normalize_tool_name(&ToolName::new("  unknown_tool  "));
        let expected = ToolName::new("unknown_tool");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_contains_case_insensitive() {
        assert!(ToolCatalog::contains(&ToolName::new("READ")));
        assert!(ToolCatalog::contains(&ToolName::new("Shell")));
        assert!(ToolCatalog::contains(&ToolName::new("PATCH")));
        assert!(!ToolCatalog::contains(&ToolName::new("nonexistent")));
    }

    #[test]
    fn test_contains_with_whitespace() {
        assert!(ToolCatalog::contains(&ToolName::new("  read  ")));
        assert!(ToolCatalog::contains(&ToolName::new(" shell ")));
    }

    #[test]
    fn test_try_from_tool_call_uppercase_name() {
        use crate::{ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("SHELL"),
            call_id: None,
            arguments: ToolCallArguments::from_json(r#"{"command": "ls"}"#),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(actual.is_ok(), "Should parse uppercase 'SHELL' tool name");
        assert!(matches!(actual.unwrap(), ToolCatalog::Shell(_)));
    }

    #[test]
    fn test_try_from_tool_call_with_whitespace_name() {
        use crate::{ToolCallArguments, ToolCallFull};

        let tool_call = ToolCallFull {
            name: ToolName::new("  patch  "),
            call_id: None,
            arguments: ToolCallArguments::from_json(
                r#"{"file_path": "/test/file.rs", "new_string": "new", "old_string": "old"}"#,
            ),
            thought_signature: None,
        };

        let actual = ToolCatalog::try_from(tool_call);

        assert!(
            actual.is_ok(),
            "Should parse whitespace-padded 'patch' tool name"
        );
        assert!(matches!(actual.unwrap(), ToolCatalog::Patch(_)));
    }
}
