//! Repeat-read bookkeeping (X24) at the batch boundary.
//!
//! A `read` call offers its rendered window to the ledger while it runs. Once
//! the batch's results are final, each offered read becomes a *shown* read
//! keyed by the digest of the result text the conversation holds. Before each
//! model request, shown reads whose result is no longer in the context (for
//! example after compaction) are forgotten, so a later read returns the lines
//! in full again.

use std::collections::HashMap;

use forge_domain::{Context, ToolCallContext, ToolCallFull, ToolResult};

/// Digest of a tool result's text, as the ledger compares it.
fn digest(result: &ToolResult) -> String {
    use sha2::{Digest, Sha256};
    let text: String = result
        .output
        .values
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Settles the reads a batch offered: each successful read result becomes a
/// shown read; per-call windows are dropped.
///
/// # Errors
/// Returns an error if the metrics lock cannot be acquired.
pub fn settle_batch(
    context: &ToolCallContext,
    records: &[(ToolCallFull, ToolResult)],
) -> anyhow::Result<()> {
    let ledger = context.reads()?;
    ledger.with(|l| {
        for (_, result) in records {
            let Some(call_id) = &result.call_id else {
                continue;
            };
            if result.is_error() {
                l.withdraw_offer(call_id.as_str());
            } else {
                l.settle_read(call_id.as_str(), &digest(result));
            }
        }
        l.settle_windows();
    });
    Ok(())
}

/// Forgets shown reads whose result text is no longer in `conversation`.
///
/// # Errors
/// Returns an error if the metrics lock cannot be acquired.
pub fn keep_shown_in_context(
    context: &ToolCallContext,
    conversation: &Context,
) -> anyhow::Result<()> {
    let results: HashMap<String, String> = conversation
        .messages
        .iter()
        .filter_map(|message| message.as_tool_result())
        .filter_map(|result| {
            result
                .call_id
                .as_ref()
                .map(|id| (id.as_str().to_string(), digest(result)))
        })
        .collect();
    context.reads()?.with(|l| l.keep_shown(&results));
    Ok(())
}

#[cfg(test)]
mod tests {
    use forge_domain::{
        ContextMessage, Metrics, ReadKey, ToolCallId, ToolName, ToolOutput, ToolResult,
    };
    use pretty_assertions::assert_eq;

    use super::*;

    fn result(call_id: &str, text: &str) -> ToolResult {
        ToolResult::new(ToolName::new("read"))
            .call_id(ToolCallId::new(call_id))
            .output(Ok(ToolOutput::text(text)))
    }

    fn key() -> ReadKey {
        ReadKey { hash: "h".to_string(), start: 1, end: 2, numbered: true }
    }

    #[test]
    fn test_settled_read_is_kept_only_while_its_result_is_in_context() {
        let fixture = ToolCallContext::new(Metrics::default());
        fixture
            .reads()
            .unwrap()
            .with(|l| l.offer_read("c1", "/a", key()));
        let read = result("c1", "1:a\n2:b");
        settle_batch(&fixture, &[(ToolCallFull::new("read"), read.clone())]).unwrap();

        let present = Context::default().add_message(ContextMessage::Tool(read));
        keep_shown_in_context(&fixture, &present).unwrap();
        let still_shown = fixture
            .reads()
            .unwrap()
            .with(|l| l.shown_read("/a", &key()).map(str::to_string));
        keep_shown_in_context(&fixture, &Context::default()).unwrap();
        let after_compaction = fixture
            .reads()
            .unwrap()
            .with(|l| l.shown_read("/a", &key()).map(str::to_string));

        let actual = (still_shown, after_compaction);
        let expected = (Some("c1".to_string()), None);
        assert_eq!(actual, expected);
    }
}
