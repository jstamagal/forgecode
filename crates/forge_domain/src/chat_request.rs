use derive_setters::Setters;
use serde::{Deserialize, Serialize};

use crate::{ConversationId, Event, ModelId};

#[derive(Debug, Serialize, Deserialize, Clone, Setters)]
#[setters(into, strip_option)]
pub struct ChatRequest {
    pub event: Event,
    pub conversation_id: ConversationId,
    /// How many `task` levels deep this chat runs (0 for the user's own
    /// conversation; a worker started by a `task` call runs one deeper).
    #[serde(default)]
    pub task_depth: usize,
    /// Model to run on instead of the agent's configured model (a `task`
    /// worker's `model` override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<ModelId>,
}

impl ChatRequest {
    pub fn new(content: Event, conversation_id: ConversationId) -> Self {
        Self {
            event: content,
            conversation_id,
            task_depth: 0,
            model_override: None,
        }
    }
}
