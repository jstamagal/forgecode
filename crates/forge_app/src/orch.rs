use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_recursion::async_recursion;
use derive_setters::Setters;
use forge_domain::{Agent, *};
use forge_template::Element;
use futures::future::join_all;
use tokio::sync::Notify;
use tracing::warn;

use crate::agent::AgentService;
use crate::transformers::{DropReasoningOnlyMessages, ModelSpecificReasoning};
use crate::{EnvironmentInfra, TemplateEngine};

/// X12: budget for all tool results of one assistant message.
const MAX_BATCH_RESULT_BYTES: usize = 200 * 1024;

/// X6: splits `indices` (model order) into groups that run together:
/// consecutive read-only calls form one group; any other call is a group of
/// its own.
fn readers_writer_groups(indices: &[usize], read_only: impl Fn(usize) -> bool) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut readers: Vec<usize> = Vec::new();
    for &index in indices {
        if read_only(index) {
            readers.push(index);
        } else {
            if !readers.is_empty() {
                groups.push(std::mem::take(&mut readers));
            }
            groups.push(vec![index]);
        }
    }
    if !readers.is_empty() {
        groups.push(readers);
    }
    groups
}

#[cfg(test)]
mod batch_tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_readers_writer_groups_split_at_writers() {
        // r r w r r r w
        let read_only = [true, true, false, true, true, true, false];
        let actual = readers_writer_groups(&[0, 1, 2, 3, 4, 5, 6], |i| read_only[i]);
        let expected = vec![vec![0, 1], vec![2], vec![3, 4, 5], vec![6]];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_resolve_surface_name_handles_case_alias_and_mcp_forms() {
        let surface = vec![
            ToolDefinition::new("read"),
            ToolDefinition::new("fs_search"),
            ToolDefinition::new("mcp_github_tool_list_issues"),
        ];
        let actual = (
            resolve_surface_name(&ToolName::new("Read"), &surface),
            resolve_surface_name(&ToolName::new("search"), &surface),
            resolve_surface_name(&ToolName::new("mcp__github__list_issues"), &surface),
            resolve_surface_name(&ToolName::new("nope"), &surface),
        );
        let expected = (
            Some(ToolName::new("read")),
            Some(ToolName::new("fs_search")),
            Some(ToolName::new("mcp_github_tool_list_issues")),
            None,
        );
        assert_eq!(actual, expected);
    }
}

/// The surface name a called name resolves to: exact, then case-insensitive
/// native names and the deprecated `search` alias, then the MCP name forms.
/// The surface holds every tool resolved for the agent (native, agents, MCP).
fn resolve_surface_name(name: &ToolName, surface: &[ToolDefinition]) -> Option<ToolName> {
    if surface.iter().any(|tool| &tool.name == name) {
        return Some(name.clone());
    }
    let lowered = name.as_str().trim().to_lowercase();
    let alias = match lowered.as_str() {
        "search" => "fs_search".to_string(),
        other => other.to_string(),
    };
    if let Some(tool) = surface
        .iter()
        .find(|tool| tool.name.as_str().to_lowercase() == alias)
    {
        return Some(tool.name.clone());
    }
    let legacy = name.to_legacy_mcp_name()?;
    surface
        .iter()
        .find(|tool| tool.name == legacy)
        .map(|tool| tool.name.clone())
}

#[derive(Clone, Setters)]
#[setters(into)]
pub struct Orchestrator<S> {
    services: Arc<S>,
    sender: Option<ArcSender>,
    conversation: Conversation,
    tool_definitions: Vec<ToolDefinition>,
    models: Vec<Model>,
    agent: Agent,
    error_tracker: ToolErrorTracker,
    hook: Arc<Hook>,
    config: forge_config::ForgeConfig,
    /// `task` nesting depth of this conversation (0 = the user's own).
    task_depth: usize,
}

impl<S: AgentService + EnvironmentInfra<Config = forge_config::ForgeConfig>> Orchestrator<S> {
    pub fn new(
        services: Arc<S>,
        conversation: Conversation,
        agent: Agent,
        config: forge_config::ForgeConfig,
    ) -> Self {
        Self {
            conversation,
            services,
            agent,
            config,
            sender: Default::default(),
            tool_definitions: Default::default(),
            models: Default::default(),
            error_tracker: Default::default(),
            hook: Arc::new(Hook::default()),
            task_depth: 0,
        }
    }

    /// Get a reference to the internal conversation
    pub fn get_conversation(&self) -> &Conversation {
        &self.conversation
    }

    // Helper function to get all tool results from a vector of tool calls
    //
    // X6: `task` calls run first, concurrently. The other calls then run in
    // model order under a readers–writer rule: consecutive read-only calls run
    // together (at most `MAX_PARALLEL_TOOLS` at once) and any other call runs
    // alone as a barrier. Results come back in the model's order.
    #[async_recursion]
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCallFull],
        tool_context: &ToolCallContext,
    ) -> anyhow::Result<Vec<(ToolCallFull, ToolResult)>> {
        const MAX_PARALLEL_TOOLS: usize = 8;
        let task_tool_name = ToolKind::Task.name();

        // Use a case-insensitive comparison since the model may send "Task" or
        // "task".
        let is_task = |tc: &ToolCallFull| {
            tc.name
                .as_str()
                .eq_ignore_ascii_case(task_tool_name.as_str())
        };

        let mut records: Vec<Option<ToolResult>> = vec![None; tool_calls.len()];

        // Execute task tool calls in parallel — mirrors how direct
        // agent-as-tool calls work.
        let task_indices: Vec<usize> = (0..tool_calls.len())
            .filter(|&i| is_task(&tool_calls[i]))
            .collect();
        let task_results = join_all(task_indices.iter().map(|&i| {
            self.services
                .call(&self.agent, tool_context, tool_calls[i].clone())
        }))
        .await;
        for (index, result) in task_indices.iter().zip(task_results) {
            records[*index] = Some(result);
        }

        let system_tools = self
            .tool_definitions
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();

        let leaves: Vec<usize> = (0..tool_calls.len())
            .filter(|&i| !is_task(&tool_calls[i]))
            .collect();
        let read_only = |call: &ToolCallFull| {
            ToolCatalog::try_from(call.clone()).is_ok_and(|tool| tool.is_read_only())
        };
        for group in readers_writer_groups(&leaves, |i| read_only(&tool_calls[i])) {
            if group.len() == 1 {
                let index = group[0];
                let result = self
                    .run_leaf(&tool_calls[index], tool_context, &system_tools)
                    .await?;
                records[index] = Some(result);
                continue;
            }
            for chunk in group.chunks(MAX_PARALLEL_TOOLS) {
                let started = self
                    .start_leaves(chunk.iter().map(|&i| &tool_calls[i]), &system_tools)
                    .await?;
                let results = join_all(chunk.iter().map(|&i| {
                    self.services
                        .call(&self.agent, tool_context, tool_calls[i].clone())
                }))
                .await;
                for ((&index, result), is_system_tool) in chunk.iter().zip(results).zip(started) {
                    self.finish_leaf(&tool_calls[index], &result, is_system_tool)
                        .await?;
                    records[index] = Some(result);
                }
            }
        }

        Ok(tool_calls
            .iter()
            .cloned()
            .zip(records)
            .map(|(call, result)| {
                let result = result.unwrap_or_else(|| {
                    ToolResult::from(call.clone())
                        .output(Ok(ToolOutput::text(ORPHAN_RESULT).is_error(true)))
                });
                (call, result)
            })
            .collect())
    }

    /// Sends start notifications and lifecycle events for calls about to run
    /// together; returns, per call, whether it is a system tool.
    async fn start_leaves<'a>(
        &mut self,
        calls: impl Iterator<Item = &'a ToolCallFull>,
        system_tools: &HashSet<ToolName>,
    ) -> anyhow::Result<Vec<bool>> {
        let mut started = Vec::new();
        for tool_call in calls {
            let is_system_tool = system_tools.contains(&tool_call.name);
            if is_system_tool {
                let notifier = Arc::new(Notify::new());
                self.send(ChatResponse::ToolCallStart {
                    tool_call: tool_call.clone(),
                    notifier: notifier.clone(),
                })
                .await?;
                // Wait for the UI to acknowledge it has rendered the tool
                // header before we execute the tool.
                notifier.notified().await;
            }
            let toolcall_start_event = LifecycleEvent::ToolcallStart(EventData::new(
                self.agent.clone(),
                self.agent.model.clone(),
                ToolcallStartPayload::new(tool_call.clone()),
            ));
            self.hook
                .handle(&toolcall_start_event, &mut self.conversation)
                .await?;
            started.push(is_system_tool);
        }
        Ok(started)
    }

    /// Fires the end lifecycle event and the end notification for one call.
    async fn finish_leaf(
        &mut self,
        tool_call: &ToolCallFull,
        tool_result: &ToolResult,
        is_system_tool: bool,
    ) -> anyhow::Result<()> {
        let toolcall_end_event = LifecycleEvent::ToolcallEnd(EventData::new(
            self.agent.clone(),
            self.agent.model.clone(),
            ToolcallEndPayload::new(tool_call.clone(), tool_result.clone()),
        ));
        self.hook
            .handle(&toolcall_end_event, &mut self.conversation)
            .await?;
        if is_system_tool {
            self.send(ChatResponse::ToolCallEnd(tool_result.clone()))
                .await?;
        }
        Ok(())
    }

    /// Runs one call alone (start events, execution, end events).
    async fn run_leaf(
        &mut self,
        tool_call: &ToolCallFull,
        tool_context: &ToolCallContext,
        system_tools: &HashSet<ToolName>,
    ) -> anyhow::Result<ToolResult> {
        let started = self
            .start_leaves(std::iter::once(tool_call), system_tools)
            .await?;
        let tool_result = self
            .services
            .call(&self.agent, tool_context, tool_call.clone())
            .await;
        self.finish_leaf(tool_call, &tool_result, started[0])
            .await?;
        Ok(tool_result)
    }

    async fn send(&self, message: ChatResponse) -> anyhow::Result<()> {
        if let Some(sender) = &self.sender {
            sender.send(Ok(message)).await?
        }
        Ok(())
    }

    // Returns if agent supports tool or not.
    fn is_tool_supported(&self) -> anyhow::Result<bool> {
        let model_id = &self.agent.model;

        // Check if at agent level tool support is defined
        let tool_supported = match self.agent.tool_supported {
            Some(tool_supported) => tool_supported,
            None => {
                // If not defined at agent level, check model level

                let model = self.models.iter().find(|model| &model.id == model_id);
                model
                    .and_then(|model| model.tools_supported)
                    .unwrap_or_default()
            }
        };

        Ok(tool_supported)
    }

    async fn execute_chat_turn(
        &self,
        model_id: &ModelId,
        context: Context,
        reasoning_supported: bool,
    ) -> anyhow::Result<ChatCompletionMessageFull> {
        let tool_supported = self.is_tool_supported()?;
        let mut transformers = DefaultTransformation::default()
            // X28: every assistant tool call gets exactly one result.
            .pipe(HealOrphanedToolCalls::new())
            .pipe(SortTools::new(self.agent.tool_order()))
            .pipe(NormalizeToolCallArguments::new())
            .pipe(TransformToolCalls::new().when(|_| !tool_supported))
            .pipe(ImageHandling::new())
            // Drop ALL reasoning (including config) when reasoning is not supported by the model
            .pipe(DropReasoningDetails.when(|_| !reasoning_supported))
            // Strip all reasoning from messages when the model has changed (signatures are
            // model-specific and invalid across models). No-op when model is unchanged.
            .pipe(ReasoningNormalizer::new(model_id.clone()))
            // Normalize Anthropic reasoning knobs per model family before provider conversion.
            .pipe(
                ModelSpecificReasoning::new(model_id.as_str())
                    .when(|_| model_id.as_str().to_lowercase().contains("claude")),
            )
            // Drop reasoning-only assistant turns; Anthropic and Bedrock both reject
            // messages whose final content block is `thinking`.
            .pipe(
                DropReasoningOnlyMessages
                    .when(|_| model_id.as_str().to_lowercase().contains("claude")),
            );
        let response = self
            .services
            .chat_agent(
                model_id,
                transformers.transform(context),
                Some(self.agent.provider.clone()),
            )
            .await?;

        // Always stream content deltas
        response
            .into_full_streaming(!tool_supported, self.sender.clone())
            .await
    }

    // Create a helper method with the core functionality
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let model_id = self.get_model();

        let mut context = self.conversation.context.clone().unwrap_or_default();

        // Fire the Start lifecycle event
        let start_event = LifecycleEvent::Start(EventData::new(
            self.agent.clone(),
            model_id.clone(),
            StartPayload,
        ));
        self.hook
            .handle(&start_event, &mut self.conversation)
            .await?;

        // Signals that the loop should suspend (task may or may not be
        // completed)
        let mut should_yield = false;

        // Signals that the task is completed
        let mut is_complete = false;

        let mut request_count = 0;

        // Retrieve the number of requests allowed per tick.
        let max_requests_per_turn = self.agent.max_requests_per_turn;
        let tool_context = ToolCallContext::new(self.conversation.metrics.clone())
            .sender(self.sender.clone())
            .with_conversation_id(self.conversation.id)
            .with_task_depth(self.task_depth);

        while !should_yield {
            // Set context for the current loop iteration
            self.conversation.context = Some(context.clone());
            self.services.update(self.conversation.clone()).await?;

            // A shown read whose result was compacted away no longer backs a
            // repeat-read stub.
            crate::read_tracking::keep_shown_in_context(&tool_context, &context)?;

            let request_event = LifecycleEvent::Request(EventData::new(
                self.agent.clone(),
                model_id.clone(),
                RequestPayload::new(request_count),
            ));
            self.hook
                .handle(&request_event, &mut self.conversation)
                .await?;

            let message = crate::retry::retry_with_config(
                &self.config.clone().retry.unwrap_or_default(),
                || {
                    self.execute_chat_turn(
                        &model_id,
                        context.clone(),
                        context.is_reasoning_supported(),
                    )
                },
                self.sender.as_ref().map(|sender| {
                    let sender = sender.clone();
                    let agent_id = self.agent.id.clone();
                    let model_id = model_id.clone();
                    move |error: &anyhow::Error, duration: Duration| {
                        let root_cause = error.root_cause();
                        // Log retry attempts - critical for debugging API
                        // failures
                        tracing::error!(
                            agent_id = %agent_id,
                            error = ?root_cause,
                            model = %model_id,
                            "Retry attempt due to error"
                        );
                        let retry_event =
                            ChatResponse::RetryAttempt { cause: error.into(), duration };
                        let _ = sender.try_send(Ok(retry_event));
                    }
                }),
            )
            .await?;

            // Fire the Response lifecycle event
            let response_event = LifecycleEvent::Response(EventData::new(
                self.agent.clone(),
                model_id.clone(),
                ResponsePayload::new(message.clone()),
            ));
            self.hook
                .handle(&response_event, &mut self.conversation)
                .await?;

            // Turn is completed, if finish_reason is 'stop'. Gemini models
            // return stop as finish reason with tool calls.
            is_complete =
                message.finish_reason == Some(FinishReason::Stop) && message.tool_calls.is_empty();

            should_yield = is_complete;

            // X5: a reply cut off by its output cap with unfinished call
            // arguments runs nothing; the model is told why.
            let mut message = message;
            let mut batch_notice: Option<String> = None;
            if let Some(notice) =
                truncated_call_notice(message.finish_reason.as_ref(), &message.tool_calls)
            {
                message.tool_calls.clear();
                batch_notice = Some(notice);
            }

            // X4: drop duplicates, cap the batch, canonicalise names and
            // arguments, and reject unavailable tools and invalid arguments
            // before any call runs.
            let surface = self.tool_definitions.clone();
            let batch = normalize_batch(
                &message.tool_calls,
                &surface,
                |name| resolve_surface_name(name, &surface),
                ToolCatalog::contains,
            );
            if let Some(notice) = &batch.notice {
                batch_notice = Some(match batch_notice {
                    Some(previous) => format!("{previous}\n\n{notice}"),
                    None => notice.clone(),
                });
            }
            let runnable: Vec<ToolCallFull> = batch
                .calls
                .iter()
                .filter(|call| call.rejection.is_none())
                .map(|call| call.call.clone())
                .collect();

            // Process tool calls and update context
            let executed = self.execute_tool_calls(&runnable, &tool_context).await?;
            let mut executed = executed.into_iter();
            let mut tool_call_records: Vec<(ToolCallFull, ToolResult)> = batch
                .calls
                .into_iter()
                .map(|normalized| match normalized.rejection {
                    None => executed.next().expect("one result per runnable call"),
                    Some(rejection) => {
                        let error = match rejection {
                            CallRejection::Unavailable(message) => {
                                anyhow::Error::new(ToolRefusal(message))
                            }
                            CallRejection::InvalidArguments(message) => anyhow::anyhow!(message),
                        };
                        let result = ToolResult::from(normalized.call.clone()).failure(error);
                        (normalized.call, result)
                    }
                })
                .collect();

            // X12: hold the whole batch to the per-message budget.
            let mut results: Vec<ToolResult> = tool_call_records
                .iter()
                .map(|(_, result)| result.clone())
                .collect();
            let originals: Vec<Option<String>> = results
                .iter()
                .map(|result| result.output.as_str().map(str::to_string))
                .collect();
            crate::result_bounds::bound_batch(&mut results, MAX_BATCH_RESULT_BYTES);
            let ledger = tool_context.reads()?;
            for ((record, result), original) in
                tool_call_records.iter_mut().zip(results).zip(originals)
            {
                if let (Some(original), Some(delivered), Some(call_id)) =
                    (original, result.output.as_str(), result.call_id.as_ref())
                {
                    crate::result_bounds::narrow_read_coverage(
                        &ledger,
                        call_id.as_str(),
                        &original,
                        delivered,
                    );
                }
                record.1 = result;
            }

            // Reads delivered in this batch become "shown"; a later exact
            // repeat is answered with a stub while the result stays in the
            // conversation.
            crate::read_tracking::settle_batch(&tool_context, &tool_call_records)?;

            // Update context from conversation after response / tool-call hooks
            // run
            if let Some(updated_context) = &self.conversation.context {
                context = updated_context.clone();
            }

            self.error_tracker.adjust_record(&tool_call_records);
            let allowed_max_attempts = self.error_tracker.limit();
            for (_, result) in tool_call_records.iter_mut() {
                if result.is_error() {
                    let attempts_left = self.error_tracker.remaining_attempts(&result.name);
                    // Add attempt information to the error message so the agent
                    // can reflect on it.
                    let context = serde_json::json!({
                        "attempts_left": attempts_left,
                        "allowed_max_attempts": allowed_max_attempts,
                    });
                    let text = TemplateEngine::default()
                        .render("forge-tool-retry-message.md", &context)?;
                    let message = Element::new("retry").text(text);

                    result.output.combine_mut(ToolOutput::text(message));
                }
            }

            context = context.append_message(
                match &batch_notice {
                    Some(notice) if message.content.is_empty() => notice.clone(),
                    Some(notice) => format!("{}\n\n{notice}", message.content),
                    None => message.content.clone(),
                },
                message.thought_signature.clone(),
                message.reasoning.clone(),
                message.reasoning_details.clone(),
                message.usage,
                tool_call_records,
                message.phase,
            );

            if self.error_tracker.limit_reached() {
                self.send(ChatResponse::Interrupt {
                    reason: InterruptionReason::MaxToolFailurePerTurnLimitReached {
                        limit: *self.error_tracker.limit() as u64,
                        errors: self.error_tracker.errors().clone(),
                    },
                })
                .await?;
                // Should yield if too many errors are produced
                should_yield = true;
            }

            // Update context in the conversation
            context = SetModel::new(model_id.clone()).transform(context);
            self.conversation.context = Some(context.clone());
            self.services.update(self.conversation.clone()).await?;
            request_count += 1;

            if !should_yield && let Some(max_request_allowed) = max_requests_per_turn {
                // Check if agent has reached the maximum request per turn limit
                if request_count >= max_request_allowed {
                    // Log warning - important for understanding conversation
                    // interruptions
                    warn!(
                        agent_id = %self.agent.id,
                        model_id = %model_id,
                        request_count,
                        max_request_allowed,
                        "Agent has reached the maximum request per turn limit"
                    );
                    // raise an interrupt event to notify the UI
                    self.send(ChatResponse::Interrupt {
                        reason: InterruptionReason::MaxRequestPerTurnLimitReached {
                            limit: max_request_allowed as u64,
                        },
                    })
                    .await?;
                    // force completion
                    should_yield = true;
                }
            }

            // Update metrics in conversation
            tool_context.with_metrics(|metrics| {
                self.conversation.metrics = metrics.clone();
            })?;

            // If completing (should_yield is due), fire End hook and check if
            // it adds messages
            if should_yield {
                let end_count_before = self.conversation.len();
                self.hook
                    .handle(
                        &LifecycleEvent::End(EventData::new(
                            self.agent.clone(),
                            model_id.clone(),
                            EndPayload,
                        )),
                        &mut self.conversation,
                    )
                    .await?;
                self.services.update(self.conversation.clone()).await?;
                // Check if End hook added messages - if so, continue the loop
                if self.conversation.len() > end_count_before {
                    // End hook added messages, sync context and continue
                    if let Some(updated_context) = &self.conversation.context {
                        context = updated_context.clone();
                    }
                    should_yield = false;
                }
            }
        }

        self.services.update(self.conversation.clone()).await?;

        // Signal Task Completion
        if is_complete {
            self.send(ChatResponse::TaskComplete).await?;
        }

        Ok(())
    }

    fn get_model(&self) -> ModelId {
        self.agent.model.clone()
    }
}
