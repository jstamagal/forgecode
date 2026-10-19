use std::collections::BTreeMap;

use fake::Dummy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// X18: one argument ban (`[[tool_bans.shell]] arg = "command" pattern =
/// "rm -rf"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct ToolBan {
    /// Argument name to test; absent or `*` tests every string argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    /// Regex searched in the argument's value.
    pub pattern: String,
    /// What the model reads when the ban applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Events a user hook can run on (X19). js's events that have a forgecode
/// counterpart; the input/stream/MCP-notification events have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool call runs; exit status 2 refuses the call.
    ToolCall,
    /// After a tool call returned.
    ToolResult,
    /// When a turn starts (a user message).
    TurnStart,
    /// When a turn ends.
    TurnEnd,
    /// After each model response.
    Response,
    /// A tool call failed.
    Error,
    /// Before the conversation is compacted.
    PreCompact,
    /// After the conversation was compacted.
    PostCompact,
    /// When forge starts a session (interactive or `-p`).
    SessionStart,
    /// When forge exits.
    SessionEnd,
}

impl HookEvent {
    /// The `hook_event_name` sent to the command.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::ToolCall => "tool_call",
            HookEvent::ToolResult => "tool_result",
            HookEvent::TurnStart => "turn_start",
            HookEvent::TurnEnd => "turn_end",
            HookEvent::Response => "response",
            HookEvent::Error => "error",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostCompact => "post_compact",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
        }
    }
}

/// A user hook (X19): `command` runs under `sh -c` with the event as one JSON
/// object on stdin (Claude Code's protocol). Under `tool_call`, exit status 2
/// refuses the call and the first line of stderr (else stdout) is what the
/// model reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct UserHook {
    pub event: HookEvent,
    /// Shell command to run.
    pub command: String,
    /// Only for these tools (exact names); empty means every tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Seconds before the hook's process group is killed (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Model-facing tool names for models whose id or provider matches (X23).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct ToolAliasProfile {
    /// Case-insensitive substrings tested against the model id and the
    /// provider id; the first profile with a hit applies.
    #[serde(rename = "match")]
    pub matches: Vec<String>,
    /// Canonical tool name → name shown to the model (e.g. `read = "Read"`).
    pub aliases: BTreeMap<String, String>,
}

/// Allow/deny glob lists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct AllowDeny {
    /// Globs that are allowed; absent allows everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Globs that are denied; deny wins over allow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl AllowDeny {
    /// Whether `name` passes: not denied, and allowed (absent `allow` allows
    /// everything).
    pub fn permits(&self, name: &str) -> bool {
        let hit = |globs: &[String]| {
            globs.iter().any(|glob| {
                glob::Pattern::new(glob)
                    .map(|p| p.matches(name))
                    .unwrap_or(glob == name)
            })
        };
        if hit(&self.deny) {
            return false;
        }
        self.allow.as_deref().is_none_or(hit)
    }
}

/// MCP policy for one agent (M4): which servers and which tools (by their
/// `{server}__{tool}` name) it may use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct McpAgentPolicy {
    #[serde(default)]
    pub servers: AllowDeny,
    #[serde(default)]
    pub tools: AllowDeny,
}

/// `[mcp]` settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct McpSettings {
    /// Per-agent server and tool policy, keyed by agent id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, McpAgentPolicy>,
}

impl McpSettings {
    /// Whether `agent` may use tool `name` of `server`.
    pub fn permits(&self, agent: &str, server: &str, name: &str) -> bool {
        self.agents
            .get(agent)
            .is_none_or(|policy| policy.servers.permits(server) && policy.tools.permits(name))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_deny_wins_and_absent_allow_allows_all() {
        let fixture = McpSettings {
            agents: [(
                "sage".to_string(),
                McpAgentPolicy {
                    servers: AllowDeny { allow: Some(vec!["git*".to_string()]), deny: vec![] },
                    tools: AllowDeny { allow: None, deny: vec!["*__delete_*".to_string()] },
                },
            )]
            .into(),
        };
        let actual = (
            fixture.permits("sage", "github", "github__list_issues"),
            fixture.permits("sage", "github", "github__delete_repo"),
            fixture.permits("sage", "jira", "jira__list"),
            fixture.permits("forge", "jira", "jira__delete_all"),
        );
        assert_eq!(actual, (true, false, false, true));
    }
}
