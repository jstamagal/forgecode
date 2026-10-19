use derive_setters::Setters;
use forge_template::Element;
use serde::{Deserialize, Serialize};

use crate::{ConversationId, Image, ToolCallFull, ToolCallId, ToolName};

const REFLECTION_PROMPT: &str =
    include_str!("../../../../templates/forge-partial-tool-error-reflection.md");

/// A refusal that is not the model's mistake with the tool itself: the tool
/// is unavailable or blocked, or a jail/ban/hook refused the call. It is
/// returned to the model as an error but does not count toward the
/// repeated-tool-failure limit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolRefusal(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, Setters)]
#[setters(into)]
pub struct ToolResult {
    pub name: ToolName,
    pub call_id: Option<ToolCallId>,
    #[setters(skip)]
    pub output: ToolOutput,
}

impl ToolResult {
    pub fn new(name: impl Into<ToolName>) -> ToolResult {
        Self {
            name: name.into(),
            call_id: Default::default(),
            output: Default::default(),
        }
    }

    pub fn success(mut self, content: impl Into<String>) -> Self {
        self.output = ToolOutput::text(content.into());

        self
    }

    pub fn failure(self, err: anyhow::Error) -> Self {
        self.output(Err(err))
    }

    pub fn is_error(&self) -> bool {
        self.output.is_error
    }

    /// Whether this result is an error that counts toward the
    /// repeated-tool-failure limit (refusals do not).
    pub fn is_counted_error(&self) -> bool {
        self.output.is_error && !self.output.uncounted
    }

    pub fn output(mut self, result: Result<ToolOutput, anyhow::Error>) -> Self {
        match result {
            Ok(output) => {
                self.output = output;
            }
            Err(err) => {
                let refusal = err.downcast_ref::<ToolRefusal>().is_some();
                let mut message = vec![err.to_string()];
                let mut source = err.source();
                if source.is_some() {
                    message.push("\nCaused by:".to_string());
                }
                let mut i = 0;
                while let Some(err) = source {
                    message.push(format!("    {i}: {err}"));
                    source = err.source();
                    i += 1;
                }

                self.output = ToolOutput::text(
                    Element::new("tool_call_error")
                        .append(Element::new("cause").cdata(message.join("\n")))
                        .append(Element::new("reflection").text(REFLECTION_PROMPT)),
                )
                .is_error(true)
                .uncounted(refusal);
            }
        }
        self
    }
}

impl From<ToolCallFull> for ToolResult {
    fn from(value: ToolCallFull) -> Self {
        Self {
            name: value.name,
            call_id: value.call_id,
            output: Default::default(),
        }
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Setters)]
#[setters(into, strip_option)]
pub struct ToolOutput {
    pub is_error: bool,
    pub values: Vec<ToolValue>,
    /// A refusal error that does not count toward the repeated-failure
    /// limit. Only meaningful within the turn that produced it.
    #[serde(default, skip)]
    pub uncounted: bool,
}

impl ToolOutput {
    pub fn text(tool: impl ToString) -> Self {
        ToolOutput {
            is_error: Default::default(),
            values: vec![ToolValue::Text(tool.to_string())],
            uncounted: false,
        }
    }

    pub fn ai(id: ConversationId, output: impl ToString) -> Self {
        ToolOutput {
            is_error: Default::default(),
            values: vec![ToolValue::AI { value: output.to_string(), conversation_id: id }],
            uncounted: false,
        }
    }

    pub fn image(img: Image) -> Self {
        ToolOutput {
            is_error: false,
            values: vec![ToolValue::Image(img)],
            uncounted: false,
        }
    }

    pub fn combine_mut(&mut self, value: ToolOutput) {
        self.values.extend(value.values);
    }

    pub fn combine(self, other: ToolOutput) -> Self {
        let mut items = self.values;
        items.extend(other.values);
        ToolOutput {
            values: items,
            is_error: self.is_error || other.is_error,
            uncounted: self.uncounted && other.uncounted,
        }
    }

    /// Returns the first item as a string if it exists
    pub fn as_str(&self) -> Option<&str> {
        self.values.iter().find_map(|item| item.as_str())
    }
}

impl<T> From<T> for ToolOutput
where
    T: Iterator<Item = ToolOutput>,
{
    fn from(item: T) -> Self {
        item.fold(ToolOutput::default(), |acc, item| acc.combine(item))
    }
}

/// Like serde_json::Value, ToolValue represents all the primitive values that
/// tools can produce.
#[derive(Default, Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ToolValue {
    Text(String),
    AI {
        value: String,
        conversation_id: ConversationId,
    },
    Image(Image),
    #[default]
    Empty,
}

impl ToolValue {
    pub fn text(text: String) -> Self {
        ToolValue::Text(text)
    }

    pub fn image(img: Image) -> Self {
        ToolValue::Image(img)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            ToolValue::Text(text) => Some(text),
            ToolValue::Image(_) => None,
            ToolValue::Empty => None,
            ToolValue::AI { value, .. } => Some(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_success_and_failure_content() {
        let success = ToolResult::new(ToolName::new("test_tool")).success("success message");
        assert!(!success.is_error());
        assert_eq!(success.output.as_str().unwrap(), "success message");

        let failure = ToolResult::new(ToolName::new("test_tool")).failure(
            anyhow::anyhow!("error 1")
                .context("error 2")
                .context("error 3"),
        );
        assert!(failure.is_error());
        insta::assert_snapshot!(failure.output.as_str().unwrap());
    }

    #[test]
    fn test_refusal_is_an_uncounted_error() {
        let fixture = ToolResult::new(ToolName::new("read"));
        let refused = fixture
            .clone()
            .failure(anyhow::Error::new(ToolRefusal("blocked".to_string())));
        let failed = fixture.failure(anyhow::anyhow!("boom"));
        let actual = (
            refused.is_error(),
            refused.is_counted_error(),
            failed.is_counted_error(),
        );
        let expected = (true, false, true);
        assert_eq!(actual, expected);
    }
}
