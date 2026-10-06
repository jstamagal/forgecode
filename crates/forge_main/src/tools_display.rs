use std::collections::HashMap;

use convert_case::{Case, Casing};
use forge_api::{Selection, ToolName, ToolsOverview};

use crate::info::Info;

/// Formats the tools overview for display: each tool with the mode the
/// agent's `tools:` chain gives it (eager, lazy, ban, or `-` when no entry
/// names it) and the entry that decided it, then any warnings about entries
/// that name no tool (X21).
pub fn format_tools(selection: &Selection, overview: &ToolsOverview) -> Info {
    let mut info = Info::new();
    let decided: HashMap<&ToolName, (&str, String)> = selection
        .decisions
        .iter()
        .map(|d| {
            let mode = d
                .mode
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".to_string());
            let by = d.decided_by.as_deref().unwrap_or("");
            (&d.tool, (by, mode))
        })
        .collect();
    let row = |tool_name: &ToolName| -> String {
        match decided.get(tool_name) {
            Some((by, mode)) if !by.is_empty() => format!("[{mode:<5}] {tool_name}  ({by})"),
            Some((_, mode)) => format!("[{mode:<5}] {tool_name}"),
            None => format!("[-    ] {tool_name}"),
        }
    };

    // System tools section
    info = info.add_title("SYSTEM");
    for tool in &overview.system {
        info = info.add_value(row(&tool.name));
    }

    // Agents section
    info = info.add_title("AGENTS");
    for tool in &overview.agents {
        info = info.add_value(row(&tool.name));
    }

    // MCP tools section
    if !overview.mcp.get_servers().is_empty() {
        for (server_name, tools) in overview.mcp.get_servers().iter() {
            let title = (*server_name).to_case(Case::UpperSnake);
            info = info.add_title(title);

            for tool in tools {
                info = info.add_value(row(&tool.name));
            }
        }
    }

    // Failed MCP servers section
    if !overview.mcp.get_failures().is_empty() {
        info = info.add_title("FAILED MCP SERVERS");
        for (server_name, error) in overview.mcp.get_failures().iter() {
            // Truncate error message for readability in list view
            // Use 'mcp show <name>' for full error details
            let truncated_error = truncate_error(error);
            info = info.add_value(format!("[✗] {server_name} - {truncated_error}"));
        }
    }

    if !selection.warnings.is_empty() {
        info = info.add_title("WARNINGS");
        for warning in &selection.warnings {
            info = info.add_value(warning.clone());
        }
    }

    info
}

/// X26: description and schema bytes per tool, with an approximate token
/// count (4 bytes per token) and totals.
pub fn format_tool_costs(tools: &[forge_api::ToolDefinition]) -> Info {
    let mut info = Info::new().add_title("TOOL COST (bytes: description + schema ≈ tokens)");
    let mut total = 0usize;
    for tool in tools {
        let description = tool.description.len();
        let schema = serde_json::to_string(&tool.input_schema)
            .map(|s| s.len())
            .unwrap_or_default();
        total += description + schema;
        info = info.add_key_value(
            tool.name.to_string(),
            format!(
                "{description} + {schema} = {} (≈{} tokens)",
                description + schema,
                (description + schema).div_ceil(4)
            ),
        );
    }
    info.add_key_value(
        "TOTAL",
        format!(
            "{total} bytes (≈{} tokens) over {} tools",
            total.div_ceil(4),
            tools.len()
        ),
    )
}

/// Truncates an error message to at most 80 characters for display.
///
/// If the message exceeds 80 characters, the first 77 characters are kept
/// followed by "...". Uses character-based counting to avoid panicking on
/// multi-byte UTF-8 strings.
fn truncate_error(error: &str) -> String {
    if error.chars().count() > 80 {
        let truncated: String = error.chars().take(77).collect();
        format!("{truncated}...")
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_truncate_error_short_message() {
        let fixture = "Connection refused";
        let actual = truncate_error(fixture);
        let expected = "Connection refused";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_truncate_error_long_ascii_message() {
        let fixture = "A".repeat(100);
        let actual = truncate_error(&fixture);
        assert_eq!(actual.chars().count(), 80);
        assert!(actual.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_multibyte_chars_no_panic() {
        // Error containing multi-byte UTF-8 chars (→ is 3 bytes)
        let fixture = "Error: struct → prioritizes struct definitions, trait → prioritizes traits, impl → prioritizes impls, and more details follow here";
        let actual = truncate_error(fixture);
        // Should not panic and should truncate correctly by char count
        assert!(actual.chars().count() <= 80);
        assert!(actual.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_emoji_no_panic() {
        // Error containing 4-byte emojis - 90 emoji chars > 80 limit
        let fixture = "🚀".repeat(90);
        let actual = truncate_error(&fixture);
        assert_eq!(actual.chars().count(), 80);
        assert!(actual.ends_with("..."));
    }

    #[test]
    fn test_truncate_error_exactly_80_chars() {
        let fixture = "A".repeat(80);
        let actual = truncate_error(&fixture);
        let expected = "A".repeat(80);
        assert_eq!(actual, expected);
    }
}
