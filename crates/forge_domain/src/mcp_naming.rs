//! MCP tool naming (M1) and secret redaction (M5).

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::McpServerConfig;

/// Longest tool name providers accept.
pub const MAX_TOOL_NAME: usize = 64;

/// A model-safe name component: lowercase, `-` and every other character
/// outside `[a-z0-9_]` become `_`, runs collapse, ends trimmed.
pub fn normalize_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(3).map(|b| format!("{b:02x}")).collect()
}

/// `{server}__{tool}`, trimmed to [`MAX_TOOL_NAME`] with a 6-hex hash
/// suffix so two long remote names cannot collapse onto one public name.
pub fn public_tool_name(server: &str, tool: &str) -> String {
    let server = normalize_component(server);
    let tool = normalize_component(tool);
    let public = format!("{server}__{tool}");
    if public.len() <= MAX_TOOL_NAME {
        return public;
    }
    let digest = short_hash(&public);
    let prefix: String = server.chars().take((MAX_TOOL_NAME / 3).max(1)).collect();
    let room = MAX_TOOL_NAME
        .saturating_sub(prefix.len() + digest.len() + 3)
        .max(1);
    let component: String = tool.chars().take(room).collect();
    format!("{prefix}__{component}_{digest}")
}

/// Public names for every server's tools, disambiguating collisions.
///
/// # Arguments
/// * `servers` - Server name → remote tool names, in a stable order.
/// * `reserved` - Names that may not be used (native tools, aliases,
///   `tool_discovery`).
///
/// Returns `(server, remote) → public name` and one warning per collision
/// that was renamed or tool that was withheld.
pub fn assign_public_names(
    servers: &BTreeMap<String, Vec<String>>,
    reserved: &BTreeSet<String>,
) -> (BTreeMap<(String, String), String>, Vec<String>) {
    let mut taken: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut names = BTreeMap::new();
    let mut warnings = Vec::new();
    for (server, tools) in servers {
        for remote in tools {
            let base = public_tool_name(server, remote);
            if reserved.contains(&base) {
                warnings.push(format!(
                    "MCP tool {server}/{remote} is withheld: {base} is a reserved tool name"
                ));
                continue;
            }
            let mut public = base.clone();
            if let Some((other_server, other_remote)) = taken.get(&public) {
                let suffix = short_hash(&format!("{server}\u{0}{remote}"));
                let stem: String = base
                    .chars()
                    .take(MAX_TOOL_NAME.saturating_sub(suffix.len() + 1))
                    .collect();
                public = format!("{stem}_{suffix}");
                warnings.push(format!(
                    "MCP tools {other_server}/{other_remote} and {server}/{remote} both normalize to {base}; the second is published as {public}"
                ));
            }
            taken.insert(public.clone(), (server.clone(), remote.clone()));
            names.insert((server.clone(), remote.clone()), public);
        }
    }
    (names, warnings)
}

/// Values in a server's configuration that must never reach the model:
/// env values, header values (and the credential part of `Bearer x`-style
/// headers), URL query values, and command-line credentials (the value after
/// a bare flag, the value side of `--flag=value`, and long positional
/// arguments). Longest first, so a value redacts before its fragments.
pub fn secret_values(server: &McpServerConfig) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    let auth_schemes = ["apikey", "basic", "bearer", "digest", "token"];
    let push_credential = |value: &str, values: &mut Vec<String>| {
        if value.is_empty() {
            return;
        }
        values.push(value.to_string());
        if let Some((scheme, credential)) = value.trim().split_once(' ')
            && auth_schemes.contains(&scheme.to_lowercase().as_str())
            && !credential.is_empty()
        {
            values.push(credential.to_string());
        }
    };
    match server {
        McpServerConfig::Stdio(stdio) => {
            for value in stdio.env.values() {
                push_credential(value, &mut values);
            }
            let mut after_flag = false;
            for arg in &stdio.args {
                if arg.starts_with('-') {
                    match arg.split_once('=') {
                        Some((_, inline)) if !inline.is_empty() => {
                            values.push(inline.to_string());
                            after_flag = false;
                        }
                        _ => after_flag = true,
                    }
                } else {
                    if after_flag || arg.len() >= 16 {
                        values.push(arg.clone());
                    }
                    after_flag = false;
                }
            }
        }
        McpServerConfig::Http(http) => {
            for value in http.headers.values() {
                push_credential(value, &mut values);
            }
            if let Some((_, query)) = http.url.split_once('?') {
                for pair in query.split('&') {
                    if let Some((_, value)) = pair.split_once('=')
                        && !value.is_empty()
                    {
                        values.push(value.to_string());
                    }
                }
            }
        }
    }
    values.retain(|v| v.len() >= 4);
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    values
}

/// Replaces every occurrence of a secret in `text` with `[REDACTED]`.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{McpHttpServer, McpStdioServer};

    #[test]
    fn test_public_name_normalizes_and_bounds_length() {
        let short = public_tool_name("GitHub-Enterprise", "list-Issues");
        let long = public_tool_name("server", &"x".repeat(100));
        assert_eq!(short, "github_enterprise__list_issues");
        assert_eq!(long.len(), MAX_TOOL_NAME);
        assert!(long.starts_with("server__xxx"), "{long}");
    }

    #[test]
    fn test_collisions_are_disambiguated_and_reserved_withheld() {
        let servers: BTreeMap<String, Vec<String>> = [
            (
                "a".to_string(),
                vec!["get-x".to_string(), "get_x".to_string()],
            ),
            ("read".to_string(), vec![]),
        ]
        .into();
        let reserved: BTreeSet<String> = ["a__reserved".to_string()].into();
        let (names, warnings) = assign_public_names(&servers, &reserved);
        let first = &names[&("a".to_string(), "get-x".to_string())];
        let second = &names[&("a".to_string(), "get_x".to_string())];
        assert_eq!(first, "a__get_x");
        assert!(
            second.starts_with("a__get_x_") && second != first,
            "{second}"
        );
        assert_eq!(warnings.len(), 1);

        let servers: BTreeMap<String, Vec<String>> =
            [("a".to_string(), vec!["reserved".to_string()])].into();
        let (names, warnings) = assign_public_names(&servers, &reserved);
        assert!(names.is_empty());
        assert!(warnings[0].contains("withheld"), "{}", warnings[0]);
    }

    #[test]
    fn test_secrets_from_env_headers_args_and_query_are_redacted() {
        let stdio = McpServerConfig::Stdio(McpStdioServer {
            command: "srv".to_string(),
            args: vec![
                "--token".to_string(),
                "abcd1234".to_string(),
                "--key=zz99yy88".to_string(),
            ],
            env: [("API_KEY".to_string(), "sk-live-123456".to_string())].into(),
            ..Default::default()
        });
        let http = McpServerConfig::Http(McpHttpServer {
            url: "https://x.test/mcp?token=q1w2e3r4".to_string(),
            headers: [("Authorization".to_string(), "Bearer tok-777".to_string())].into(),
            ..Default::default()
        });
        let text = "env sk-live-123456 arg abcd1234 inline zz99yy88 query q1w2e3r4 bearer tok-777";
        let actual = redact(&redact(text, &secret_values(&stdio)), &secret_values(&http));
        let expected =
            "env [REDACTED] arg [REDACTED] inline [REDACTED] query [REDACTED] bearer [REDACTED]";
        assert_eq!(actual, expected);
    }
}
