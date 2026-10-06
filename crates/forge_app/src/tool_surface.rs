//! Builds one request's tool surface (T24, X1, X21, X23).
//!
//! Every tool the process knows (native catalog, agents-as-tools, MCP) is
//! resolved against the agent's `tools:` chain; descriptions are rendered
//! against the tools actually co-present on the surface; an alias profile
//! renames tools for models that expect other names.

use std::collections::HashSet;

use forge_domain::{
    Agent, AliasMap, CatalogEntry, EntryKind, McpServers, Skill, SurfaceTool, ToolCatalog,
    ToolDefinition, ToolMeta, ToolMode, ToolName, ToolSource, select_tools,
};
use serde_json::{Map, Value, json};
use strum::IntoEnumIterator;

/// Native tools that are eager unless an agent says otherwise.
fn default_mode(name: &str, source: &ToolSource) -> ToolMode {
    match source {
        ToolSource::Native if forge_domain::DEFAULT_EAGER_TOOLS.contains(&name) => ToolMode::Eager,
        // An agent-as-tool is a delegation target the user configured.
        ToolSource::Agent => ToolMode::Eager,
        _ => ToolMode::Lazy,
    }
}

/// Whether calls to a tool can be read-only (for `tag:read_only`).
fn can_be_read_only(name: &str) -> bool {
    matches!(
        name,
        "read" | "fs_search" | "ast_search" | "fetch" | "skill" | "todo_read" | "tool_discovery"
    )
}

/// The tool every agent definition may name, with its source.
#[derive(Debug, Clone)]
pub struct KnownTool {
    pub definition: ToolDefinition,
    pub source: ToolSource,
}

/// Every tool the process knows, in a stable order.
pub fn known_tools(
    native: Vec<ToolDefinition>,
    agents: Vec<ToolDefinition>,
    mcp: &McpServers,
) -> Vec<KnownTool> {
    let native_names: HashSet<String> = native.iter().map(|d| d.name.to_string()).collect();
    let mut tools: Vec<KnownTool> = native
        .into_iter()
        .map(|definition| KnownTool { definition, source: ToolSource::Native })
        .collect();
    // T23: an agent whose id collides with a built-in tool is skipped.
    for definition in agents {
        if native_names.contains(definition.name.as_str()) {
            tracing::warn!(
                agent = %definition.name,
                "agent id collides with a built-in tool name; the agent is not offered as a tool"
            );
            continue;
        }
        tools.push(KnownTool { definition, source: ToolSource::Agent });
    }
    let mut servers: Vec<_> = mcp.get_servers().iter().collect();
    servers.sort_by_key(|(server, _)| server.to_string());
    for (server, definitions) in servers {
        for definition in definitions {
            tools.push(KnownTool {
                definition: definition.clone(),
                source: ToolSource::Mcp(server.to_string()),
            });
        }
    }
    tools
}

fn meta(tool: &KnownTool) -> ToolMeta {
    let name = tool.definition.name.as_str();
    let alternate_names = match (&tool.source, name.split_once("__")) {
        // M1: agent files written for the old `mcp_{server}_tool_{tool}` form
        // still match.
        (ToolSource::Mcp(_), Some((server, remote))) => {
            vec![
                format!("mcp_{server}_tool_{remote}"),
                format!("mcp__{name}"),
            ]
        }
        _ => vec![],
    };
    ToolMeta {
        name: tool.definition.name.clone(),
        alternate_names,
        read_only: can_be_read_only(name),
        default_mode: default_mode(name, &tool.source),
    }
}

/// Resolves `agent`'s `tools:` chain against `tools`. Warnings (unmatched or
/// malformed entries) are logged.
///
/// `tool_discovery` joins the surface on its own whenever there is something
/// to discover (a lazy tool, or MCP servers / skills the caller adds) unless
/// an entry bans it; publication then decides whether it is offered.
pub fn resolve_surface(agent: &Agent, tools: &[KnownTool]) -> Vec<SurfaceTool> {
    let entries: Vec<String> = agent
        .tools
        .iter()
        .flatten()
        .map(|name| name.to_string())
        .collect();
    let metas: Vec<ToolMeta> = tools.iter().map(meta).collect();
    let selection = select_tools(&entries, &metas);
    for warning in &selection.warnings {
        tracing::warn!(agent = %agent.id, "{warning}");
    }
    let banned_discovery = selection
        .decisions
        .iter()
        .any(|d| d.tool.as_str() == forge_domain::TOOL_DISCOVERY && d.mode == Some(ToolMode::Ban));
    let mut surface: Vec<SurfaceTool> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.definition.name.as_str();
            let mode = selection.mode(name).or_else(|| {
                (name == forge_domain::TOOL_DISCOVERY && !banned_discovery)
                    .then_some(ToolMode::Eager)
            });
            mode.map(|mode| SurfaceTool {
                definition: tool.definition.clone(),
                mode,
                source: tool.source.clone(),
            })
        })
        .collect();
    let order = agent.tool_order();
    let mut definitions: Vec<&ToolDefinition> = surface.iter().map(|t| &t.definition).collect();
    order.sort_refs(&mut definitions);
    let ranked: Vec<ToolName> = definitions.iter().map(|d| d.name.clone()).collect();
    surface.sort_by_key(|tool| {
        ranked
            .iter()
            .position(|name| name == &tool.definition.name)
            .unwrap_or(usize::MAX)
    });
    surface
}

/// The agent's `tools:` table for display: every tool, its resolved mode and
/// the entry that decided it, plus warnings.
pub fn surface_table(agent: &Agent, tools: &[KnownTool]) -> forge_domain::Selection {
    let entries: Vec<String> = agent
        .tools
        .iter()
        .flatten()
        .map(|name| name.to_string())
        .collect();
    let metas: Vec<ToolMeta> = tools.iter().map(meta).collect();
    select_tools(&entries, &metas)
}

/// Template data naming the co-present tools: `tool_names.<canonical>` is
/// the model-facing name of every tool on the surface (eager or lazy), so a
/// description's `{{#if tool_names.x}}` block shows only when `x` is there.
pub fn tool_names_map(surface: &[SurfaceTool], aliases: &AliasMap) -> Map<String, Value> {
    surface
        .iter()
        .map(|tool| {
            let name = tool.definition.name.to_string();
            let shown = aliases.get(&name).cloned().unwrap_or_else(|| name.clone());
            (name, json!(shown))
        })
        .collect()
}

/// Renders every native description against the surface's tool names. A
/// render failure is logged and the raw description kept (X1).
pub fn render_native_descriptions(surface: &mut [SurfaceTool], data: &forge_domain::SystemContext) {
    let handlebars = crate::TemplateEngine::handlebar_instance();
    for tool in surface.iter_mut() {
        if tool.source != ToolSource::Native {
            continue;
        }
        match handlebars.render_template(&tool.definition.description, data) {
            Ok(rendered) => tool.definition.description = rendered,
            Err(error) => tracing::warn!(
                tool = %tool.definition.name,
                %error,
                "tool description failed to render"
            ),
        }
    }
}

/// Picks the alias map of the first profile whose `match` substrings hit the
/// model or provider id (case-insensitive); aliases that would collide with
/// another tool's name are dropped.
pub fn resolve_alias_profile(
    profiles: &[forge_config::ToolAliasProfile],
    model: &str,
    provider: &str,
    surface: &[SurfaceTool],
) -> AliasMap {
    let haystacks = [model.to_lowercase(), provider.to_lowercase()];
    let names: HashSet<String> = surface
        .iter()
        .map(|t| t.definition.name.to_string())
        .collect();
    for profile in profiles {
        let hit = profile
            .matches
            .iter()
            .map(|m| m.trim().to_lowercase())
            .filter(|m| !m.is_empty())
            .any(|needle| haystacks.iter().any(|h| h.contains(&needle)));
        if !hit {
            continue;
        }
        let usable: AliasMap = profile
            .aliases
            .iter()
            .filter(|(canonical, alias)| {
                names.contains(canonical.as_str())
                    && !alias.trim().is_empty()
                    && alias.as_str() != forge_domain::TOOL_DISCOVERY
                    && !names
                        .iter()
                        .any(|n| n != *canonical && n.eq_ignore_ascii_case(alias))
            })
            .map(|(c, a)| (c.clone(), a.clone()))
            .collect();
        if !usable.is_empty() {
            return usable;
        }
    }
    AliasMap::new()
}

/// Rewrites published definitions to their aliases: the name, and
/// backtick-quoted canonical names inside every description.
pub fn apply_aliases(definitions: Vec<ToolDefinition>, aliases: &AliasMap) -> Vec<ToolDefinition> {
    if aliases.is_empty() {
        return definitions;
    }
    definitions
        .into_iter()
        .map(|mut definition| {
            for (canonical, alias) in aliases {
                definition.description = definition
                    .description
                    .replace(&format!("`{canonical}`"), &format!("`{alias}`"));
            }
            if let Some(alias) = aliases.get(definition.name.as_str()) {
                definition.name = ToolName::new(alias);
            }
            definition
        })
        .collect()
}

/// The canonical name of a model-facing name under `aliases`.
pub fn canonical_name(name: &ToolName, aliases: &AliasMap) -> ToolName {
    aliases
        .iter()
        .find(|(_, alias)| alias.as_str() == name.as_str())
        .map(|(canonical, _)| ToolName::new(canonical))
        .unwrap_or_else(|| name.clone())
}

/// Catalog rows for skills the model may load (not user-only skills).
pub fn skill_rows(skills: &[Skill]) -> Vec<CatalogEntry> {
    skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .map(|skill| CatalogEntry {
            id: format!("skill:{}", skill.name),
            name: skill.name.clone(),
            kind: EntryKind::Skill,
            source: "skill".to_string(),
            description: forge_domain::summary(&skill.description),
            search_text: String::new(),
            loadable: true,
        })
        .collect()
}

/// All native catalog definitions, unrendered.
pub fn native_definitions() -> Vec<ToolDefinition> {
    ToolCatalog::iter().map(|tool| tool.definition()).collect()
}

#[cfg(test)]
mod tests {
    use forge_domain::{AgentId, ModelId, ProviderId};
    use pretty_assertions::assert_eq;

    use super::*;

    fn agent(tools: &[&str]) -> Agent {
        Agent::new(AgentId::new("a"), ProviderId::ANTHROPIC, ModelId::new("m"))
            .tools(tools.iter().map(ToolName::new).collect::<Vec<_>>())
    }

    fn known(names: &[&str]) -> Vec<KnownTool> {
        names
            .iter()
            .map(|n| KnownTool {
                definition: ToolDefinition::new(n),
                source: ToolSource::Native,
            })
            .collect()
    }

    fn modes(surface: &[SurfaceTool]) -> Vec<(String, ToolMode)> {
        surface
            .iter()
            .map(|t| (t.definition.name.to_string(), t.mode))
            .collect()
    }

    #[test]
    fn test_surface_follows_agent_order_and_defaults() {
        let tools = known(&["read", "shell", "ast_search", "tool_discovery"]);
        let actual = modes(&resolve_surface(
            &agent(&["shell", "ast_search", "read"]),
            &tools,
        ));
        // tool_discovery is not named but joins the surface on its own.
        let expected = vec![
            ("shell".to_string(), ToolMode::Eager),
            ("ast_search".to_string(), ToolMode::Lazy),
            ("read".to_string(), ToolMode::Eager),
            ("tool_discovery".to_string(), ToolMode::Eager),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_banned_tool_discovery_stays_off() {
        let tools = known(&["read", "tool_discovery"]);
        let actual = modes(&resolve_surface(
            &agent(&["read", "tool_discovery:ban"]),
            &tools,
        ));
        assert_eq!(actual, vec![("read".to_string(), ToolMode::Eager)]);
    }

    #[test]
    fn test_agent_colliding_with_native_tool_is_skipped() {
        let actual: Vec<String> = known_tools(
            vec![ToolDefinition::new("read")],
            vec![ToolDefinition::new("read"), ToolDefinition::new("sage")],
            &McpServers::default(),
        )
        .into_iter()
        .map(|t| t.definition.name.to_string())
        .collect();
        assert_eq!(actual, vec!["read", "sage"]);
    }

    #[test]
    fn test_co_presence_blocks_render_against_surface() {
        let mut surface = vec![
            SurfaceTool {
                definition: ToolDefinition::new("read").description(
                    "Read.{{#if tool_names.fs_search}} Find with `{{tool_names.fs_search}}`.{{/if}}{{#unless tool_names.fs_search}} No search here.{{/unless}}",
                ),
                mode: ToolMode::Eager,
                source: ToolSource::Native,
            },
        ];
        let data = forge_domain::SystemContext {
            tool_names: tool_names_map(&surface, &AliasMap::new()),
            ..Default::default()
        };
        render_native_descriptions(&mut surface, &data);
        assert_eq!(surface[0].definition.description, "Read. No search here.");
    }

    #[test]
    fn test_alias_profile_renames_and_maps_back() {
        let surface = vec![SurfaceTool {
            definition: ToolDefinition::new("read").description("Use `read` first."),
            mode: ToolMode::Eager,
            source: ToolSource::Native,
        }];
        let profiles = vec![forge_config::ToolAliasProfile {
            matches: vec!["claude".to_string()],
            aliases: [("read".to_string(), "Read".to_string())].into(),
        }];
        let aliases = resolve_alias_profile(&profiles, "claude-sonnet", "anthropic", &surface);
        let published = apply_aliases(vec![surface[0].definition.clone()], &aliases);
        assert_eq!(published[0].name.as_str(), "Read");
        assert_eq!(published[0].description, "Use `Read` first.");
        assert_eq!(
            canonical_name(&ToolName::new("Read"), &aliases).as_str(),
            "read"
        );
        assert_eq!(
            resolve_alias_profile(&profiles, "gpt-5", "openai", &surface),
            AliasMap::new()
        );
    }
}
