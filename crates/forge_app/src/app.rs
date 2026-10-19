use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use forge_config::ForgeConfig;
use forge_domain::*;
use forge_stream::MpscStream;

use crate::apply_tunable_parameters::ApplyTunableParameters;
use crate::changed_files::ChangedFiles;
use crate::dto::ToolsOverview;
use crate::hooks::{
    CompactionHandler, DoomLoopDetector, PendingTodosHandler, TitleGenerationHandler,
    TracingHandler,
};
use crate::init_conversation_metrics::InitConversationMetrics;
use crate::orch::Orchestrator;
use crate::services::{AgentRegistry, CustomInstructionsService, ProviderAuthService};
use crate::set_conversation_id::SetConversationId;
use crate::system_prompt::SystemPrompt;
use crate::tool_registry::ToolRegistry;
use crate::user_prompt::UserPromptGenerator;
use crate::{
    AgentExt, AgentProviderResolver, ConversationService, EnvironmentInfra, FileDiscoveryService,
    ProviderService, Services,
};

/// Builds a [`TemplateConfig`] from a [`ForgeConfig`].
///
/// Converts the configuration-layer field names into the domain-layer struct
/// expected by [`SystemContext`] for tool description template rendering.
pub(crate) fn build_template_config(config: &ForgeConfig) -> forge_domain::TemplateConfig {
    forge_domain::TemplateConfig {
        max_read_size: config.max_read_lines as usize,
        max_read_bytes: config.max_read_bytes as usize,
        max_image_size: config.max_image_size_bytes as usize,
    }
}

/// Appends one droppable reminder per background `task` run of this
/// conversation that finished since the last turn and has not been read, and
/// (X19) one per queued hook output. Worker conversations (depth > 0) get
/// only their own task notes.
fn add_task_notes(mut conversation: Conversation, agent: &Agent, depth: usize) -> Conversation {
    let mut notes = crate::tool_registry::task_jobs().completion_notes(&conversation.id);
    if depth == 0 {
        notes.extend(crate::user_hooks::take_reminders());
    }
    if notes.is_empty() {
        return conversation;
    }
    let mut context = conversation.context.take().unwrap_or_default();
    for note in notes {
        let text = forge_template::Element::new("system_reminder")
            .text(note)
            .to_string();
        let message = TextMessage::new(Role::User, text)
            .droppable(true)
            .model(agent.model.clone());
        context = context.add_message(ContextMessage::from(message));
    }
    conversation.context(context)
}

/// ForgeApp handles the core chat functionality by orchestrating various
/// services. It encapsulates the complex logic previously contained in the
/// ForgeAPI chat method.
pub struct ForgeApp<S> {
    services: Arc<S>,
    tool_registry: ToolRegistry<S>,
}

impl<S: Services + EnvironmentInfra<Config = forge_config::ForgeConfig>> ForgeApp<S> {
    /// Creates a new ForgeApp instance with the provided services.
    pub fn new(services: Arc<S>) -> Self {
        Self { tool_registry: ToolRegistry::new(services.clone()), services }
    }

    /// Executes a chat request and returns a stream of responses.
    /// This method contains the core chat logic extracted from ForgeAPI.
    pub async fn chat(
        &self,
        agent_id: AgentId,
        chat: ChatRequest,
    ) -> Result<MpscStream<Result<ChatResponse, anyhow::Error>>> {
        let services = self.services.clone();

        // Get the conversation for the chat request
        let conversation = services
            .find_conversation(&chat.conversation_id)
            .await?
            .ok_or_else(|| forge_domain::Error::ConversationNotFound(chat.conversation_id))?;

        // Discover files using the discovery service
        let forge_config = self.services.get_config()?;
        let environment = services.get_environment();

        let files = services.list_current_directory().await?;

        let custom_instructions = services.get_custom_instructions().await;

        // Prepare agents with user configuration
        let agent_provider_resolver = AgentProviderResolver::new(services.clone());

        // Get agent and apply workflow config
        let agent = self
            .services
            .get_agent(&agent_id)
            .await?
            .ok_or(crate::Error::AgentNotFound(agent_id.clone()))?
            .apply_config(&forge_config)
            .set_compact_model_if_none();
        // A `task` worker may run on another model than its agent's.
        let agent = match chat.model_override.clone() {
            Some(model) => agent.model(model),
            None => agent,
        };

        let agent_provider = agent_provider_resolver
            .get_provider(Some(agent.id.clone()))
            .await?;
        let agent_provider = self
            .services
            .provider_auth_service()
            .refresh_provider_credential(agent_provider)
            .await?;

        let provider_id = agent_provider.id.to_string();
        let models = services.models(agent_provider).await?;
        let selected_model = models.iter().find(|model| model.id == agent.model);
        let agent = agent.compaction_threshold(selected_model);

        // T24: the agent's tool surface for this request (eager tools, lazy
        // catalog, skills, MCP status rows); tools loaded earlier in the
        // conversation are published again.
        let (surface, aliases) = self
            .tool_registry
            .request_surface(&agent, selected_model.cloned(), &provider_id)
            .await?;
        let surface_state = conversation.metrics.surface.snapshot();
        let tool_definitions: Vec<ToolDefinition> = surface.published(&surface_state);
        let max_tool_failure_per_turn = agent.max_tool_failure_per_turn.unwrap_or(3);

        let current_time = Local::now();

        // Insert system prompt
        let conversation =
            SystemPrompt::new(self.services.clone(), environment.clone(), agent.clone())
                .custom_instructions(custom_instructions.clone())
                .tool_definitions(tool_definitions.clone())
                .models(models.clone())
                .files(files.clone())
                .max_extensions(forge_config.max_extensions)
                .template_config(build_template_config(&forge_config))
                .aliases(aliases.clone())
                .add_system_message(conversation)
                .await?;

        // Insert user prompt
        let conversation = UserPromptGenerator::new(
            self.services.clone(),
            agent.clone(),
            chat.event.clone(),
            current_time,
        )
        .add_user_prompt(conversation)
        .await?;

        // Detect and render externally changed files notification
        let conversation = ChangedFiles::new(services.clone(), agent.clone())
            .update_file_stats(conversation)
            .await;

        // T22: background tasks that finished unread since the last turn;
        // X19: queued hook output.
        let conversation = add_task_notes(conversation, &agent, chat.task_depth);

        let conversation = InitConversationMetrics::new(current_time).apply(conversation);
        let conversation = ApplyTunableParameters::new(
            agent.clone(),
            crate::tool_surface::apply_aliases(tool_definitions.clone(), &aliases),
        )
        .apply(conversation);
        let conversation = SetConversationId.apply(conversation);

        // Create the orchestrator with all necessary dependencies
        let tracing_handler = TracingHandler::new();
        let title_handler = TitleGenerationHandler::new(services.clone());

        // Build the on_end hook, conditionally adding PendingTodosHandler based
        // on config
        let on_end_hook = if forge_config.verify_todos {
            tracing_handler
                .clone()
                .and(title_handler.clone())
                .and(PendingTodosHandler::new())
        } else {
            tracing_handler.clone().and(title_handler.clone())
        };

        let hook = Hook::default()
            .on_start(tracing_handler.clone().and(title_handler))
            .on_request(tracing_handler.clone().and(DoomLoopDetector::default()))
            .on_response(
                tracing_handler
                    .clone()
                    .and(CompactionHandler::new(agent.clone(), environment.clone())),
            )
            .on_toolcall_start(tracing_handler.clone())
            .on_toolcall_end(tracing_handler)
            .on_end(on_end_hook);

        let orch = Orchestrator::new(
            services.clone(),
            conversation,
            agent,
            self.services.get_config()?,
        )
        .error_tracker(ToolErrorTracker::new(max_tool_failure_per_turn))
        .tool_definitions(tool_definitions)
        .models(models)
        .hook(Arc::new(hook))
        .task_depth(chat.task_depth)
        .surface(surface)
        .aliases(aliases);

        // Create and return the stream
        let stream = MpscStream::spawn(
            |tx: tokio::sync::mpsc::Sender<Result<ChatResponse, anyhow::Error>>| {
                async move {
                    // Execute dispatch and always save conversation afterwards
                    let mut orch = orch.sender(tx.clone());
                    let dispatch_result = orch.run().await;

                    // Always save conversation using get_conversation()
                    let conversation = orch.get_conversation().clone();
                    let save_result = services.upsert_conversation(conversation).await;

                    // Send any error to the stream (prioritize dispatch error
                    // over save error)
                    #[allow(clippy::collapsible_if)]
                    if let Some(err) = dispatch_result.err().or(save_result.err()) {
                        if let Err(e) = tx.send(Err(err)).await {
                            tracing::error!("Failed to send error to stream: {}", e);
                        }
                    }
                }
            },
        );

        Ok(stream)
    }

    /// Compacts the context of the main agent for the given conversation and
    /// persists it. Returns metrics about the compaction (original vs.
    /// compacted tokens and messages).
    pub async fn compact_conversation(
        &self,
        active_agent_id: AgentId,
        conversation_id: &ConversationId,
    ) -> Result<CompactionResult> {
        use crate::compact::Compactor;

        // Get the conversation
        let mut conversation = self
            .services
            .find_conversation(conversation_id)
            .await?
            .ok_or_else(|| forge_domain::Error::ConversationNotFound(*conversation_id))?;

        // Get the context from the conversation
        let context = match conversation.context.as_ref() {
            Some(context) => context.clone(),
            None => {
                // No context to compact, return zero metrics
                return Ok(CompactionResult::new(0, 0, 0, 0));
            }
        };

        // Calculate original metrics
        let original_messages = context.messages.len();
        let original_token_count = *context.token_count();

        let forge_config = self.services.get_config()?;

        // Get agent and apply workflow config
        let agent = self.services.get_agent(&active_agent_id).await?;

        let Some(agent) = agent else {
            return Ok(CompactionResult::new(
                original_token_count,
                0,
                original_messages,
                0,
            ));
        };

        // Get compact config from the agent
        let compact = agent
            .apply_config(&forge_config)
            .set_compact_model_if_none()
            .compact;

        // Apply compaction using the Compactor
        let environment = self.services.get_environment();
        let compacted_context = Compactor::new(compact, environment).compact(context, true)?;

        let compacted_messages = compacted_context.messages.len();
        let compacted_tokens = *compacted_context.token_count();

        // Update the conversation with the compacted context
        conversation.context = Some(compacted_context);

        // Save the updated conversation
        self.services.upsert_conversation(conversation).await?;

        Ok(CompactionResult::new(
            original_token_count,
            compacted_tokens,
            original_messages,
            compacted_messages,
        ))
    }

    pub async fn list_tools(&self) -> Result<ToolsOverview> {
        self.tool_registry.tools_overview().await
    }

    /// Gets available models for the default provider with automatic credential
    /// refresh.
    pub async fn get_models(&self) -> Result<Vec<Model>> {
        let agent_provider_resolver = AgentProviderResolver::new(self.services.clone());
        let provider = agent_provider_resolver.get_provider(None).await?;
        let provider = self
            .services
            .provider_auth_service()
            .refresh_provider_credential(provider)
            .await?;

        self.services.models(provider).await
    }

    /// Gets available models from all configured providers concurrently.
    ///
    /// Returns a list of `ProviderModels` for each configured provider that
    /// successfully returned models. If every configured provider fails (e.g.
    /// due to an invalid API key), the first error encountered is returned so
    /// the caller receives the real underlying cause rather than an empty list.
    pub async fn get_all_provider_models(&self) -> Result<Vec<ProviderModels>> {
        let all_providers = self.services.get_all_providers().await?;

        // Build one future per configured provider, preserving the error on
        // failure.
        let futures: Vec<_> = all_providers
            .into_iter()
            .filter_map(|any_provider| any_provider.into_configured())
            .map(|provider| {
                let provider_id = provider.id.clone();
                let services = self.services.clone();
                async move {
                    let result: Result<ProviderModels> = async {
                        let refreshed = services
                            .provider_auth_service()
                            .refresh_provider_credential(provider)
                            .await?;
                        let models = services.models(refreshed).await?;
                        Ok(ProviderModels { provider_id, models })
                    }
                    .await;
                    result
                }
            })
            .collect();

        // Execute all provider fetches concurrently.
        futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
    }
}
