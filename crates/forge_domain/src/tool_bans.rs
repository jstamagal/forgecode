//! X18: argument ban patterns, refused before a call runs.

use serde_json::Value;

/// One ban as the matcher sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ban<'a> {
    /// Argument name to test; `None` or `*` tests every string argument at
    /// any depth.
    pub arg: Option<&'a str>,
    /// Regex searched in the argument's value.
    pub pattern: &'a str,
    /// What the model reads when the ban applies.
    pub reason: Option<&'a str>,
}

fn string_values<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| string_values(item, out)),
        Value::Object(map) => map.values().for_each(|item| string_values(item, out)),
        _ => {}
    }
}

/// The refusal for a call to `tool` whose arguments match one of `bans`, or
/// `None`. An invalid regex never matches ([`invalid_ban`] reports it).
pub fn ban_refusal<'a>(
    tool: &str,
    arguments: &Value,
    bans: impl IntoIterator<Item = Ban<'a>>,
) -> Option<String> {
    for ban in bans {
        let Ok(regex) = regex::Regex::new(ban.pattern) else {
            continue;
        };
        let arg = ban.arg.filter(|arg| *arg != "*");
        let mut values = Vec::new();
        match arg {
            Some(arg) => {
                if let Some(value) = arguments.get(arg) {
                    string_values(value, &mut values);
                }
            }
            None => string_values(arguments, &mut values),
        }
        if values.iter().any(|value| regex.is_match(value)) {
            return Some(match ban.reason {
                Some(reason) => format!("{tool} call refused before running: {reason}"),
                None => format!(
                    "{tool} call refused before running: {} matches ban pattern {:?}.",
                    arg.unwrap_or("an argument"),
                    ban.pattern
                ),
            });
        }
    }
    None
}

/// The error of an invalid ban regex, or `None`.
pub fn invalid_ban(pattern: &str) -> Option<String> {
    regex::Regex::new(pattern)
        .err()
        .map(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    const SHELL: Ban<'static> = Ban {
        arg: Some("command"),
        pattern: r"(?i)\brm\s+-rf\b",
        reason: None,
    };
    const FETCH: Ban<'static> = Ban { arg: None, pattern: r"^https?://internal\.", reason: None };

    #[test]
    fn test_regex_on_named_argument_and_on_any_argument() {
        let actual = (
            ban_refusal("shell", &json!({"command": "cd x && RM -RF /"}), [SHELL]).is_some(),
            ban_refusal("shell", &json!({"command": "rm -r x"}), [SHELL]).is_some(),
            ban_refusal("shell", &json!({"command": "ls", "cwd": "rm -rf"}), [SHELL]).is_some(),
            ban_refusal("fetch", &json!({"url": "https://internal.corp/x"}), [FETCH]).is_some(),
            ban_refusal(
                "fetch",
                &json!({"url": "https://example.com/?u=internal.corp"}),
                [FETCH],
            )
            .is_some(),
        );
        // A named argument is the only one tested.
        assert_eq!(actual, (true, false, false, true, false));
    }

    #[test]
    fn test_message_uses_reason_or_names_the_pattern() {
        let reasoned = Ban { reason: Some("internal hosts are off limits"), ..FETCH };
        let actual = (
            ban_refusal("shell", &json!({"command": "rm -rf /"}), [SHELL]),
            ban_refusal("fetch", &json!({"url": "http://internal.x"}), [reasoned]),
        );
        let expected = (
            Some(format!(
                "shell call refused before running: command matches ban pattern {:?}.",
                SHELL.pattern
            )),
            Some("fetch call refused before running: internal hosts are off limits".to_string()),
        );
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_invalid_regex_is_reported_and_never_matches() {
        let broken = Ban { arg: None, pattern: "(unclosed", reason: None };
        assert!(invalid_ban("(unclosed").is_some());
        assert_eq!(
            ban_refusal("shell", &json!({"command": "(unclosed"}), [broken]),
            None
        );
    }
}
