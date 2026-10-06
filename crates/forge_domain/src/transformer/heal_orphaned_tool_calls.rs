use std::collections::HashSet;

use super::Transformer;
use crate::{Context, ContextMessage, MessageEntry, ToolCallId, ToolOutput, ToolResult};

/// The result text a healed call receives.
pub const ORPHAN_RESULT: &str = "interrupted; no result was recorded for this tool call";

/// X28: makes every assistant tool call be followed by exactly one result.
///
/// Before each request, an assistant tool call with no result (a crash, a
/// cancelled turn) gets a synthetic error result right after the results it
/// does have. A second result for a call already answered, or a result whose
/// call is not in the preceding assistant message, is dropped: providers
/// reject both shapes.
#[derive(Debug, Clone, Default)]
pub struct HealOrphanedToolCalls;

impl HealOrphanedToolCalls {
    /// Creates the transformer.
    pub fn new() -> Self {
        Self
    }
}

impl Transformer for HealOrphanedToolCalls {
    type Value = Context;

    fn transform(&mut self, mut context: Self::Value) -> Self::Value {
        let messages = std::mem::take(&mut context.messages);
        let mut healed: Vec<MessageEntry> = Vec::with_capacity(messages.len());
        let mut iter = messages.into_iter().peekable();
        while let Some(entry) = iter.next() {
            if matches!(entry.message, ContextMessage::Tool(_)) {
                // A result with no call right before it.
                continue;
            }
            let calls = match &entry.message {
                ContextMessage::Text(text) => text.tool_calls.clone().unwrap_or_default(),
                _ => Vec::new(),
            };
            healed.push(entry);
            if calls.is_empty() {
                continue;
            }
            let expected: HashSet<ToolCallId> = calls
                .iter()
                .filter_map(|call| call.call_id.clone())
                .collect();
            let mut answered: HashSet<ToolCallId> = HashSet::new();
            while let Some(next) = iter.peek() {
                let ContextMessage::Tool(result) = &next.message else {
                    break;
                };
                let keep = match &result.call_id {
                    Some(id) => expected.contains(id) && answered.insert(id.clone()),
                    // Results without ids cannot be matched; keep them.
                    None => true,
                };
                let next = iter.next().expect("peeked entry exists");
                if keep {
                    healed.push(next);
                }
            }
            for call in &calls {
                if let Some(id) = &call.call_id
                    && !answered.contains(id)
                {
                    let result = ToolResult::new(call.name.clone())
                        .call_id(id.clone())
                        .output(Ok(ToolOutput::text(ORPHAN_RESULT).is_error(true)));
                    healed.push(ContextMessage::Tool(result).into());
                }
            }
        }
        context.messages = healed;
        context
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{ToolCallFull, ToolName};

    fn call(id: &str) -> ToolCallFull {
        ToolCallFull::new("read").call_id(ToolCallId::new(id))
    }

    fn result(id: &str, text: &str) -> ContextMessage {
        ContextMessage::Tool(
            ToolResult::new(ToolName::new("read"))
                .call_id(ToolCallId::new(id))
                .output(Ok(ToolOutput::text(text))),
        )
    }

    fn texts(context: &Context) -> Vec<String> {
        context
            .messages
            .iter()
            .map(|entry| match &entry.message {
                ContextMessage::Tool(result) => {
                    format!(
                        "{}={}",
                        result.call_id.as_ref().unwrap().as_str(),
                        result.output.as_str().unwrap()
                    )
                }
                ContextMessage::Text(text) => format!("{:?}", text.role),
                ContextMessage::Image(_) => "image".to_string(),
            })
            .collect()
    }

    #[test]
    fn test_missing_result_is_backfilled_and_strays_dropped() {
        let fixture = Context::default()
            .add_message(result("stray", "x"))
            .add_message(ContextMessage::assistant(
                "",
                None,
                None,
                Some(vec![call("a"), call("b")]),
            ))
            .add_message(result("a", "ok"))
            .add_message(result("a", "late duplicate"))
            .add_message(ContextMessage::user("next", None));

        let actual = texts(&HealOrphanedToolCalls::new().transform(fixture));

        let expected = vec![
            "Assistant".to_string(),
            "a=ok".to_string(),
            format!("b={ORPHAN_RESULT}"),
            "User".to_string(),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_balanced_context_is_unchanged() {
        let fixture = Context::default()
            .add_message(ContextMessage::assistant(
                "",
                None,
                None,
                Some(vec![call("a")]),
            ))
            .add_message(result("a", "ok"));
        let actual = HealOrphanedToolCalls::new().transform(fixture.clone());
        assert_eq!(actual, fixture);
    }
}
