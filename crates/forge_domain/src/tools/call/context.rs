use std::sync::{Arc, Mutex};

use derive_setters::Setters;

use crate::{
    ArcSender, ChatResponse, ConversationId, Metrics, SharedLedger, TitleFormat, Todo, TodoItem,
};

/// Provides additional context for tool calls.
#[derive(Debug, Clone, Setters)]
pub struct ToolCallContext {
    sender: Option<ArcSender>,
    metrics: Arc<Mutex<Metrics>>,
    /// Conversation the call belongs to (owns the undo stacks).
    #[setters(skip)]
    conversation_id: ConversationId,
    /// Id of the tool call being executed, when known.
    #[setters(skip)]
    call_id: Option<String>,
    /// Whether the current model accepts image input.
    #[setters(skip)]
    vision: bool,
    /// How many `task` levels deep the conversation runs (0 = the user's own).
    #[setters(skip)]
    task_depth: usize,
}

impl ToolCallContext {
    /// Creates a new ToolCallContext with default values
    pub fn new(metrics: Metrics) -> Self {
        Self {
            sender: None,
            metrics: Arc::new(Mutex::new(metrics)),
            conversation_id: ConversationId::default(),
            call_id: None,
            vision: false,
            task_depth: 0,
        }
    }

    /// Sets the `task` nesting depth of the conversation.
    pub fn with_task_depth(mut self, task_depth: usize) -> Self {
        self.task_depth = task_depth;
        self
    }

    /// The `task` nesting depth of the conversation.
    pub fn task_depth(&self) -> usize {
        self.task_depth
    }

    /// Binds the context to a conversation.
    pub fn with_conversation_id(mut self, conversation_id: ConversationId) -> Self {
        self.conversation_id = conversation_id;
        self
    }

    /// Binds the context to a tool call id.
    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    /// Sets whether the current model accepts image input.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// Whether the current model accepts image input.
    pub fn vision(&self) -> bool {
        self.vision
    }

    /// The conversation the call belongs to.
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }

    /// The id of the tool call being executed, if any.
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// A copy of this context bound to another tool call id; it shares the
    /// metrics and sender.
    pub fn for_call(&self, call_id: Option<String>) -> Self {
        Self { call_id, ..self.clone() }
    }

    /// The conversation's read ledger (shared, not a copy).
    ///
    /// # Errors
    /// Returns an error if the metrics lock cannot be acquired.
    pub fn reads(&self) -> anyhow::Result<SharedLedger> {
        self.with_metrics(|metrics| metrics.reads.clone())
    }

    /// Send a message through the sender if available
    pub async fn send(&self, agent_message: impl Into<ChatResponse>) -> anyhow::Result<()> {
        if let Some(sender) = &self.sender {
            sender.send(Ok(agent_message.into())).await?
        }
        Ok(())
    }

    /// Send tool input title - MUST ONLY be used for presenting tool input
    /// information
    pub async fn send_tool_input(&self, title: impl Into<TitleFormat>) -> anyhow::Result<()> {
        let title = title.into();
        self.send(ChatResponse::TaskMessage {
            content: crate::ChatResponseContent::ToolInput(title),
        })
        .await
    }

    /// Execute a closure with access to the metrics
    pub fn with_metrics<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Metrics) -> R,
    {
        let mut metrics = self
            .metrics
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire metrics lock"))?;
        Ok(f(&mut metrics))
    }

    /// Execute a fallible closure with access to the metrics
    pub fn try_with_metrics<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Metrics) -> anyhow::Result<R>,
    {
        let mut metrics = self
            .metrics
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire metrics lock"))?;
        f(&mut metrics)
    }

    /// Returns all known todos (active and historical completed todos).
    ///
    /// # Errors
    ///
    /// Returns an error if the metrics lock cannot be acquired.
    pub fn get_todos(&self) -> anyhow::Result<Vec<Todo>> {
        self.with_metrics(|metrics| metrics.get_todos().to_vec())
    }

    /// Applies incremental todo changes using content as the matching key.
    ///
    /// # Arguments
    ///
    /// * `changes` - Todo items to add, update, or remove (via `cancelled`
    ///   status).
    ///
    /// # Errors
    ///
    /// Returns an error if the metrics lock cannot be acquired or todo
    /// validation fails.
    pub fn update_todos(&self, changes: Vec<TodoItem>) -> anyhow::Result<Vec<Todo>> {
        self.try_with_metrics(|metrics| metrics.apply_todo_changes(changes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_context() {
        let metrics = Metrics::default();
        let context = ToolCallContext::new(metrics);
        assert!(context.sender.is_none());
    }

    #[test]
    fn test_with_sender() {
        let metrics = Metrics::default();
        let context = ToolCallContext::new(metrics);
        assert!(context.sender.is_none());
    }
}
