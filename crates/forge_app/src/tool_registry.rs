use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use console::style;
use forge_domain::{
    Agent, AgentId, AgentInput, ChatResponse, ChatResponseContent, Environment, InputModality,
    Model, SystemContext, TemplateConfig, ToolCallContext, ToolCallFull, ToolCatalog,
    ToolDefinition, ToolName, ToolOutput, ToolResult,
};
use forge_template::Element;
use futures::future::join_all;
use serde_json::{Map, Value, json};
use strum::IntoEnumIterator;
use tokio::time::timeout;

use crate::agent_executor::AgentExecutor;
use crate::dto::ToolsOverview;
use crate::error::Error;
use crate::fmt::content::FormatContent;
use crate::mcp_executor::McpExecutor;
use crate::task_jobs::TaskJobs;
use crate::tool_executor::ToolExecutor;
use crate::{
    AgentRegistry, CapabilityService, ConversationService, EnvironmentInfra, McpConfigManager,
    McpService, PolicyService, ProviderService, Services, SkillFetchService,
};

/// Background task runs live as long as the process: the registry is rebuilt
/// for every call, and a later call (poll, the completion note) must find
/// them.
pub(crate) fn task_jobs() -> &'static TaskJobs {
    static JOBS: std::sync::OnceLock<TaskJobs> = std::sync::OnceLock::new();
    JOBS.get_or_init(TaskJobs::default)
}

/// One worker's reply is returned verbatim; a fan-out is numbered in task
/// order under one header. A failed worker reports its error in its own slot.
/// Each slot is held to an equal share of `budget` (0 = no cap) so one large
/// reply cannot crowd out its siblings. The worker's session id is named so
/// the model can resume it.
fn assemble_task_results(
    results: Vec<anyhow::Result<ToolOutput>>,
    agent_id: &str,
    budget: usize,
) -> String {
    let count = results.len().max(1);
    let share = if budget > 0 { budget / count } else { 0 };
    let texts: Vec<String> = results
        .into_iter()
        .map(|result| {
            let text = match result {
                Ok(output) => {
                    let session = output.values.iter().find_map(|value| match value {
                        forge_domain::ToolValue::AI { conversation_id, .. } => {
                            Some(conversation_id.to_string())
                        }
                        _ => None,
                    });
                    let body = output
                        .values
                        .iter()
                        .filter_map(forge_domain::ToolValue::as_str)
                        .collect::<Vec<_>>()
                        .join("\n");
                    match session {
                        Some(id) => format!("{body}\n(session_id={id})"),
                        None => body,
                    }
                }
                Err(error) => format!("ERROR {error:#}"),
            };
            if share > 0 && text.len() > share {
                let marker =
                    format!("\n[truncated: equal share ({share} bytes) of max_tool_result_bytes]");
                let mut end = share.saturating_sub(marker.len());
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}{marker}", &text[..end])
            } else {
                text
            }
        })
        .collect();
    if texts.len() == 1 {
        return texts.into_iter().next().unwrap_or_default();
    }
    let numbered: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(index, text)| format!("{}. {text}", index + 1))
        .collect();
    format!("TASK_RESULTS agent={agent_id}\n\n{}", numbered.join("\n\n"))
}

pub struct ToolRegistry<S> {
    tool_executor: ToolExecutor<S>,
    agent_executor: AgentExecutor<S>,
    mcp_executor: McpExecutor<S>,
    services: Arc<S>,
}

impl<S: Services + EnvironmentInfra<Config = forge_config::ForgeConfig>> ToolRegistry<S> {
    pub fn new(services: Arc<S>) -> Self {
        Self {
            services: services.clone(),
            tool_executor: ToolExecutor::new(services.clone()),
            agent_executor: AgentExecutor::new(services.clone()),
            mcp_executor: McpExecutor::new(services.clone()),
        }
    }

    async fn call_with_timeout<F, Fut>(
        &self,
        tool_name: &ToolName,
        future: F,
    ) -> anyhow::Result<ToolOutput>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<ToolOutput>>,
    {
        // X7: a blanket backstop for tools that do not bound their own run
        // time. `tool_timeout_secs` (default 600) of 0 disables it.
        let secs = self.services.get_config()?.tool_timeout_secs;
        if secs == 0 {
            return future().await;
        }
        let tool_timeout = Duration::from_secs(secs);
        timeout(tool_timeout, future())
            .await
            .context(Error::CallTimeout {
                timeout: tool_timeout.as_secs(),
                tool_name: tool_name.clone(),
            })?
    }

    /// Check if a tool operation is allowed based on the workflow policies
    async fn check_tool_permission(
        &self,
        tool_input: &ToolCatalog,
        context: &ToolCallContext,
    ) -> anyhow::Result<bool> {
        let cwd = self.services.get_environment().cwd;
        let operation = tool_input.to_policy_operation(cwd.clone());
        if let Some(operation) = operation {
            let decision = self.services.check_operation_permission(&operation).await?;

            // Send custom policy message to the user when a policy file was
            // created
            if let Some(policy_path) = decision.path {
                use forge_domain::TitleFormat;

                use crate::utils::format_display_path;
                context
                    .send_tool_input(
                        TitleFormat::debug("Permissions Update")
                            .sub_title(format_display_path(policy_path.as_path(), &cwd)),
                    )
                    .await?;
            }
            if !decision.allowed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn call_inner(
        &self,
        agent: &Agent,
        input: ToolCallFull,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        Self::validate_tool_call(agent, &input.name, context)?;

        // X18: argument ban patterns are refused before anything runs (and
        // are refusals: they do not count toward the failure limit).
        let config = self.services.get_config()?;
        if let Some(bans) = config.tool_bans.get(input.name.as_str())
            && let Ok(arguments) = input.arguments.parse()
            && let Some(refusal) = forge_domain::ban_refusal(
                input.name.as_str(),
                &arguments,
                bans.iter().map(|ban| forge_domain::Ban {
                    arg: ban.arg.as_deref(),
                    pattern: &ban.pattern,
                    reason: ban.reason.as_deref(),
                }),
            )
        {
            return Err(anyhow::Error::new(forge_domain::ToolRefusal(refusal)));
        }
        // X19: `tool_call` hooks may refuse the call (exit 2).
        if let Some(refusal) = self.run_tool_call_hooks(agent, &input, context).await {
            return Err(anyhow::Error::new(forge_domain::ToolRefusal(refusal)));
        }

        tracing::info!(tool_name = %input.name, arguments = %input.arguments.clone().into_string(), "Executing tool call");
        let tool_name = input.name.clone();

        // First, try to call a Forge tool
        if ToolCatalog::contains(&input.name) {
            let tool_input: ToolCatalog = ToolCatalog::try_from(input)?;

            // Special handling for Task tool - delegate to AgentExecutor
            if let ToolCatalog::Task(task_input) = tool_input {
                return self.run_task(task_input, context).await;
            }
            // tool_discovery works on the request's surface.
            if let ToolCatalog::ToolDiscovery(discovery) = tool_input {
                return self.run_discovery(agent, discovery, context).await;
            }
            // T16: a kernel's cells call tools back through this registry.
            if let ToolCatalog::Kernel(kernel) = tool_input {
                return self.run_kernel(agent, kernel, context).await;
            }

            let env = self.services.get_environment();
            if let Some(content) = tool_input.to_content(&env) {
                context.send(content).await?;
            }

            // Check permissions before executing the tool (only in restricted
            // mode) This is done BEFORE the timeout to ensure
            // permissions are never timed out
            let is_restricted = self.services.get_config()?.restricted;
            if is_restricted && self.check_tool_permission(&tool_input, context).await? {
                // Send formatted output message for policy denial
                context
                    .send(forge_domain::TitleFormat::error("Permission Denied"))
                    .await?;

                return Ok(ToolOutput::text(
                    Element::new("permission_denied")
                        .cdata("User has denied the permission to execute this tool"),
                ));
            }

            // Images come back as images only for models with image input;
            // the read service falls back to text (PDF text, image stub)
            // otherwise. Only resolve the model when a visual file is read.
            let context = if matches!(&tool_input, ToolCatalog::Read(input) if Self::has_image_extension(&input.file_path))
            {
                let model = self.get_current_model().await;
                context
                    .clone()
                    .with_vision(Self::supports_images(model.as_ref()))
            } else {
                context.clone()
            };

            if tool_input.has_own_timeout() {
                self.tool_executor.execute(tool_input, &context).await
            } else {
                self.call_with_timeout(&tool_name, || {
                    self.tool_executor.execute(tool_input, &context)
                })
                .await
            }
        } else if self.agent_executor.contains_tool(&input.name).await? {
            // Handle agent delegation tool calls
            let agent_input = AgentInput::try_from(&input)?;
            let executor = self.agent_executor.clone();
            let agent_name = input.name.as_str().to_string();
            // NOTE: Agents should not timeout
            let outputs = join_all(agent_input.tasks.into_iter().map(|task| {
                let agent_name = agent_name.clone();
                let executor = executor.clone();
                async move {
                    executor
                        .execute(AgentId::new(&agent_name), task, context, None)
                        .await
                }
            }))
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(ToolOutput::from(outputs.into_iter()))
        } else if self.mcp_executor.contains_tool(&input.name).await? {
            // MCP calls are bounded by the protocol client's own request
            // window; the blanket backstop does not apply (X7).
            let output = self.mcp_executor.execute(input, context).await?;
            let text = output
                .values
                .iter()
                .filter_map(|output| output.as_str())
                .fold(String::new(), |mut a, b| {
                    a.push('\n');
                    a.push_str(b);
                    a
                });
            if !text.trim().is_empty() {
                let text = style(text).cyan().dim().to_string();
                context
                    .send(ChatResponse::TaskMessage {
                        content: ChatResponseContent::ToolOutput(text),
                    })
                    .await?;
            }
            Ok(output)
        } else {
            Err(Error::NotFound(input.name).into())
        }
    }

    /// T16: the `kernel` tool. Cells reach tools through a bridge that runs
    /// each `tools.<name>(...)` as a direct call of this agent (same policy,
    /// schema, bans, hooks, read-before-edit), except `kernel`, `toolbox` and
    /// `task`, which a cell cannot call.
    async fn run_kernel(
        &self,
        agent: &Agent,
        input: forge_domain::Kernel,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        const NOT_FROM_CELL: [&str; 3] = ["kernel", "toolbox", "task"];
        let names: Vec<String> = context
            .surface()
            .published(&context.surface_state()?.snapshot())
            .into_iter()
            .map(|tool| tool.name.to_string())
            .filter(|name| !NOT_FROM_CELL.contains(&name.as_str()))
            .collect();
        let services = self.services.clone();
        let agent = agent.clone();
        let cell_context = context.clone();
        let bridge: crate::CellToolBridge = Arc::new(move |tool: String, args: Value| {
            let services = services.clone();
            let agent = agent.clone();
            let context = cell_context.clone();
            Box::pin(async move {
                if NOT_FROM_CELL.contains(&tool.as_str()) {
                    return Err(format!(
                        "{tool} cannot be called from a kernel cell; call it directly"
                    ));
                }
                let call = ToolCallFull::new(ToolName::new(&tool))
                    .arguments(forge_domain::ToolCallArguments::from(args));
                let registry = ToolRegistry::new(services);
                let result = registry
                    .call_with_bounds(&agent, &context, call, false)
                    .await;
                let text = result
                    .output
                    .values
                    .iter()
                    .filter_map(forge_domain::ToolValue::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                if result.output.is_error {
                    Err(text)
                } else {
                    Ok(text)
                }
            })
        });
        let cwd = self.services.get_environment().cwd;
        let text = self
            .services
            .run_kernel(input, cwd, context, bridge, names)
            .await?;
        Ok(ToolOutput::text(text))
    }

    /// T24: the `tool_discovery` tool. Searches the request's catalog or
    /// loads one id; loading changes the conversation's surface state, which
    /// the orchestrator publishes from the next model call.
    async fn run_discovery(
        &self,
        agent: &Agent,
        input: forge_domain::ToolDiscovery,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        use forge_domain::{DiscoveryKind, EntryKind, catalog_page};

        let surface = context.surface().clone();
        let state = context.surface_state()?;

        if let Some(id) = input
            .load
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return self.load_catalog_id(id, &surface, &state).await;
        }

        // A search scoped to MCP (kind or a server name as source) connects
        // the agent's servers first, so their tools become catalog rows.
        let wants_mcp = matches!(input.kind, Some(DiscoveryKind::Mcp))
            || input.source.as_deref().is_some_and(|source| {
                surface.with(|d| d.status_rows.iter().any(|r| r.source == source))
            });
        if wants_mcp {
            self.connect_mcp_into_surface(agent, &surface, input.source.as_deref())
                .await;
        }

        let kind = input.kind.map(|kind| match kind {
            DiscoveryKind::Native => EntryKind::Native,
            DiscoveryKind::Mcp => EntryKind::Mcp,
            DiscoveryKind::Skill => EntryKind::Skill,
        });
        let rows: Vec<forge_domain::CatalogEntry> = surface
            .catalog()
            .into_iter()
            .filter(|row| kind.is_none_or(|k| row.kind == k))
            .filter(|row| {
                input
                    .source
                    .as_deref()
                    .is_none_or(|source| row.source.eq_ignore_ascii_case(source))
            })
            .collect();
        let page = catalog_page(
            &rows,
            &input.query,
            &state.snapshot(),
            input.offset.unwrap_or(0) as usize,
        );
        Ok(ToolOutput::text(page))
    }

    /// Loads one catalog id: a lazy tool (published from the next call) or a
    /// skill (instructions returned, its declared tools loaded where policy
    /// allows).
    async fn load_catalog_id(
        &self,
        id: &str,
        surface: &forge_domain::SharedSurface,
        state: &forge_domain::SharedSurfaceState,
    ) -> anyhow::Result<ToolOutput> {
        if let Some(name) = id.strip_prefix("skill:") {
            let known = surface.with(|d| d.skills.iter().any(|row| row.id == id));
            if !known {
                anyhow::bail!("no allowed catalog entry with id {id:?}");
            }
            let skill = self.services.fetch_skill(name.to_string()).await?;
            let mut loaded = Vec::new();
            let mut denied = Vec::new();
            for tool in &skill.tools {
                let tool_id = surface.with(|d| {
                    d.tools
                        .iter()
                        .find(|t| t.definition.name.as_str() == tool)
                        .map(|t| (t.catalog_id(), t.mode))
                });
                match tool_id {
                    Some((tool_id, forge_domain::ToolMode::Lazy)) => {
                        state.with(|s| s.loaded.insert(tool_id));
                        loaded.push(tool.clone());
                    }
                    Some((_, _)) => loaded.push(tool.clone()),
                    None => denied.push(tool.clone()),
                }
            }
            state.with(|s| {
                s.loaded.insert(id.to_string());
                s.offered_skills.insert(name.to_string());
            });
            let mut result = serde_json::json!({
                "id": id,
                "instructions": skill.command,
                "loaded": loaded,
            });
            if !denied.is_empty() {
                result["denied_tools"] = serde_json::json!(denied);
            }
            return Ok(ToolOutput::text(result.to_string()));
        }
        let found = surface.with(|d| {
            d.tools
                .iter()
                .find(|t| t.mode == forge_domain::ToolMode::Lazy && t.catalog_id() == id)
                .map(|t| t.definition.name.to_string())
        });
        match found {
            Some(name) => {
                state.with(|s| s.loaded.insert(id.to_string()));
                Ok(ToolOutput::text(
                    serde_json::json!({"id": id, "loaded": [name]}).to_string(),
                ))
            }
            None => {
                let status = surface.with(|d| d.status_rows.iter().any(|r| r.id == id));
                if status {
                    anyhow::bail!("{id} is a status row, not a loadable tool");
                }
                anyhow::bail!("no allowed catalog entry with id {id:?}")
            }
        }
    }

    /// M2: connects the MCP servers this agent may use (all, or the one
    /// named by `source`) and adds their tools to the surface as lazy
    /// catalog rows; failures become status rows with secrets redacted.
    async fn connect_mcp_into_surface(
        &self,
        agent: &Agent,
        surface: &forge_domain::SharedSurface,
        source: Option<&str>,
    ) {
        let config = self.services.get_config().unwrap_or_default();
        let servers = match self.services.get_mcp_servers().await {
            Ok(servers) => servers,
            Err(error) => {
                surface.set_status(
                    "mcp",
                    forge_domain::CatalogEntry {
                        id: "mcp:server-error:all".to_string(),
                        name: "mcp".to_string(),
                        kind: forge_domain::EntryKind::Mcp,
                        source: "mcp".to_string(),
                        description: format!("MCP servers could not be listed: {error:#}"),
                        search_text: String::new(),
                        loadable: false,
                    },
                );
                return;
            }
        };
        let policy = config.mcp.agents.get(agent.id.as_str());
        for (server, tools) in servers.get_servers() {
            let server = server.to_string();
            let scoped_out = source.is_some_and(|s| !s.eq_ignore_ascii_case(&server));
            let denied = policy.is_some_and(|p| !p.servers.permits(&server));
            if scoped_out || denied {
                continue;
            }
            let tools: Vec<forge_domain::SurfaceTool> = tools
                .iter()
                .filter(|tool| {
                    config
                        .mcp
                        .permits(agent.id.as_str(), &server, tool.name.as_str())
                })
                .filter(|tool| {
                    surface.with(|d| {
                        !d.tools.iter().any(|t| {
                            t.definition.name == tool.name && t.mode != forge_domain::ToolMode::Lazy
                        })
                    })
                })
                .map(|tool| forge_domain::SurfaceTool {
                    definition: tool.clone(),
                    mode: forge_domain::ToolMode::Lazy,
                    source: forge_domain::ToolSource::Mcp(server.clone()),
                })
                .collect();
            surface.add_tools(tools, &server);
        }
        for (server, error) in servers.get_failures() {
            let server = server.to_string();
            if source.is_some_and(|s| !s.eq_ignore_ascii_case(&server)) {
                continue;
            }
            surface.set_status(
                &server,
                forge_domain::CatalogEntry {
                    id: format!("mcp:server-error:{server}"),
                    name: server.clone(),
                    kind: forge_domain::EntryKind::Mcp,
                    source: server.clone(),
                    description: forge_domain::summary(&format!("MCP server error: {error}")),
                    search_text: String::new(),
                    loadable: false,
                },
            );
        }
    }

    /// T22: the `task` tool. Starts workers (foreground or background) or acts
    /// on a background run's handle.
    async fn run_task(
        &self,
        input: forge_domain::TaskInput,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        use forge_domain::ShellAction;

        use crate::task_jobs::{report, started_text};

        let jobs = task_jobs();
        if input.action != ShellAction::Run {
            let owner = context.conversation_id();
            let action = match input.action {
                ShellAction::Poll => "poll",
                ShellAction::Wait => "wait",
                _ => "kill",
            };
            let Some(job) = jobs.find(owner, input.handle.as_deref()) else {
                anyhow::bail!(
                    "no background task{} to {action}",
                    input.handle.map(|h| format!(" {h}")).unwrap_or_default()
                );
            };
            let was_running = job.running();
            match input.action {
                ShellAction::Kill => job.kill(),
                ShellAction::Wait => {
                    job.wait(input.timeout.map(std::time::Duration::from_secs))
                        .await
                }
                _ => {}
            }
            return Ok(ToolOutput::text(report(
                &job,
                input.action == ShellAction::Kill,
                was_running,
            )));
        }

        let config = self.services.get_config()?;
        let agent_id = input.agent_id.trim().to_string();
        if agent_id.is_empty() {
            anyhow::bail!("task requires agent_id");
        }
        let tasks: Vec<String> = input
            .tasks
            .iter()
            .map(|task| task.trim().to_string())
            .filter(|task| !task.is_empty())
            .collect();
        if tasks.is_empty() {
            anyhow::bail!("task requires at least one non-empty task");
        }
        if input.session_id.is_some() && tasks.len() > 1 {
            anyhow::bail!(
                "session_id runs one task per session. Pass a single task, or omit session_id so each parallel worker gets its own fresh session."
            );
        }
        let max_depth = config.task_max_depth.max(1);
        if context.task_depth() >= max_depth {
            anyhow::bail!("task recursion depth limit reached ({max_depth})");
        }
        if input.model.is_some() && config.lock_subagent_model {
            anyhow::bail!("the model override is locked by configuration; omit `model`");
        }
        if self
            .services
            .get_agent(&AgentId::new(&agent_id))
            .await?
            .is_none()
        {
            anyhow::bail!("agent {agent_id:?} not found");
        }
        let session = match &input.session_id {
            Some(id) => {
                let id = forge_domain::ConversationId::parse(id)
                    .map_err(|_| anyhow::anyhow!("invalid session_id {id:?}"))?;
                if self.services.find_conversation(&id).await?.is_none() {
                    anyhow::bail!("session_id {id} not found");
                }
                Some(id)
            }
            None => None,
        };

        let workers = config.subagent_max_workers.max(1);
        let budget = config.max_tool_result_bytes as usize;
        let executor = self.agent_executor.clone();
        let model = input.model.clone().map(forge_domain::ModelId::new);
        let count = tasks.len();
        let worker_context = context.clone();
        let agent = agent_id.clone();
        let fan_out = async move {
            let gate = Arc::new(tokio::sync::Semaphore::new(workers));
            let results = join_all(tasks.into_iter().map(|task| {
                let gate = gate.clone();
                let executor = executor.clone();
                let context = worker_context.clone();
                let agent = agent.clone();
                let model = model.clone();
                async move {
                    let _permit = gate.acquire().await;
                    executor
                        .execute_with(AgentId::new(&agent), task, &context, session, model)
                        .await
                }
            }))
            .await;
            assemble_task_results(results, &agent, budget)
        };

        if input.background {
            let results_dir =
                crate::result_bounds::spill_dir(&self.services.get_environment().base_path);
            let job = jobs.start(
                *context.conversation_id(),
                &agent_id,
                count,
                &results_dir,
                fan_out,
            );
            return Ok(ToolOutput::text(started_text(&job)));
        }
        Ok(ToolOutput::text(fan_out.await))
    }

    pub async fn call(
        &self,
        agent: &Agent,
        context: &ToolCallContext,
        call: ToolCallFull,
    ) -> ToolResult {
        self.call_with_bounds(agent, context, call, true).await
    }

    /// [`Self::call`]; with `bounded` false the result is not clipped or
    /// spilled (X11) — a kernel cell receives tool results whole, and only
    /// what it prints reaches the model.
    async fn call_with_bounds(
        &self,
        agent: &Agent,
        context: &ToolCallContext,
        call: ToolCallFull,
        bounded: bool,
    ) -> ToolResult {
        let call_id = call.call_id.clone();
        let tool_name = call.name.clone();
        let context = match &call_id {
            Some(id) => context.clone().with_call_id(id.as_str()),
            None => context.clone(),
        };
        let path = Self::touched_path(&call);
        let output = self.call_inner(agent, call, &context).await.map(|output| {
            if bounded {
                self.bound(output, &context)
            } else {
                output
            }
        });
        // T21: the first time a file matching a skill's `paths` is read or
        // edited, the result offers that skill once.
        let output = match (output, path) {
            (Ok(output), Some(path)) => Ok(match self.skill_offer(&path, &context) {
                Some(offer) => output.combine(ToolOutput::text(offer)),
                None => output,
            }),
            (output, _) => output,
        };

        // X19: `tool_result` hooks see the call and its outcome; they cannot
        // change it. X25: workers inherit only refusing hooks, so observers
        // run for the user's own conversation only.
        if context.task_depth() == 0
            && let Ok(config) = self.services.get_config()
            && crate::user_hooks::matching(
                &config.hooks,
                forge_config::HookEvent::ToolResult,
                Some(tool_name.as_str()),
            )
            .next()
            .is_some()
        {
            let payload = json!({
                "hook_event_name": "tool_result",
                "session_id": context.conversation_id().to_string(),
                "tool_name": tool_name.as_str(),
                "is_error": output.is_err() || output.as_ref().is_ok_and(|o| o.is_error),
            });
            self.run_observer_hooks(
                &config.hooks,
                forge_config::HookEvent::ToolResult,
                Some(tool_name.as_str()),
                payload,
            )
            .await;
        }

        ToolResult::new(tool_name).call_id(call_id).output(output)
    }

    /// X19: runs the `tool_call` hooks for `input`; the first that exits 2
    /// refuses the call with its message.
    async fn run_tool_call_hooks(
        &self,
        agent: &Agent,
        input: &ToolCallFull,
        context: &ToolCallContext,
    ) -> Option<String> {
        let config = self.services.get_config().ok()?;
        let hooks: Vec<&forge_config::UserHook> = crate::user_hooks::matching(
            &config.hooks,
            forge_config::HookEvent::ToolCall,
            Some(input.name.as_str()),
        )
        .collect();
        if hooks.is_empty() {
            return None;
        }
        let payload = json!({
            "hook_event_name": "tool_call",
            "session_id": context.conversation_id().to_string(),
            "agent_id": agent.id.as_str(),
            "tool_name": input.name.as_str(),
            "tool_input": input.arguments.parse().unwrap_or(Value::Null),
            "cwd": self.services.get_environment().cwd,
        });
        let cwd = self.services.get_environment().cwd;
        for hook in hooks {
            match crate::user_hooks::run_and_queue(hook, &payload, &cwd).await {
                Ok(outcome) => {
                    if let Some(refusal) = outcome.refusal(input.name.as_str()) {
                        return Some(refusal);
                    }
                }
                Err(error) => {
                    tracing::warn!(command = %hook.command, %error, "tool_call hook failed to start")
                }
            }
        }
        None
    }

    /// X19: runs observing hooks (their exit status is ignored).
    async fn run_observer_hooks(
        &self,
        hooks: &[forge_config::UserHook],
        event: forge_config::HookEvent,
        tool: Option<&str>,
        payload: Value,
    ) {
        let cwd = self.services.get_environment().cwd;
        for hook in crate::user_hooks::matching(hooks, event, tool) {
            if let Err(error) = crate::user_hooks::run_and_queue(hook, &payload, &cwd).await {
                tracing::warn!(command = %hook.command, %error, "hook failed to start");
            }
        }
    }

    /// The file a read/write/patch call works on.
    fn touched_path(call: &ToolCallFull) -> Option<String> {
        match ToolCatalog::try_from(call.clone()).ok()? {
            ToolCatalog::Read(input) => Some(input.file_path),
            ToolCatalog::Write(input) => Some(input.file_path),
            ToolCatalog::Patch(input) => Some(input.file_path),
            _ => None,
        }
    }

    /// A one-time `<system_reminder>` offering each skill whose `paths` match
    /// `path` and that this conversation has not been offered or loaded.
    fn skill_offer(&self, path: &str, context: &ToolCallContext) -> Option<String> {
        let surface = context.surface();
        let candidates = surface.with(|data| data.skill_paths.clone());
        if candidates.is_empty() {
            return None;
        }
        let cwd = self.services.get_environment().cwd;
        let absolute = std::path::Path::new(path);
        let relative = absolute
            .strip_prefix(&cwd)
            .unwrap_or(absolute)
            .display()
            .to_string();
        let state = context.surface_state().ok()?;
        let offers: Vec<String> = state.with(|state| {
            let matched: Vec<String> = candidates
                .into_iter()
                .filter(|(name, _)| !state.offered_skills.contains(name))
                .filter(|(_, globs)| {
                    let mut skill = forge_domain::Skill::new("", "", "");
                    skill.paths = globs.clone();
                    skill.matches_path(&relative)
                })
                .map(|(name, _)| name)
                .collect();
            state.offered_skills.extend(matched.iter().cloned());
            matched
                .into_iter()
                .filter(|name| !state.loaded.contains(&format!("skill:{name}")))
                .collect()
        });
        if offers.is_empty() {
            return None;
        }
        let lines: Vec<String> = offers
            .iter()
            .map(|name| {
                format!(
                    "Skill {name} applies to {relative}. Load it with tool_discovery {{\"load\":\"skill:{name}\"}}."
                )
            })
            .collect();
        Some(
            Element::new("system_reminder")
                .text(lines.join(" "))
                .to_string(),
        )
    }

    /// X11: spill/clip one result and narrow read coverage to what the model
    /// receives.
    fn bound(&self, output: ToolOutput, context: &ToolCallContext) -> ToolOutput {
        let Ok(config) = self.services.get_config() else {
            return output;
        };
        let limits = crate::result_bounds::ResultLimits {
            inline: config.max_tool_result_inline_bytes as usize,
            hard: config.max_tool_result_bytes as usize,
            batch: 0,
        };
        let env = self.services.get_environment();
        let original = output.as_str().map(str::to_string);
        let bounded = crate::result_bounds::bound_result(
            output,
            limits,
            &crate::result_bounds::spill_dir(&env.base_path),
        );
        if let (Some(original), Some(delivered), Some(call_id), Ok(ledger)) = (
            original,
            bounded.as_str(),
            context.call_id(),
            context.reads(),
        ) {
            crate::result_bounds::narrow_read_coverage(&ledger, call_id, &original, delivered);
        }
        bounded
    }

    /// Gets the model for the currently active agent by looking up the agent
    /// and fetching its model from the provider's model list.
    ///
    /// Returns None if no active agent, agent not found, or model not in
    /// provider list.
    async fn get_current_model(&self) -> Option<Model> {
        let agent_id = self.services.get_active_agent_id().await.ok()??;
        let agent = self.services.get_agent(&agent_id).await.ok()??;
        let provider = self.services.get_provider(agent.provider).await.ok()?;
        let models = self.services.models(provider).await.ok()?;
        models.iter().find(|m| m.id == agent.model).cloned()
    }

    pub async fn tools_overview(&self) -> anyhow::Result<ToolsOverview> {
        let mcp_tools = self.services.get_mcp_servers().await?;
        let agent_tools = self.agent_executor.agent_definitions().await?;

        // Get agents for template rendering in Task tool description
        let mut agents = self.services.get_agents().await?;

        let environment = self.services.get_environment();

        // Get current model for dynamic tool descriptions
        let model = self.get_current_model().await;

        // Build TemplateConfig from ForgeConfig for tool description templates
        let config = self.services.get_config()?;

        // Filter out research subagents from task tool description when
        // disabled
        if !config.research_subagent {
            agents.retain(|agent| {
                let id = agent.id.as_str();
                id != "sage" && id != "agent"
            });
        }

        let template_config = TemplateConfig {
            max_read_size: config.max_read_lines as usize,
            max_read_bytes: config.max_read_bytes as usize,
            max_image_size: config.max_image_size_bytes as usize,
        };

        Ok(ToolsOverview::new()
            .system(Self::get_system_tools(
                &environment,
                model,
                agents,
                &template_config,
            ))
            .agents(agent_tools)
            .mcp(mcp_tools))
    }

    /// Builds the tool surface of one request for `agent` (T24, X1, X21,
    /// X23, M2).
    ///
    /// MCP servers are not connected here unless the agent names MCP tools
    /// `:eager`; otherwise each configured server is a status row that
    /// `tool_discovery` connects on demand. Native descriptions are rendered
    /// against the tools actually on the surface.
    ///
    /// # Errors
    /// Returns an error when agents, skills or config cannot be loaded.
    pub async fn request_surface(
        &self,
        agent: &Agent,
        model: Option<Model>,
        provider: &str,
    ) -> anyhow::Result<(forge_domain::SharedSurface, forge_domain::AliasMap)> {
        use crate::tool_surface as ts;

        let config = self.services.get_config()?;
        let agent_tools = self.agent_executor.agent_definitions().await?;
        let wants_eager_mcp =
            agent.tools.iter().flatten().any(|entry| {
                entry.as_str().starts_with("mcp") && entry.as_str().ends_with(":eager")
            });
        let mcp = if wants_eager_mcp {
            self.services.get_mcp_servers().await.unwrap_or_default()
        } else {
            forge_domain::McpServers::default()
        };
        let known = ts::known_tools(ts::native_definitions(), agent_tools, &mcp);
        let mut tools = ts::resolve_surface(agent, &known);
        if wants_eager_mcp {
            let policy = config.mcp.agents.get(agent.id.as_str());
            tools.retain(|tool| match &tool.source {
                forge_domain::ToolSource::Mcp(server) => policy.is_none_or(|p| {
                    p.servers.permits(server) && p.tools.permits(tool.definition.name.as_str())
                }),
                _ => true,
            });
        }
        let on_surface: HashSet<String> = tools
            .iter()
            .map(|t| t.definition.name.to_string())
            .collect();
        let blocked = known
            .iter()
            .map(|t| t.definition.name.to_string())
            .filter(|name| !on_surface.contains(name))
            .collect();

        let aliases = ts::resolve_alias_profile(
            &config.tool_alias_profiles,
            agent.model.as_str(),
            provider,
            &tools,
        );
        let mut agents = self.services.get_agents().await?;
        if !config.research_subagent {
            agents.retain(|a| a.id.as_str() != "sage" && a.id.as_str() != "agent");
        }
        agents.sort_by(|l, r| l.id.as_str().cmp(r.id.as_str()));
        let data = SystemContext {
            env: Some(self.services.get_environment()),
            model,
            tool_names: ts::tool_names_map(&tools, &aliases),
            agents,
            config: Some(TemplateConfig {
                max_read_size: config.max_read_lines as usize,
                max_read_bytes: config.max_read_bytes as usize,
                max_image_size: config.max_image_size_bytes as usize,
            }),
            ..Default::default()
        };
        ts::render_native_descriptions(&mut tools, &data);

        // Skills: catalog rows when the agent has `tool_discovery`.
        let has_discovery = tools
            .iter()
            .any(|t| t.definition.name.as_str() == forge_domain::TOOL_DISCOVERY);
        let skills = if has_discovery {
            self.services.list_skills().await.unwrap_or_default()
        } else {
            Vec::new()
        };
        // MCP servers not connected yet are status rows the model can scope a
        // discovery to.
        let status_rows = if has_discovery && !wants_eager_mcp {
            self.services
                .read_mcp_config(None)
                .await
                .map(|cfg| {
                    cfg.mcp_servers
                        .iter()
                        .filter(|(_, server)| !server.is_disabled())
                        .filter(|(name, _)| {
                            config
                                .mcp
                                .agents
                                .get(agent.id.as_str())
                                .is_none_or(|p| p.servers.permits(&name.to_string()))
                        })
                        .map(|(name, _)| forge_domain::CatalogEntry {
                            id: format!("mcp:server:{name}"),
                            name: name.to_string(),
                            kind: forge_domain::EntryKind::Mcp,
                            source: name.to_string(),
                            description: format!(
                                "MCP server {name} (not connected); search with kind=\"mcp\" or source=\"{name}\" to connect and list its tools."
                            ),
                            search_text: String::new(),
                            loadable: false,
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let surface = forge_domain::SharedSurface::new(forge_domain::SurfaceData {
            tools,
            status_rows,
            skills: ts::skill_rows(&skills),
            blocked,
            skill_paths: skills
                .iter()
                .filter(|s| !s.paths.is_empty() && !s.disable_model_invocation)
                .map(|s| (s.name.clone(), s.paths.clone()))
                .collect(),
            skill_tools: skills
                .iter()
                .map(|s| (s.name.clone(), s.tools.clone()))
                .collect(),
        });
        Ok((surface, aliases))
    }
}

impl<S> ToolRegistry<S> {
    fn get_system_tools(
        env: &Environment,
        model: Option<Model>,
        agents: Vec<forge_domain::Agent>,
        template_config: &TemplateConfig,
    ) -> Vec<ToolDefinition> {
        use crate::TemplateEngine;

        let handlebars = TemplateEngine::handlebar_instance();
        let mut agents = agents;
        agents.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));

        // Build tool_names map from all available tools
        let tool_names: Map<String, Value> = ToolCatalog::iter()
            .map(|tool| {
                let def = tool.definition();
                (def.name.to_string(), json!(def.name.to_string()))
            })
            .collect();

        // Create template data with environment nested under "env"
        let ctx = SystemContext {
            env: Some(env.clone()),
            model,
            tool_names,
            agents,
            config: Some(template_config.clone()),
            ..Default::default()
        };

        ToolCatalog::iter()
            .map(|tool| {
                let mut def = tool.definition();
                // Render template variables in description
                if let Ok(rendered) = handlebars.render_template(&def.description, &ctx) {
                    def.description = rendered;
                }
                def
            })
            .collect::<Vec<_>>()
    }

    /// Validates that `tool_name` is callable on this request: published by
    /// the request's surface (eager, or lazy and loaded). A refusal
    /// distinguishes a lazy tool that needs loading, a real tool this agent
    /// may not use, and a name that does not exist (with a "did you mean"
    /// suggestion); none counts toward the repeated-failure limit.
    ///
    /// A context with no surface (an internal call outside a chat) falls
    /// back to the agent's `tools:` entries.
    fn validate_tool_call(
        agent: &Agent,
        tool_name: &ToolName,
        context: &ToolCallContext,
    ) -> anyhow::Result<()> {
        let surface = context.surface();
        let state = context.surface_state()?.snapshot();
        let has_surface = surface.with(|data| !data.tools.is_empty());
        let tool_name = &forge_domain::deprecated_alias(tool_name.as_str())
            .map(ToolName::new)
            .unwrap_or_else(|| tool_name.clone());
        let entries: Vec<String> = agent
            .tools
            .iter()
            .flatten()
            .map(|t| t.to_string())
            .collect();
        let published: Vec<String> = if has_surface {
            surface
                .published(&state)
                .iter()
                .map(|t| t.name.to_string())
                .collect()
        } else {
            // No surface (an internal call outside a chat): the agent's exact
            // entries stand in for it.
            entries
                .iter()
                .filter_map(|entry| forge_domain::PolicyEntry::parse(entry).ok())
                .filter(|entry| entry.modifier != Some(forge_domain::ToolMode::Ban))
                .filter_map(|entry| match entry.noun {
                    forge_domain::Noun::Name(name) => Some(name),
                    _ => None,
                })
                .collect()
        };
        let allowed = if has_surface {
            let legacy = tool_name.to_legacy_mcp_name().map(|n| n.to_string());
            let prefixed = format!("mcp__{}", tool_name.as_str());
            published.iter().any(|name| {
                name == tool_name.as_str() || legacy.as_ref() == Some(name) || *name == prefixed
            })
        } else {
            let meta = forge_domain::ToolMeta {
                name: tool_name.clone(),
                alternate_names: tool_name
                    .to_legacy_mcp_name()
                    .map(|n| vec![n.to_string()])
                    .unwrap_or_default(),
                read_only: false,
                default_mode: forge_domain::ToolMode::Eager,
            };
            forge_domain::select_tools(&entries, &[meta])
                .mode(tool_name.as_str())
                .is_some()
        };
        if !allowed {
            tracing::error!(tool_name = %tool_name, "No tool with name");
            let names: Vec<&str> = published.iter().map(String::as_str).collect();
            let absent = surface.absent(tool_name, &state);
            return Err(anyhow::Error::new(forge_domain::ToolRefusal(
                forge_domain::unavailable_message(tool_name.as_str(), &names, &absent),
            )));
        }
        Ok(())
    }

    /// Checks if a file path has an image extension.
    /// This is a lightweight check that doesn't require reading the file.
    fn has_image_extension(path: &str) -> bool {
        const IMAGE_EXTENSIONS: &[&str] = &[
            ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".pdf",
        ];

        let path_lower = path.to_lowercase();
        IMAGE_EXTENSIONS.iter().any(|ext| path_lower.ends_with(ext))
    }

    /// Whether `model` accepts image input. An unknown model is treated as
    /// text-only.
    fn supports_images(model: Option<&Model>) -> bool {
        model.is_some_and(|m| {
            m.input_modalities
                .iter()
                .any(|im| matches!(im, InputModality::Image))
        })
    }
}

#[cfg(test)]
mod tests {
    use forge_domain::{
        Agent, AgentId, Environment, ModelId, ProviderId, TemplateConfig, ToolCallContext,
        ToolCatalog, ToolName,
    };
    use pretty_assertions::assert_eq;

    use crate::tool_registry::{ToolRegistry, create_test_agents};

    fn agent() -> Agent {
        // only allow read and search tools for this agent
        Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("read"), ToolName::new("fs_search")])
    }

    #[tokio::test]
    async fn test_restricted_tool_call() {
        let result = ToolRegistry::<()>::validate_tool_call(
            &agent(),
            &ToolName::new(ToolCatalog::Read(Default::default())),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        assert!(result.is_ok(), "Tool call should be valid");
    }

    #[tokio::test]
    async fn test_restricted_tool_call_err() {
        let error = ToolRegistry::<()>::validate_tool_call(
            &agent(),
            &ToolName::new("write"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "write is not allowed by this agent's tool policy.");
    }

    #[test]
    fn test_unknown_tool_suggests_closest_and_is_a_refusal() {
        let error = ToolRegistry::<()>::validate_tool_call(
            &agent(),
            &ToolName::new("raed"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        )
        .unwrap_err();
        let refusal = error.downcast_ref::<forge_domain::ToolRefusal>().is_some();
        let actual = (refusal, error.to_string());
        let expected = (
            true,
            "unknown tool raed. Did you mean read? This agent's tools: read, fs_search."
                .to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_wildcard() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("mcp_*"), ToolName::new("read")]);

        let actual = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("mcp_foo"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual.is_ok());
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_multiple_tools() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("mcp_*"), ToolName::new("read")]);

        let actual_mcp_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("mcp_read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_mcp_write = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("mcp_write"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_mcp_read.is_ok());
        assert!(actual_mcp_write.is_ok());
        assert!(actual_read.is_ok());
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_no_match() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("mcp_*"), ToolName::new("read")]);

        let actual = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("write"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        let expected = "write is not allowed by this agent's tool policy.";

        assert_eq!(actual.unwrap_err().to_string(), expected);
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_question_mark() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("read?"), ToolName::new("write")]);

        let actual_read1 = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read1"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_readx = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("readx"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_read1.is_ok());
        assert!(actual_readx.is_ok());
        assert!(actual_read.is_err());
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_character_class() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("tool_[abc]"), ToolName::new("write")]);

        let actual_tool_a = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("tool_a"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_tool_b = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("tool_b"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_tool_c = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("tool_c"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_tool_d = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("tool_d"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_tool_a.is_ok());
        assert!(actual_tool_b.is_ok());
        assert!(actual_tool_c.is_ok());
        assert!(actual_tool_d.is_err());
    }

    #[test]
    fn test_validate_tool_call_with_glob_pattern_double_wildcard() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("**"), ToolName::new("read")]);

        let actual_any_tool = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("any_tool_name"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_nested = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("nested/tool"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_any_tool.is_ok());
        assert!(actual_nested.is_ok());
    }

    #[test]
    fn test_validate_tool_call_exact_match_with_special_chars() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("tool_[special]"), ToolName::new("read")]);

        let actual = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("tool_[special]"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        // The glob pattern "tool_[special]" will match "tool_s", "tool_p",
        // etc., not the literal string So this test verifies that exact
        // matching doesn't work when the pattern is a valid glob
        assert!(actual.is_err());
    }

    #[test]
    fn test_validate_tool_call_backward_compatibility_exact_match() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![
            ToolName::new("read"),
            ToolName::new("write"),
            ToolName::new("fs_search"),
        ]);

        let actual_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_write = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("write"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_invalid = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("delete"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_read.is_ok());
        assert!(actual_write.is_ok());
        assert!(actual_invalid.is_err());
    }

    #[test]
    fn test_validate_tool_call_empty_tools_list() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        );

        let actual = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual.is_err());
    }

    #[test]
    fn test_validate_tool_call_glob_with_prefix_suffix() {
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("mcp_*_tool")]);

        let actual_match = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("mcp_read_tool"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_no_match = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("mcp_read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_match.is_ok());
        assert!(actual_no_match.is_err());
    }

    #[test]
    fn test_validate_tool_call_capitalized_read_write() {
        // Test that capitalized "Read" and "Write" are accepted when agent has
        // lowercase versions
        let fixture = Agent::new(
            AgentId::new("test_agent"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .tools(vec![ToolName::new("read"), ToolName::new("write")]);

        let actual_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("Read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_write = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("Write"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );
        let actual_lowercase_read = ToolRegistry::<()>::validate_tool_call(
            &fixture,
            &ToolName::new("read"),
            &ToolCallContext::new(forge_domain::Metrics::default()),
        );

        assert!(actual_read.is_ok(), "Capitalized 'Read' should be accepted");
        assert!(
            actual_write.is_ok(),
            "Capitalized 'Write' should be accepted"
        );
        assert!(
            actual_lowercase_read.is_ok(),
            "Lowercase 'read' should still be accepted"
        );
    }

    #[test]
    fn test_task_tool_description_is_stable_across_agent_order() {
        use fake::{Fake, Faker};
        let env: Environment = Faker.fake();
        let template_config = TemplateConfig::default();
        let agents = create_test_agents();
        let mut reversed_agents = agents.clone();
        reversed_agents.reverse();

        let fixture = ToolRegistry::<()>::get_system_tools(&env, None, agents, &template_config);
        let actual =
            ToolRegistry::<()>::get_system_tools(&env, None, reversed_agents, &template_config);

        let expected = fixture
            .iter()
            .find(|tool| tool.name.as_str() == "task")
            .expect("Task tool should exist")
            .description
            .clone();
        let actual = actual
            .iter()
            .find(|tool| tool.name.as_str() == "task")
            .expect("Task tool should exist")
            .description
            .clone();

        assert_eq!(actual, expected);
    }
}

#[cfg(test)]
fn create_test_agents() -> Vec<forge_domain::Agent> {
    use forge_domain::{Agent, AgentId, ModelId, ProviderId, ToolName};

    vec![
        Agent::new(
            AgentId::new("sage"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .id(AgentId::new("sage"))
        .title("Research Agent")
        .description("Specialized in researching codebases")
        .tools(vec![
            ToolName::new("read"),
            ToolName::new("fs_search"),
            ToolName::new("fetch"),
        ]),
        Agent::new(
            AgentId::new("debug"),
            ProviderId::ANTHROPIC,
            ModelId::new("claude-3-5-sonnet-20241022"),
        )
        .id(AgentId::new("debug"))
        .title("Debug Agent")
        .description("Specialized in debugging issues")
        .tools(vec![
            ToolName::new("read"),
            ToolName::new("shell"),
            ToolName::new("fs_search"),
            ToolName::new("fetch"),
        ]),
    ]
}

#[cfg(test)]
fn create_test_model(
    id: &str,
    modalities: Vec<forge_domain::InputModality>,
) -> forge_domain::Model {
    use forge_domain::{Model, ModelId};

    Model {
        id: ModelId::new(id),
        name: Some(format!("Test {}", id)),
        description: None,
        context_length: Some(128000),
        tools_supported: Some(true),
        supports_parallel_tool_calls: Some(true),
        supports_reasoning: Some(false),
        input_modalities: modalities,
    }
}

#[test]
fn test_template_rendering_in_tool_descriptions() {
    use fake::{Fake, Faker};

    let env: Environment = Faker.fake();
    let template_config = TemplateConfig { max_read_bytes: 262144, ..Default::default() };

    let actual =
        ToolRegistry::<()>::get_system_tools(&env, None, create_test_agents(), &template_config);
    let fs_search_tool = actual
        .iter()
        .find(|t| t.name.as_str() == "fs_search")
        .unwrap();

    // The description should not contain unrendered template variables
    assert!(
        !fs_search_tool.description.contains("{{"),
        "Description should not contain unrendered template variable: {}",
        fs_search_tool.description
    );

    // The description should contain the expected usage info
    assert!(
        fs_search_tool.description.contains("ripgrep"),
        "Description should mention ripgrep: {}",
        fs_search_tool.description
    );
}

#[test]
fn test_dynamic_tool_description_with_vision_model() {
    use fake::{Fake, Faker};
    use forge_domain::InputModality;

    let env: Environment = Faker.fake();
    let template_config = TemplateConfig {
        max_read_size: 2000,
        max_read_bytes: 262144,
        max_image_size: 5000,
    };
    let vision_model = create_test_model("gpt-4o", vec![InputModality::Text, InputModality::Image]);

    let tools_with_vision = ToolRegistry::<()>::get_system_tools(
        &env,
        Some(vision_model),
        create_test_agents(),
        &template_config,
    );
    let read_tool = tools_with_vision
        .iter()
        .find(|t| t.name.as_str() == "read")
        .unwrap();
    insta::assert_snapshot!(read_tool.description);
}

#[test]
fn test_dynamic_tool_description_with_text_only_model() {
    use fake::{Fake, Faker};
    use forge_domain::InputModality;

    let env: Environment = Faker.fake();
    let template_config = TemplateConfig {
        max_read_size: 2000,
        max_read_bytes: 262144,
        max_image_size: 5000,
    };
    let text_only_model = create_test_model("gpt-3.5-turbo", vec![InputModality::Text]);

    let tools_text_only = ToolRegistry::<()>::get_system_tools(
        &env,
        Some(text_only_model),
        create_test_agents(),
        &template_config,
    );
    let read_tool = tools_text_only
        .iter()
        .find(|t| t.name.as_str() == "read")
        .unwrap();

    // Text-only model should NOT see image and PDF support
    insta::assert_snapshot!(read_tool.description);
}

#[test]
fn test_supports_images() {
    use forge_domain::InputModality;

    let vision = create_test_model("gpt-4o", vec![InputModality::Text, InputModality::Image]);
    let text_only = create_test_model("gpt-3.5-turbo", vec![InputModality::Text]);
    let actual = (
        ToolRegistry::<()>::supports_images(Some(&vision)),
        ToolRegistry::<()>::supports_images(Some(&text_only)),
        ToolRegistry::<()>::supports_images(None),
    );
    let expected = (true, false, false);
    pretty_assertions::assert_eq!(actual, expected);
}

#[test]
fn test_assemble_task_results_single_verbatim_and_fan_out_numbered() {
    let conversation = forge_domain::ConversationId::generate();
    let single = assemble_task_results(
        vec![Ok(ToolOutput::ai(conversation, "the answer"))],
        "sage",
        0,
    );
    let fan_out = assemble_task_results(
        vec![
            Ok(ToolOutput::text("first")),
            Err(anyhow::anyhow!("worker exploded")),
        ],
        "sage",
        0,
    );
    let expected_single = format!("the answer\n(session_id={conversation})");
    let expected_fan_out = "TASK_RESULTS agent=sage\n\n1. first\n\n2. ERROR worker exploded";
    pretty_assertions::assert_eq!(
        (single, fan_out.as_str()),
        (expected_single, expected_fan_out)
    );
}

#[test]
fn test_assemble_task_results_holds_each_reply_to_its_share() {
    let actual = assemble_task_results(
        vec![
            Ok(ToolOutput::text("x".repeat(500))),
            Ok(ToolOutput::text("y")),
        ],
        "forge",
        400,
    );
    let first = actual.split("\n\n2. ").next().unwrap();
    assert!(
        first.contains("[truncated: equal share (200 bytes)"),
        "{first}"
    );
    assert!(actual.ends_with("2. y"), "{actual}");
}

#[test]
fn test_has_image_extension() {
    // Test various image extensions (case-insensitive)
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.png"));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.PNG"));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.jpg"));
    assert!(ToolRegistry::<()>::has_image_extension(
        "/path/to/file.jpeg"
    ));
    assert!(ToolRegistry::<()>::has_image_extension(
        "/path/to/file.JPEG"
    ));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.gif"));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.bmp"));
    assert!(ToolRegistry::<()>::has_image_extension(
        "/path/to/file.webp"
    ));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.svg"));

    // Test relative paths
    assert!(ToolRegistry::<()>::has_image_extension("image.png"));
    assert!(ToolRegistry::<()>::has_image_extension(
        "../images/photo.jpg"
    ));
    assert!(ToolRegistry::<()>::has_image_extension("/path/to/file.pdf"));

    // Test non-image files
    assert!(!ToolRegistry::<()>::has_image_extension(
        "/path/to/file.txt"
    ));
    assert!(!ToolRegistry::<()>::has_image_extension("/path/to/file.rs"));
    assert!(!ToolRegistry::<()>::has_image_extension("/path/to/file"));
    assert!(!ToolRegistry::<()>::has_image_extension("README.md"));

    // Test edge cases
    assert!(!ToolRegistry::<()>::has_image_extension(""));
    assert!(ToolRegistry::<()>::has_image_extension(
        "file.with.dots.png"
    ));
    assert!(ToolRegistry::<()>::has_image_extension(".png")); // Hidden file with .png extension
}

#[test]
fn test_dynamic_tool_description_without_model() {
    use fake::{Fake, Faker};

    let env: Environment = Faker.fake();
    let template_config = TemplateConfig {
        max_read_size: 2000,
        max_image_size: 5000,
        max_read_bytes: 262144,
    };

    // When no model is provided, should default to showing minimal capabilities
    let tools_no_model =
        ToolRegistry::<()>::get_system_tools(&env, None, create_test_agents(), &template_config);
    let read_tool = tools_no_model
        .iter()
        .find(|t| t.name.as_str() == "read")
        .unwrap();

    // Without model info, should show basic text file support
    insta::assert_snapshot!(read_tool.description);
}

#[test]
fn test_all_rendered_tool_descriptions() {
    use fake::{Fake, Faker};

    let mut env: Environment = Faker.fake();
    env.cwd = "/home/user/project".into();
    env.shell = "/bin/bash".to_string();

    let template_config = TemplateConfig {
        max_read_size: 2000,
        max_read_bytes: 262144,
        max_image_size: 5000,
    };

    let tools =
        ToolRegistry::<()>::get_system_tools(&env, None, create_test_agents(), &template_config);

    // Verify all tools have rendered descriptions (no template syntax left)
    for tool in &tools {
        assert!(
            !tool.description.contains("{{"),
            "Tool '{}' has unrendered template variables:\n{}",
            tool.name,
            tool.description
        );
    }

    // Snapshot all rendered tool descriptions for visual verification
    // This will fail if a tool is renamed and descriptions reference the old
    // name
    let all_descriptions: Vec<_> = tools
        .iter()
        .map(|t| format!("### {}\n\n{}\n", t.name, t.description))
        .collect();

    insta::assert_snapshot!(
        "all_rendered_tool_descriptions",
        all_descriptions.join("\n---\n\n")
    );
}

/// X1: every native description renders without error on a minimal surface
/// too: the tool itself plus `read` and `shell`. Co-presence blocks resolve
/// against the tools actually present; a cross-reference to an absent tool
/// is dropped, never left as a strict-mode render error.
#[test]
fn test_descriptions_render_for_a_minimal_surface() {
    use fake::{Fake, Faker};
    use forge_domain::{SurfaceTool, ToolMode, ToolSource};

    let mut env: Environment = Faker.fake();
    env.cwd = "/home/user/project".into();
    let handlebars = crate::TemplateEngine::handlebar_instance();
    let data_for = |own: &str| {
        let surface: Vec<SurfaceTool> = ["read", "shell", own]
            .into_iter()
            .map(|name| SurfaceTool {
                definition: ToolDefinition::new(name),
                mode: ToolMode::Eager,
                source: ToolSource::Native,
            })
            .collect();
        SystemContext {
            env: Some(env.clone()),
            tool_names: crate::tool_surface::tool_names_map(&surface, &Default::default()),
            agents: create_test_agents(),
            config: Some(TemplateConfig {
                max_read_size: 2000,
                max_read_bytes: 262144,
                max_image_size: 5000,
            }),
            ..Default::default()
        }
    };

    let failures: Vec<String> = ToolCatalog::iter()
        .map(|tool| tool.definition())
        .filter_map(|def| {
            handlebars
                .render_template(&def.description, &data_for(def.name.as_str()))
                .err()
                .map(|error| format!("{}: {error}", def.name))
        })
        .collect();
    let shell_description = ToolCatalog::iter()
        .map(|tool| tool.definition())
        .find(|def| def.name.as_str() == "shell")
        .unwrap()
        .description;
    let shell = handlebars
        .render_template(&shell_description, &data_for("shell"))
        .unwrap();

    assert_eq!(failures, Vec::<String>::new());
    // Co-presence: fs_search is absent, so its cross-reference names nothing.
    assert!(shell.contains("Read files with `read`"), "{shell}");
    assert!(!shell.contains("fs_search"), "{shell}");
}
