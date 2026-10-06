//! Executor-side batch handling for one assistant message's tool calls:
//! argument repair and container coercion (X3), strict schema validation
//! before deserialisation (X2), duplicate removal, the per-message call cap
//! and unavailable-tool rejection (X4), the truncated-arguments guard (X5)
//! and "did you mean" suggestions (X22).

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::{FinishReason, ToolCallArguments, ToolCallFull, ToolDefinition, ToolName};

/// Most calls kept from one assistant message.
pub const MAX_CALLS_PER_MESSAGE: usize = 50;

/// Why a call in a batch was not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallRejection {
    /// The tool is not on this request's surface. Refusal (uncounted).
    Unavailable(String),
    /// The arguments do not fit the tool's schema. Counted error.
    InvalidArguments(String),
}

impl CallRejection {
    /// The message returned to the model.
    pub fn message(&self) -> &str {
        match self {
            CallRejection::Unavailable(message) | CallRejection::InvalidArguments(message) => {
                message
            }
        }
    }
}

/// One call after normalisation.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedCall {
    /// The call with canonical name and repaired, coerced arguments.
    pub call: ToolCallFull,
    /// Set when the call must not run.
    pub rejection: Option<CallRejection>,
}

/// The outcome of normalising one assistant message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NormalizedBatch {
    /// Calls kept, in model order.
    pub calls: Vec<NormalizedCall>,
    /// Exact duplicates dropped.
    pub duplicates: usize,
    /// Calls dropped beyond the cap.
    pub capped: usize,
    /// Notice to append to the assistant text, if any.
    pub notice: Option<String>,
}

/// Whether some call's raw arguments are not a complete JSON object. Empty
/// arguments are a complete (empty) object.
pub fn has_dangling_arguments(calls: &[ToolCallFull]) -> bool {
    calls.iter().any(|call| match &call.arguments {
        ToolCallArguments::Unparsed(raw) => {
            !raw.trim().is_empty()
                && !matches!(serde_json::from_str::<Value>(raw), Ok(Value::Object(_)))
        }
        ToolCallArguments::Parsed(value) => !value.is_object(),
    })
}

/// X5: when the response was cut off by its output cap and some call's
/// arguments are incomplete, all calls are dropped and this notice explains
/// why. Returns `None` when the calls can run.
pub fn truncated_call_notice(
    finish_reason: Option<&FinishReason>,
    calls: &[ToolCallFull],
) -> Option<String> {
    if finish_reason != Some(&FinishReason::Length) || calls.is_empty() {
        return None;
    }
    if !has_dangling_arguments(calls) {
        return None;
    }
    let mut names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    Some(format!(
        "Response incomplete (length); dropped truncated tool call arguments for {}. Retry the request or ask for the change in smaller chunks.",
        names.join(", ")
    ))
}

/// The published tool a mistyped `name` most likely meant: a name it ends
/// with (`functions_read` → `read`), else the closest by similarity (the
/// matching-character ratio, cutoff 0.6, as Python's `difflib` computes it).
pub fn closest_tool_name<'a>(
    name: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let candidates: Vec<&str> = names.into_iter().collect();
    let lowered = name.to_lowercase();
    if let Some(suffix) = candidates
        .iter()
        .filter(|candidate| lowered.ends_with(&format!("_{}", candidate.to_lowercase())))
        .max_by_key(|candidate| candidate.len())
    {
        return Some(suffix.to_string());
    }
    candidates
        .iter()
        .map(|candidate| (match_ratio(&lowered, &candidate.to_lowercase()), candidate))
        .filter(|(score, _)| *score >= 0.6)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, candidate)| candidate.to_string())
}

/// 2·M / (|a| + |b|) where M counts characters in matching blocks of a
/// longest-common-subsequence alignment.
fn match_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 0..a.len() {
        for j in 0..b.len() {
            table[i + 1][j + 1] = if a[i] == b[j] {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    2.0 * table[a.len()][b.len()] as f64 / (a.len() + b.len()) as f64
}

/// The error for a call to a tool this request does not offer.
///
/// # Arguments
/// * `name` - The name the model called.
/// * `surface` - Names of the tools on this request.
/// * `known` - Whether the name is a real tool this agent may not use.
pub fn unavailable_message(name: &str, surface: &[&str], known: bool) -> String {
    if known {
        return format!("{name} is not allowed by this agent's tool policy.");
    }
    let mut message = format!("unknown tool {name}.");
    if let Some(closest) = closest_tool_name(name, surface.iter().copied()) {
        message.push_str(&format!(" Did you mean {closest}?"));
    }
    if surface.contains(&"tool_discovery") {
        message.push_str(
            " Use tool_discovery to find an allowed tool and load its exact catalog id before calling it.",
        );
    } else {
        message.push_str(&format!(" This agent's tools: {}.", surface.join(", ")));
    }
    message
}

/// Parses a string that holds JSON where `schema` declares a container
/// (array or object) and the string parses to exactly that type. Recurses
/// into object properties and array items.
pub fn coerce_json_containers(value: Value, schema: &Value) -> Value {
    let Some(schema) = schema.as_object() else {
        return value;
    };
    let types = schema_types(schema);
    let value = match value {
        Value::String(text) if !types.contains(&"string") => {
            let parsed = serde_json::from_str::<Value>(&text).ok().filter(|parsed| {
                (types.contains(&"array") && parsed.is_array())
                    || (types.contains(&"object") && parsed.is_object())
            });
            parsed.unwrap_or(Value::String(text))
        }
        other => other,
    };
    match value {
        Value::Object(map) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            Value::Object(
                map.into_iter()
                    .map(|(key, item)| {
                        let item = match properties.and_then(|p| p.get(&key)) {
                            Some(sub) => coerce_json_containers(item, sub),
                            None => item,
                        };
                        (key, item)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => match schema.get("items") {
            Some(sub) if sub.is_object() => Value::Array(
                items
                    .into_iter()
                    .map(|item| coerce_json_containers(item, sub))
                    .collect(),
            ),
            _ => Value::Array(items),
        },
        other => other,
    }
}

fn schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_fits(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        other => type_name(value) == other,
    }
}

/// One line saying how `value` breaks `schema`, or `None` when it fits. Covers
/// the keywords tool schemas use: `type`, `nullable`, `enum`, `required`,
/// `properties`, `additionalProperties: false`, `items`, `minItems`,
/// `maxItems`, `minimum`, `maximum`, `anyOf`/`oneOf`. Other keywords are
/// ignored. A root schema that does not declare an object (a definition with
/// no input schema) accepts anything.
pub fn schema_error(value: &Value, schema: &Value) -> Option<String> {
    let declares_object = schema.as_object().is_some_and(|root| {
        schema_types(root).contains(&"object") || root.contains_key("properties")
    });
    if !declares_object {
        return None;
    }
    validate_at(value, schema, "")
}

fn at(path: &str) -> String {
    if path.is_empty() {
        "arguments".to_string()
    } else {
        format!("`{path}`")
    }
}

fn validate_at(value: &Value, schema: &Value, path: &str) -> Option<String> {
    let schema = schema.as_object()?;
    if value.is_null() && schema.get("nullable").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array)
            && !options.is_empty()
        {
            let errors: Vec<String> = options
                .iter()
                .filter_map(|option| validate_at(value, option, path))
                .collect();
            if errors.len() == options.len() {
                return errors.into_iter().next();
            }
        }
    }
    let types = schema_types(schema);
    if !types.is_empty() && !types.iter().any(|kind| type_fits(value, kind)) {
        return Some(format!(
            "{} must be {}, got {}",
            at(path),
            types.join(" or "),
            type_name(value)
        ));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        let options: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Some(format!(
            "{} must be one of {}",
            at(path),
            options.join(", ")
        ));
    }
    match value {
        Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                let missing: Vec<&str> = required
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| !map.contains_key(*name))
                    .collect();
                if !missing.is_empty() {
                    return Some(format!(
                        "missing required {}: {}",
                        if missing.len() == 1 {
                            "argument"
                        } else {
                            "arguments"
                        },
                        missing.join(", ")
                    ));
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                let mut unknown: Vec<&str> = map
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !properties.is_some_and(|p| p.contains_key(*key)))
                    .collect();
                if !unknown.is_empty() {
                    unknown.sort_unstable();
                    let allowed: Vec<&str> = properties
                        .map(|p| p.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    return Some(format!(
                        "unknown {} {}{}; allowed: {}",
                        if unknown.len() == 1 {
                            "argument"
                        } else {
                            "arguments"
                        },
                        unknown.join(", "),
                        if path.is_empty() {
                            String::new()
                        } else {
                            format!(" in {}", at(path))
                        },
                        allowed.join(", ")
                    ));
                }
            }
            if let Some(properties) = properties {
                for (key, item) in map {
                    if let Some(sub) = properties.get(key) {
                        let child = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        if let Some(error) = validate_at(item, sub, &child) {
                            return Some(error);
                        }
                    }
                }
            }
            None
        }
        Value::Array(items) => {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                return Some(format!("{} must have at least {min} item(s)", at(path)));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > max
            {
                return Some(format!("{} must have at most {max} item(s)", at(path)));
            }
            if let Some(sub) = schema.get("items").filter(|sub| sub.is_object()) {
                for (index, item) in items.iter().enumerate() {
                    if let Some(error) = validate_at(item, sub, &format!("{path}[{index}]")) {
                        return Some(error);
                    }
                }
            }
            None
        }
        Value::Number(number) => {
            let number = number.as_f64()?;
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && number < min
            {
                return Some(format!("{} must be >= {min}", at(path)));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && number > max
            {
                return Some(format!("{} must be <= {max}", at(path)));
            }
            None
        }
        _ => None,
    }
}

/// Repairs and parses raw arguments; an empty string is `{}`.
fn parse_arguments(arguments: &ToolCallArguments) -> Result<Value, String> {
    match arguments {
        ToolCallArguments::Unparsed(raw) if raw.trim().is_empty() => Ok(Value::Object(Map::new())),
        other => other.parse().map_err(|error| error.to_string()),
    }
}

/// Input spellings each native tool accepts besides its schema names (kept
/// out of the schema). Rewritten before validation so a closed schema does
/// not reject them.
fn argument_aliases(tool: &str) -> &'static [(&'static str, &'static str)] {
    match tool {
        "read" | "write" => &[("path", "file_path")],
        "patch" => &[
            ("path", "file_path"),
            ("search", "old_string"),
            ("content", "new_string"),
        ],
        "fs_search" => &crate::FSSearch::ARGUMENT_ALIASES,
        "ast_search" => &[("lang", "language")],
        _ => &[],
    }
}

/// Renames alias keys of `value` (and of patch `edits[]` items) to their
/// schema names. A key whose schema name is also present is left alone, so
/// the validator reports it as unknown.
fn canonicalize_aliases(tool: &str, mut value: Value) -> Value {
    fn rename(map: &mut Map<String, Value>, aliases: &[(&str, &str)]) {
        for (alias, name) in aliases {
            if !map.contains_key(*name)
                && let Some(item) = map.remove(*alias)
            {
                map.insert((*name).to_string(), item);
            }
        }
    }
    if let Value::Object(map) = &mut value {
        rename(map, argument_aliases(tool));
        if tool == "patch"
            && let Some(Value::Array(edits)) = map.get_mut("edits")
        {
            for edit in edits.iter_mut() {
                if let Value::Object(edit) = edit {
                    rename(edit, &[("search", "old_string"), ("content", "new_string")]);
                }
            }
        }
    }
    value
}

/// Normalises one assistant message's tool calls against the tools offered
/// on this request.
///
/// # Arguments
/// * `calls` - The calls in model order.
/// * `surface` - Tool definitions the request offered (name → schema).
/// * `resolve` - Maps a called name to the canonical surface name (case,
///   aliases, MCP name forms); `None` when nothing matches.
/// * `known` - Whether a name is a real tool this agent may not use.
pub fn normalize_batch(
    calls: &[ToolCallFull],
    surface: &[ToolDefinition],
    resolve: impl Fn(&ToolName) -> Option<ToolName>,
    known: impl Fn(&ToolName) -> bool,
) -> NormalizedBatch {
    let names: Vec<&str> = surface.iter().map(|tool| tool.name.as_str()).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut batch = NormalizedBatch::default();
    for call in calls {
        let canonical = resolve(&call.name);
        let definition = canonical
            .as_ref()
            .and_then(|name| surface.iter().find(|tool| &tool.name == name));
        let name = canonical.clone().unwrap_or_else(|| call.name.clone());
        let parsed = parse_arguments(&call.arguments);
        let (arguments, mut rejection) = match parsed {
            Ok(value) => {
                let value = canonicalize_aliases(name.as_str(), value);
                let value = match definition {
                    Some(tool) => coerce_json_containers(value, tool.input_schema.as_value()),
                    None => value,
                };
                (value, None)
            }
            Err(error) => (
                Value::Object(Map::new()),
                Some(CallRejection::InvalidArguments(format!(
                    "could not parse arguments for {name}: {error}"
                ))),
            ),
        };
        let identity = (name.as_str().to_string(), canonical_json(&arguments));
        if rejection.is_none() && !seen.insert(identity) {
            batch.duplicates += 1;
            continue;
        }
        if definition.is_none() {
            rejection = Some(CallRejection::Unavailable(unavailable_message(
                call.name.as_str(),
                &names,
                known(&call.name),
            )));
        }
        if rejection.is_none()
            && let Some(tool) = definition
            && let Some(error) = schema_error(&arguments, tool.input_schema.as_value())
        {
            rejection = Some(CallRejection::InvalidArguments(format!(
                "invalid arguments for {name}: {error}"
            )));
        }
        if batch.calls.len() >= MAX_CALLS_PER_MESSAGE {
            batch.capped += 1;
            continue;
        }
        let arguments = if rejection.is_none() {
            ToolCallArguments::Parsed(arguments)
        } else {
            call.arguments.clone()
        };
        batch.calls.push(NormalizedCall {
            call: ToolCallFull {
                name,
                call_id: call.call_id.clone(),
                arguments,
                thought_signature: call.thought_signature.clone(),
            },
            rejection,
        });
    }
    if batch.capped > 0 {
        batch.notice = Some(format!(
            "Tool-call batch limit reached: retained {} distinct calls and rejected {} beyond the limit of {MAX_CALLS_PER_MESSAGE} per message.",
            batch.calls.len(),
            batch.capped
        ));
    }
    batch
}

/// Compact JSON with sorted object keys, used as a call's identity.
fn canonical_json(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&map[k])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::{ToolCallId, ToolCatalog};

    fn surface() -> Vec<ToolDefinition> {
        ToolCatalog::iter_definitions()
    }

    fn call(name: &str, args: &str, id: &str) -> ToolCallFull {
        ToolCallFull::new(name)
            .call_id(ToolCallId::new(id))
            .arguments(ToolCallArguments::from_json(args))
    }

    fn resolve(name: &ToolName) -> Option<ToolName> {
        let lower = name.as_str().to_lowercase();
        surface()
            .into_iter()
            .map(|tool| tool.name)
            .find(|tool| tool.as_str() == lower)
    }

    #[test]
    fn test_duplicates_dropped_and_order_kept() {
        let fixture = vec![
            call("read", r#"{"file_path":"/a"}"#, "1"),
            call("read", r#"{ "file_path" : "/a" }"#, "2"),
            call("Read", r#"{"file_path":"/b"}"#, "3"),
        ];
        let actual = normalize_batch(&fixture, &surface(), resolve, |_| false);
        let ids: Vec<String> = actual
            .calls
            .iter()
            .map(|c| c.call.call_id.as_ref().unwrap().as_str().to_string())
            .collect();
        assert_eq!(
            (ids, actual.duplicates),
            (vec!["1".to_string(), "3".to_string()], 1)
        );
    }

    #[test]
    fn test_unknown_argument_named_in_one_line() {
        let fixture = vec![call("read", r#"{"file_path":"/a","bogus":1}"#, "1")];
        let actual = normalize_batch(&fixture, &surface(), resolve, |_| false);
        let expected = Some(CallRejection::InvalidArguments(
            "invalid arguments for read: unknown argument bogus; allowed: file_path, range, show_line_numbers".to_string(),
        ));
        assert_eq!(actual.calls[0].rejection, expected);
    }

    #[test]
    fn test_unavailable_tool_suggests_closest() {
        let fixture = vec![
            call("raed", r#"{}"#, "1"),
            call("functions_read", r#"{}"#, "2"),
        ];
        let actual = normalize_batch(&fixture, &surface(), resolve, |_| false);
        let messages: Vec<String> = actual
            .calls
            .iter()
            .map(|c| c.rejection.as_ref().unwrap().message().to_string())
            .collect();
        assert!(
            messages[0].starts_with("unknown tool raed. Did you mean read?"),
            "{}",
            messages[0]
        );
        assert!(
            messages[1].starts_with("unknown tool functions_read. Did you mean read?"),
            "{}",
            messages[1]
        );
    }

    #[test]
    fn test_policy_blocked_is_distinct_from_unknown() {
        let actual = unavailable_message("shell", &["read"], true);
        let expected = "shell is not allowed by this agent's tool policy.";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_stringified_edits_coerced_before_validation() {
        let fixture = vec![call(
            "patch",
            r#"{"file_path":"/a","edits":"[{\"old_string\":\"x\",\"new_string\":\"y\"}]"}"#,
            "1",
        )];
        let actual = normalize_batch(&fixture, &surface(), resolve, |_| false);
        assert_eq!(actual.calls[0].rejection, None);
        let ToolCallArguments::Parsed(args) = &actual.calls[0].call.arguments else {
            panic!("expected parsed arguments")
        };
        assert_eq!(
            args["edits"],
            json!([{"old_string": "x", "new_string": "y"}])
        );
    }

    #[test]
    fn test_cap_drops_calls_beyond_limit() {
        let fixture: Vec<ToolCallFull> = (0..55)
            .map(|i| {
                call(
                    "read",
                    &format!(r#"{{"file_path":"/f{i}"}}"#),
                    &i.to_string(),
                )
            })
            .collect();
        let actual = normalize_batch(&fixture, &surface(), resolve, |_| false);
        assert_eq!((actual.calls.len(), actual.capped), (50, 5));
        assert!(
            actual
                .notice
                .unwrap()
                .contains("rejected 5 beyond the limit of 50")
        );
    }

    #[test]
    fn test_truncated_arguments_drop_all_calls() {
        let fixture = vec![
            call("read", r#"{"file_path":"/a"}"#, "1"),
            call("patch", r#"{"file_path":"/a","old_str"#, "2"),
        ];
        let actual = truncated_call_notice(Some(&FinishReason::Length), &fixture);
        let expected = Some("Response incomplete (length); dropped truncated tool call arguments for patch, read. Retry the request or ask for the change in smaller chunks.".to_string());
        assert_eq!(actual, expected);
        assert_eq!(
            truncated_call_notice(Some(&FinishReason::Stop), &fixture),
            None
        );
    }

    #[test]
    fn test_schema_error_messages() {
        let schema = json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "minimum": 1}, "mode": {"type": "string", "enum": ["a", "b"]}},
            "required": ["n"],
            "additionalProperties": false
        });
        let actual = (
            schema_error(&json!({}), &schema),
            schema_error(&json!({"n": "x"}), &schema),
            schema_error(&json!({"n": 0}), &schema),
            schema_error(&json!({"n": 2, "mode": "c"}), &schema),
            schema_error(&json!({"n": 2, "mode": "a"}), &schema),
        );
        let expected = (
            Some("missing required argument: n".to_string()),
            Some("`n` must be integer, got string".to_string()),
            Some("`n` must be >= 1".to_string()),
            Some("`mode` must be one of \"a\", \"b\"".to_string()),
            None,
        );
        assert_eq!(actual, expected);
    }
}
