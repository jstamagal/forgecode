//! Engines of the phase 7 tools that need no long-lived state: web and docs
//! search (T11, T12), browse (T13), browser_probe (T14), notebook_edit (T19)
//! and the wiki helpers (T25).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

/// Base URLs of the HTTP services (overridable for tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub serper: String,
    pub tavily: String,
    pub exa: String,
    pub context7: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            serper: "https://google.serper.dev/search".to_string(),
            tavily: "https://api.tavily.com/search".to_string(),
            exa: "https://api.exa.ai/search".to_string(),
            context7: "https://context7.com/api/v1".to_string(),
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("forge/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

/// Redacts `secret` from `text`.
fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[REDACTED]")
    }
}

async fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: Value,
    secret: &str,
) -> anyhow::Result<Value> {
    let mut request = client().post(url).json(&body);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = request
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(redact(&format!("request failed: {e}"), secret)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        bail!("{}", redact(&format!("HTTP {status}: {snippet}"), secret));
    }
    serde_json::from_str(&text).context("the search API returned non-JSON")
}

fn numbered(results: &Value, url_key: &str, body_key: &str) -> anyhow::Result<String> {
    let Some(items) = results.as_array() else {
        if results.is_null() {
            return Ok("no results".to_string());
        }
        bail!("expected search results to be an array");
    };
    let lines: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let title = hit["title"].as_str().unwrap_or_default().trim();
            let url = hit[url_key].as_str().unwrap_or_default().trim();
            let body = hit[body_key].as_str().unwrap_or_default().trim();
            if body.is_empty() {
                format!("{}. {title}\n{url}", i + 1)
            } else {
                format!("{}. {title}\n{url}\n{body}", i + 1)
            }
        })
        .collect();
    Ok(if lines.is_empty() {
        "no results".to_string()
    } else {
        lines.join("\n\n")
    })
}

/// T11 Serper.
///
/// # Errors
/// Returns an error for a missing key, an HTTP failure or a bad reply.
pub async fn serper(
    endpoints: &Endpoints,
    key: Option<String>,
    query: &str,
    num: u32,
) -> anyhow::Result<String> {
    let key = key.context("SERPER_API_KEY is not set")?;
    let reply = post_json(
        &endpoints.serper,
        &[("X-API-KEY", &key)],
        json!({"q": query, "num": num.clamp(1, 100)}),
        &key,
    )
    .await?;
    let mut parts = Vec::new();
    let answer = reply["answerBox"]["answer"]
        .as_str()
        .or_else(|| reply["answerBox"]["snippet"].as_str());
    if let Some(answer) = answer.filter(|a| !a.trim().is_empty()) {
        parts.push(format!("answer box: {}", answer.trim()));
    }
    parts.push(numbered(&reply["organic"], "link", "snippet")?);
    Ok(parts.join("\n\n"))
}

/// T11 Tavily.
///
/// # Errors
/// Returns an error for a missing key, an HTTP failure or a bad reply.
pub async fn tavily(
    endpoints: &Endpoints,
    key: Option<String>,
    query: &str,
    max: u32,
) -> anyhow::Result<String> {
    let key = key.context("TAVILY_API_KEY is not set")?;
    let auth = format!("Bearer {key}");
    let reply = post_json(
        &endpoints.tavily,
        &[("Authorization", &auth)],
        json!({"query": query, "max_results": max.clamp(1, 20), "include_answer": true}),
        &key,
    )
    .await?;
    let mut parts = Vec::new();
    if let Some(answer) = reply["answer"].as_str().filter(|a| !a.trim().is_empty()) {
        parts.push(format!("answer: {}", answer.trim()));
    }
    parts.push(numbered(&reply["results"], "url", "content")?);
    Ok(parts.join("\n\n"))
}

/// T11 Exa.
///
/// # Errors
/// Returns an error for a missing key, an HTTP failure or a bad reply.
pub async fn exa(
    endpoints: &Endpoints,
    key: Option<String>,
    query: &str,
    num: u32,
    chars: u32,
) -> anyhow::Result<String> {
    let key = key.context("EXA_API_KEY is not set")?;
    let reply = post_json(
        &endpoints.exa,
        &[("x-api-key", &key)],
        json!({"query": query, "numResults": num.clamp(1, 100), "type": "auto", "contents": {"text": {"maxCharacters": chars.max(100)}}}),
        &key,
    )
    .await?;
    numbered(&reply["results"], "url", "text")
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Context7 plausibility: the requested name (separators ignored) is the
/// hit's project name or one whole word of it.
pub fn context7_plausible(library: &str, hit: &Value) -> bool {
    let wanted = words(library).concat();
    if wanted.is_empty() {
        return false;
    }
    let id = hit["id"].as_str().unwrap_or_default().trim_matches('/');
    let mut names = std::collections::BTreeSet::new();
    for form in id
        .split('/')
        .chain(std::iter::once(hit["title"].as_str().unwrap_or_default()))
    {
        let w = words(form);
        names.insert(w.concat());
        names.extend(w);
    }
    names.contains(&wanted)
}

/// T12 Context7.
///
/// # Errors
/// Returns an error for an HTTP failure or empty docs.
pub async fn docs(
    endpoints: &Endpoints,
    key: Option<String>,
    library: &str,
    topic: Option<&str>,
    tokens: u32,
) -> anyhow::Result<String> {
    let secret = key.clone().unwrap_or_default();
    let get = |url: String| {
        let key = key.clone();
        async move {
            let mut request = client().get(&url);
            if let Some(key) = &key {
                request = request.bearer_auth(key);
            }
            let mut response = request.send().await?;
            if key.is_some() && matches!(response.status().as_u16(), 401 | 403) {
                response = client().get(&url).send().await?;
            }
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!(
                    "HTTP {status}: {}",
                    text.chars().take(300).collect::<String>()
                );
            }
            Ok::<String, anyhow::Error>(text)
        }
    };
    let search_url = format!(
        "{}/search?query={}",
        endpoints.context7,
        url::form_urlencoded::byte_serialize(library.as_bytes()).collect::<String>()
    );
    let found: Value = serde_json::from_str(
        &get(search_url)
            .await
            .map_err(|e| anyhow::anyhow!(redact(&e.to_string(), &secret)))?,
    )
    .context("context7 search returned non-JSON")?;
    let rank = |hit: &Value| {
        (
            hit["score"].as_f64().unwrap_or(0.0),
            hit["verified"].as_bool().unwrap_or(false) as u8,
            hit["trustScore"].as_f64().unwrap_or(0.0),
        )
    };
    let best = found["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hit| context7_plausible(library, hit))
        .max_by(|a, b| {
            rank(a)
                .partial_cmp(&rank(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(id) = best
        .and_then(|hit| hit["id"].as_str())
        .filter(|id| !id.is_empty())
    else {
        return Ok(format!("no library on context7 matches {library:?}"));
    };
    let mut query = vec![
        ("type", "txt".to_string()),
        ("tokens", tokens.max(500).to_string()),
    ];
    if let Some(topic) = topic.filter(|t| !t.trim().is_empty()) {
        query.push(("topic", topic.trim().to_string()));
    }
    let qs: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(query)
        .finish();
    let text = get(format!("{}{id}?{qs}", endpoints.context7))
        .await
        .map_err(|e| anyhow::anyhow!(redact(&e.to_string(), &secret)))?;
    if text.trim().is_empty() {
        bail!("context7 returned empty docs for {id}");
    }
    let header = match topic.filter(|t| !t.trim().is_empty()) {
        Some(topic) => format!("[context7 {id}, topic: {topic}]"),
        None => format!("[context7 {id}]"),
    };
    Ok(format!("{header}\n{text}"))
}

const BROWSE_DUMPS: [&str; 7] = [
    "markdown", "text", "html", "links", "original", "assets", "cookies",
];

fn private_literal(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        })
}

/// Rewrites relative markdown link targets against `base`.
pub fn absolutize(markdown: &str, base: &str) -> String {
    let Ok(base) = url::Url::parse(base) else {
        return markdown.to_string();
    };
    let re =
        regex::Regex::new(r#"(!?\[[^\]]*\])\(([^)\s]+)((?:\s+"[^"]*")?)\)"#).expect("valid regex");
    re.replace_all(markdown, |c: &regex::Captures| {
        let target = &c[2];
        if target.starts_with('#')
            || target.contains("://")
            || ["mailto:", "tel:", "data:"]
                .iter()
                .any(|p| target.starts_with(p))
        {
            return c[0].to_string();
        }
        match base.join(target) {
            Ok(abs) => format!("{}({}{})", &c[1], abs, &c[3]),
            Err(_) => c[0].to_string(),
        }
    })
    .into_owned()
}

/// Runs `argv` (in the jail under `-C`) with an optional stdin, bounded by
/// `timeout`.
async fn run_capped(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
    input: Option<&str>,
) -> anyhow::Result<(i32, String, String)> {
    use tokio::io::AsyncWriteExt;
    let argv = forge_domain::jailed_argv(argv.to_vec(), cwd);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", argv[0]))?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).await?;
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("{} timed out after {}s", argv[0], timeout.as_secs()))??;
    Ok((
        output.status.code().unwrap_or(-1),
        bstr::ByteSlice::to_str_lossy(output.stdout.as_slice()).into_owned(),
        bstr::ByteSlice::to_str_lossy(output.stderr.as_slice()).into_owned(),
    ))
}

/// T13 browse through obscura.
///
/// # Errors
/// Returns an error for a bad dump, a missing binary or an obscura failure.
pub async fn browse(
    obscura: &Path,
    url: &str,
    dump: &str,
    screenshot: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<String> {
    if !BROWSE_DUMPS.contains(&dump) {
        bail!("dump must be one of {}", BROWSE_DUMPS.join(", "));
    }
    let bin = obscura.display().to_string();
    let mut argv = vec![
        bin.clone(),
        "--stealth".to_string(),
        "fetch".to_string(),
        "--dump".to_string(),
        dump.to_string(),
        "--timeout".to_string(),
        "59".to_string(),
    ];
    if dump != "original" {
        argv.extend(["--wait".to_string(), "5".to_string()]);
    }
    if let Some(shot) = screenshot {
        if let Some(parent) = shot.parent() {
            std::fs::create_dir_all(parent)?;
        }
        argv.extend(["--screenshot".to_string(), shot.display().to_string()]);
    }
    if private_literal(url) {
        argv.push("--allow-private-network".to_string());
    }
    argv.push(url.to_string());
    // obscura reports no status on the render path; its batch mode does.
    let status = if screenshot.is_none() {
        let mut probe = vec![bin, "--stealth".to_string()];
        if private_literal(url) {
            probe.push("--allow-private-network".to_string());
        }
        probe.extend(["fetch", "--file", "-", "--timeout", "10"].map(str::to_string));
        run_capped(
            &probe,
            cwd,
            Duration::from_secs(15),
            Some(&format!("{url}\n")),
        )
        .await
        .ok()
        .and_then(|(_, out, _)| {
            out.lines()
                .rev()
                .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
                .find_map(|v| v["status"].as_u64())
        })
    } else {
        None
    };
    let (code, stdout, stderr) = run_capped(&argv, cwd, Duration::from_secs(60), None).await?;
    if code != 0 {
        let tail: String = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!(
            "obscura exited {code}: {}",
            tail.chars()
                .rev()
                .take(300)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
    }
    if let Some(shot) = screenshot {
        let size = std::fs::metadata(shot).map(|m| m.len()).with_context(|| {
            format!(
                "obscura exited 0 but wrote no screenshot at {}",
                shot.display()
            )
        })?;
        return Ok(format!(
            "SCREENSHOT path={} size={size} bytes\nobscura returns either a picture or a dump, never both; call browse again without screenshot to read the page.",
            shot.display()
        ));
    }
    let mut body = if dump == "original" {
        stdout
    } else {
        stdout.trim().to_string()
    };
    if dump == "markdown" {
        body = absolutize(&body, url);
    }
    if body.is_empty() {
        body = "(empty page)".to_string();
    }
    Ok(match status {
        Some(code) if code >= 400 => {
            format!("ERROR: HTTP {code} from {url}; the page below is the error response\n{body}")
        }
        _ => body,
    })
}

const PROBE: &str = include_str!("browser_probe.py");

/// T14 browser_probe through Playwright.
///
/// # Errors
/// Returns an error when python3/playwright are missing or the probe fails.
pub async fn browser_probe(spec: Value, output_dir: &Path, cwd: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(output_dir)?;
    let mut spec = spec;
    spec["output_dir"] = json!(output_dir.display().to_string());
    let argv = vec!["python3".to_string(), "-c".to_string(), PROBE.to_string()];
    let (code, stdout, stderr) = run_capped(
        &argv,
        cwd,
        Duration::from_secs(120),
        Some(&spec.to_string()),
    )
    .await?;
    if code != 0 {
        if stderr.contains("No module named 'playwright'") {
            bail!(
                "browser_probe needs python3 with the playwright package (pip install playwright; playwright install chromium)"
            );
        }
        bail!(
            "browser_probe failed: {}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(stdout.trim().to_string())
}

/// T19: replace, insert or delete one cell. Returns the new notebook bytes
/// and a one-line summary.
///
/// # Errors
/// Returns an error for an unknown cell, a bad mode or a malformed notebook.
pub fn notebook_edit(
    data: &[u8],
    cell_id: Option<&str>,
    new_source: Option<&str>,
    cell_type: Option<&str>,
    mode: &str,
) -> anyhow::Result<(Vec<u8>, String)> {
    let mut nb: Map<String, Value> = serde_json::from_slice(data).context("not a JSON notebook")?;
    let cells = nb
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .context("notebook has no cells array")?;
    let ids: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            c["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("cell-{i}"))
        })
        .collect();
    let index = match cell_id {
        None => None,
        Some(id) => Some(
            match id
                .strip_prefix("index:")
                .and_then(|n| n.parse::<usize>().ok())
            {
                Some(i) if i < cells.len() => i,
                Some(i) => bail!(
                    "cell index {i} is out of range (the notebook has {} cells)",
                    cells.len()
                ),
                None => ids
                    .iter()
                    .position(|x| x == id)
                    .with_context(|| format!("no cell with id {id:?}"))?,
            },
        ),
    };
    let source_lines = |text: &str| -> Value {
        let mut lines: Vec<Value> = text.split_inclusive('\n').map(|l| json!(l)).collect();
        if lines.is_empty() {
            lines = vec![];
        }
        Value::Array(lines)
    };
    let summary = match mode {
        "replace" => {
            let i = index.context("replace needs cell_id")?;
            let source = new_source.context("replace needs new_source")?;
            let cell = &mut cells[i];
            if let Some(kind) = cell_type {
                cell["cell_type"] = json!(kind);
            }
            cell["source"] = source_lines(source);
            if cell["cell_type"] == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            format!(
                "replaced cell {} ({})",
                ids[i],
                cell["cell_type"].as_str().unwrap_or("?")
            )
        }
        "insert" => {
            let kind = cell_type.context("insert needs cell_type (code or markdown)")?;
            if !matches!(kind, "code" | "markdown" | "raw") {
                bail!("cell_type must be code, markdown or raw");
            }
            let source = new_source.unwrap_or_default();
            let at = index.map(|i| i + 1).unwrap_or(0);
            let id = format!(
                "{:08x}",
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
                    & 0xffff_ffff) as u32
            );
            let mut cell = json!({"cell_type": kind, "id": id, "metadata": {}, "source": source_lines(source)});
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            let has_ids = cells.iter().any(|c| c.get("id").is_some());
            if !has_ids {
                cell.as_object_mut().map(|o| o.remove("id"));
            }
            cells.insert(at, cell);
            format!("inserted a {kind} cell at position {at}")
        }
        "delete" => {
            let i = index.context("delete needs cell_id")?;
            cells.remove(i);
            format!("deleted cell {}", ids[i])
        }
        other => bail!("edit_mode must be replace, insert or delete; got {other:?}"),
    };
    // nbformat writes one-space indentation and a trailing newline.
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&Value::Object(nb), &mut ser)?;
    out.push(b'\n');
    Ok((out, summary))
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn vault(path: &Path) -> anyhow::Result<PathBuf> {
    let text = path.display().to_string();
    if text.trim().is_empty() || text == "." {
        bail!("vault must name an existing vault directory");
    }
    if !path.is_dir() {
        bail!("vault {} does not exist", path.display());
    }
    Ok(path.to_path_buf())
}

/// T25 wiki_write.
///
/// # Errors
/// Returns an error for a bad kind, a missing vault or a protected page.
pub fn wiki_write(
    vault_path: &Path,
    kind: &str,
    title: &str,
    body: &str,
    tags: &[String],
    overwrite: bool,
) -> anyhow::Result<String> {
    let vault = vault(vault_path)?;
    let kind = kind.trim().to_lowercase();
    if !matches!(kind.as_str(), "source" | "entity" | "concept" | "synthesis") {
        bail!("kind must be source, entity, concept or synthesis (got {kind:?})");
    }
    let slug = slugify(title);
    if slug.is_empty() {
        bail!("title must contain letters or digits");
    }
    let page = vault.join(format!("{kind}s")).join(format!("{slug}.md"));
    let existed = page.exists();
    if existed && !overwrite {
        bail!(
            "{} exists; pass overwrite=true with a fully merged body to replace it",
            page.display()
        );
    }
    let mut tag_list = vec![kind.clone()];
    tag_list.extend(
        tags.iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty() && *t != kind),
    );
    tag_list.dedup();
    let front = format!(
        "---\ntype: {kind}\ntitle: {}\ntags: {}\ndate_updated: {}\n---\n\n",
        serde_json::to_string(title)?,
        serde_json::to_string(&tag_list)?,
        chrono::Local::now().format("%Y-%m-%d")
    );
    if let Some(parent) = page.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&page, format!("{front}{}\n", body.trim_end()))?;
    Ok(format!(
        "{} {} (type: {kind})",
        if existed { "overwrote" } else { "wrote" },
        page.display()
    ))
}

async fn git(vault: &Path, args: &[&str]) -> (i32, String, String) {
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        vault.display().to_string(),
    ];
    argv.extend(args.iter().map(|a| a.to_string()));
    run_capped(&argv, vault, Duration::from_secs(60), None)
        .await
        .unwrap_or((-1, String::new(), "git failed to start".to_string()))
}

/// T25 wiki_finish_ingest.
///
/// # Errors
/// Returns an error when archiving fails or the commit fails.
pub async fn wiki_finish_ingest(
    vault_path: &Path,
    source: &Path,
    note: &str,
) -> anyhow::Result<String> {
    let vault = vault(vault_path)?;
    let raw = vault.join("raw");
    let relative = source
        .strip_prefix(&raw)
        .with_context(|| format!("source must be under {}", raw.display()))?;
    if relative.starts_with("archive") {
        bail!("{} is already archived", source.display());
    }
    if !source.exists() {
        bail!("no such source: {}", source.display());
    }
    let dest = raw.join("archive").join(relative);
    if dest.exists() {
        bail!("already archived: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(source, &dest)?;
    let title = relative.display().to_string();
    let entry = format!(
        "\n## [{}] ingest | {title}\n{}\n",
        chrono::Local::now().format("%Y-%m-%d"),
        note.trim()
    );
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(vault.join("log.md"))?
        .write_all(entry.as_bytes())?;
    let mut lines = vec![
        format!("archived: raw/{title} -> raw/archive/{title}"),
        format!("logged: {title}"),
    ];
    let (rc, out, _) = git(&vault, &["rev-parse", "--is-inside-work-tree"]).await;
    if rc != 0 || out.trim() != "true" {
        lines.push("git: no repository".to_string());
        return Ok(lines.join("\n"));
    }
    let (_, top, _) = git(&vault, &["rev-parse", "--show-toplevel"]).await;
    let top = PathBuf::from(top.trim());
    if std::fs::canonicalize(&top).ok() != std::fs::canonicalize(&vault).ok() {
        lines.push(format!(
            "git: {} is not the root of its work tree (the repository is {}); not committed",
            vault.display(),
            top.display()
        ));
        return Ok(lines.join("\n"));
    }
    git(&vault, &["add", "-A"]).await;
    let (rc, _, _) = git(&vault, &["diff", "--cached", "--quiet"]).await;
    if rc == 0 {
        lines.push("git: nothing to commit".to_string());
        return Ok(lines.join("\n"));
    }
    let (rc, out, err) = git(&vault, &["commit", "-m", &format!("ingest: {title}")]).await;
    if rc != 0 {
        bail!(
            "{}\ngit commit failed: {}",
            lines.join("\n"),
            (if err.trim().is_empty() { out } else { err }).trim()
        );
    }
    let (_, sha, _) = git(&vault, &["rev-parse", "--short", "HEAD"]).await;
    lines.push(format!("git: committed {}", sha.trim()));
    Ok(lines.join("\n"))
}

/// T25 wiki_convert.
///
/// # Errors
/// Returns an error for a missing file or converter.
pub async fn wiki_convert(
    file: &Path,
    vault_path: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<String> {
    if !file.is_file() {
        bail!("no such file: {}", file.display());
    }
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let media = matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "svg"
            | "mp3"
            | "wav"
            | "ogg"
            | "m4a"
            | "mp4"
            | "mov"
            | "webm"
            | "mkv"
    );
    let image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp");
    if media {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut out = vec![format!("![[{name}]]")];
        if let Some(v) = vault_path {
            let v = vault(v)?;
            let assets = v.join("assets");
            std::fs::create_dir_all(&assets)?;
            std::fs::copy(file, assets.join(&name))?;
            out.push(format!("copied to {}", assets.join(&name).display()));
        }
        if image {
            match run_capped(
                &[
                    "tesseract".to_string(),
                    file.display().to_string(),
                    "-".to_string(),
                ],
                cwd,
                Duration::from_secs(120),
                None,
            )
            .await
            {
                Ok((0, text, _)) if !text.trim().is_empty() => {
                    out.push(format!("OCR:\n{}", text.trim()))
                }
                Ok((0, _, _)) => out.push("OCR: no text found".to_string()),
                _ => out.push("OCR unavailable (tesseract missing or failed)".to_string()),
            }
        }
        return Ok(out.join("\n"));
    }
    match ext.as_str() {
        "pdf" => {
            let (rc, out, err) = run_capped(
                &[
                    "pdftotext".to_string(),
                    "-layout".to_string(),
                    file.display().to_string(),
                    "-".to_string(),
                ],
                cwd,
                Duration::from_secs(120),
                None,
            )
            .await
            .context("PDF conversion needs pdftotext (poppler)")?;
            if rc != 0 {
                bail!("pdftotext failed: {}", err.trim());
            }
            Ok(out)
        }
        "docx" | "odt" | "epub" | "rtf" | "html" | "htm" | "doc" | "pptx" | "xlsx" | "xls"
        | "ods" => {
            let pandoc_ok = !matches!(ext.as_str(), "doc" | "xls" | "xlsx" | "ods" | "pptx");
            if pandoc_ok
                && let Ok((0, out, _)) = run_capped(
                    &[
                        "pandoc".to_string(),
                        file.display().to_string(),
                        "-t".to_string(),
                        "markdown".to_string(),
                    ],
                    cwd,
                    Duration::from_secs(120),
                    None,
                )
                .await
            {
                return Ok(out);
            }
            let tmp = tempfile::tempdir()?;
            let target = if matches!(ext.as_str(), "xls" | "xlsx" | "ods") {
                "csv"
            } else {
                "txt"
            };
            let (rc, _, err) = run_capped(
                &[
                    "soffice".to_string(),
                    "--headless".to_string(),
                    "--convert-to".to_string(),
                    target.to_string(),
                    "--outdir".to_string(),
                    tmp.path().display().to_string(),
                    file.display().to_string(),
                ],
                cwd,
                Duration::from_secs(180),
                None,
            )
            .await
            .with_context(|| {
                format!(
                    "no converter installed for .{ext}: install pandoc or LibreOffice (soffice)"
                )
            })?;
            if rc != 0 {
                bail!("soffice failed: {}", err.trim());
            }
            let converted = std::fs::read_dir(tmp.path())?
                .flatten()
                .next()
                .context("soffice wrote nothing")?;
            Ok(std::fs::read_to_string(converted.path())?)
        }
        _ => {
            let bytes = std::fs::read(file)?;
            match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(text.to_string()),
                Err(_) => bail!("{} is binary and has no converter", file.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn serve(replies: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let n = socket.read(&mut buf).await.unwrap();
                seen.push(bstr::ByteSlice::to_str_lossy(&buf[..n]).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn test_serper_formats_answer_and_results_and_sends_key() {
        let body = json!({"answerBox": {"answer": "42"}, "organic": [{"title": "T", "link": "https://x", "snippet": "S"}]}).to_string();
        let (base, server) = serve(vec![(200, body)]).await;
        let endpoints = Endpoints { serper: format!("{base}/search"), ..Default::default() };

        let actual = serper(&endpoints, Some("k123".to_string()), "q", 8)
            .await
            .unwrap();

        assert_eq!(actual, "answer box: 42\n\n1. T\nhttps://x\nS");
        let seen = server.await.unwrap();
        assert!(
            seen[0].to_lowercase().contains("x-api-key: k123"),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn test_missing_key_and_redacted_http_error() {
        let (base, _server) = serve(vec![(401, "bad key sekrit".to_string())]).await;
        let endpoints = Endpoints { exa: format!("{base}/search"), ..Default::default() };
        let missing = tavily(&Endpoints::default(), None, "q", 5)
            .await
            .unwrap_err()
            .to_string();
        let failed = exa(&endpoints, Some("sekrit".to_string()), "q", 5, 500)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(missing, "TAVILY_API_KEY is not set");
        assert!(
            failed.contains("[REDACTED]") && !failed.contains("sekrit"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn test_docs_search_resolves_plausible_library_only() {
        let search = json!({"results": [
            {"id": "/other/unrelated", "title": "Unrelated", "score": 999},
            {"id": "/tokio-rs/tokio", "title": "Tokio", "score": 10},
        ]})
        .to_string();
        let (base, server) = serve(vec![(200, search), (200, "spawn docs".to_string())]).await;
        let endpoints = Endpoints { context7: base, ..Default::default() };

        let actual = docs(&endpoints, None, "tokio", Some("spawn"), 1000)
            .await
            .unwrap();

        assert_eq!(
            actual,
            "[context7 /tokio-rs/tokio, topic: spawn]\nspawn docs"
        );
        let seen = server.await.unwrap();
        assert!(
            seen[1].starts_with("GET /tokio-rs/tokio?type=txt&tokens=1000&topic=spawn"),
            "{}",
            seen[1]
        );
        assert!(!context7_plausible(
            "react",
            &json!({"id": "/x/preact", "title": "Preact"})
        ));
    }

    #[test]
    fn test_absolutize_keeps_anchors_and_absolute_links() {
        let actual = absolutize(
            "[a](/x) [b](#top) [c](https://y/z) ![i](img.png \"t\")",
            "https://h.test/dir/page",
        );
        assert_eq!(
            actual,
            "[a](https://h.test/x) [b](#top) [c](https://y/z) ![i](https://h.test/dir/img.png \"t\")"
        );
    }

    fn notebook() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "cells": [
                {"cell_type": "code", "id": "a1", "metadata": {}, "source": ["x = 1\n"], "outputs": [{"output_type": "stream", "text": "old"}], "execution_count": 3},
                {"cell_type": "markdown", "id": "b2", "metadata": {}, "source": ["# Title"]},
            ],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        }))
        .unwrap()
    }

    #[test]
    fn test_notebook_replace_clears_outputs_insert_and_delete() {
        let (replaced, summary) = notebook_edit(
            &notebook(),
            Some("a1"),
            Some("x = 2\ny = 3"),
            None,
            "replace",
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&replaced).unwrap();
        assert_eq!(summary, "replaced cell a1 (code)");
        assert_eq!(v["cells"][0]["source"], json!(["x = 2\n", "y = 3"]));
        assert_eq!(
            (
                v["cells"][0]["outputs"].clone(),
                v["cells"][0]["execution_count"].clone()
            ),
            (json!([]), Value::Null)
        );
        assert!(replaced.ends_with(b"}\n") && replaced.starts_with(b"{\n \""));

        let (inserted, _) = notebook_edit(
            &notebook(),
            Some("index:0"),
            Some("print(1)"),
            Some("code"),
            "insert",
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&inserted).unwrap();
        assert_eq!(v["cells"].as_array().unwrap().len(), 3);
        assert_eq!(v["cells"][1]["source"], json!(["print(1)"]));

        let (deleted, _) = notebook_edit(&notebook(), Some("b2"), None, None, "delete").unwrap();
        let v: Value = serde_json::from_slice(&deleted).unwrap();
        assert_eq!(v["cells"].as_array().unwrap().len(), 1);
        assert!(notebook_edit(&notebook(), Some("zz"), Some("x"), None, "replace").is_err());
    }

    #[tokio::test]
    async fn test_wiki_write_protects_pages_and_finish_ingest_commits_root_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("raw")).unwrap();
        std::fs::write(vault.join("raw/note.txt"), "content").unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&vault)
            .status()
            .is_ok_and(|s| s.success());
        if git_ok {
            for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
                std::process::Command::new("git")
                    .args(["config", k, v])
                    .current_dir(&vault)
                    .status()
                    .unwrap();
            }
        }

        let wrote = wiki_write(
            &vault,
            "Concept",
            "Rust Ownership!",
            "Body [[x]]",
            &["Memory".to_string()],
            false,
        )
        .unwrap();
        let refused = wiki_write(&vault, "concept", "Rust Ownership", "B", &[], false)
            .unwrap_err()
            .to_string();
        let page = std::fs::read_to_string(vault.join("concepts/rust-ownership.md")).unwrap();
        let finished =
            wiki_finish_ingest(&vault, &vault.join("raw/note.txt"), "added ownership page")
                .await
                .unwrap();

        assert!(wrote.starts_with("wrote "), "{wrote}");
        assert!(refused.contains("overwrite=true"), "{refused}");
        assert!(
            page.starts_with(
                "---\ntype: concept\ntitle: \"Rust Ownership!\"\ntags: [\"concept\",\"memory\"]\n"
            ),
            "{page}"
        );
        assert!(vault.join("raw/archive/note.txt").exists());
        assert!(
            std::fs::read_to_string(vault.join("log.md"))
                .unwrap()
                .contains("added ownership page")
        );
        if git_ok {
            assert!(finished.contains("git: committed "), "{finished}");
        }
        assert!(wiki_write(Path::new("."), "concept", "x", "y", &[], false).is_err());
    }

    #[tokio::test]
    async fn test_wiki_convert_text_and_media_copy() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("v");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(dir.path().join("a.json"), "{\"k\": 1}").unwrap();
        std::fs::write(dir.path().join("clip.mp3"), b"ID3").unwrap();

        let text = wiki_convert(&dir.path().join("a.json"), None, dir.path())
            .await
            .unwrap();
        let media = wiki_convert(&dir.path().join("clip.mp3"), Some(&vault), dir.path())
            .await
            .unwrap();

        assert_eq!(text, "{\"k\": 1}");
        assert!(media.starts_with("![[clip.mp3]]\ncopied to "), "{media}");
        assert!(vault.join("assets/clip.mp3").exists());
    }

    #[tokio::test]
    async fn test_browser_probe_click_changes_pixels_when_available() {
        let ready = std::process::Command::new("python3")
            .args(["-c", "import playwright"])
            .status()
            .is_ok_and(|s| s.success());
        if !ready {
            eprintln!("skipped: python3 playwright not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("index.html");
        std::fs::write(
            &page,
            "<html><body style=\"background:#fff\"><button onclick=\"document.body.style.background='#f00'\">Go</button></body></html>",
        )
        .unwrap();
        let spec = json!({"target": page.display().to_string(), "click": "go", "settle_ms": 200, "hold_ms": 200, "viewport_width": 200, "viewport_height": 120});

        let report = match browser_probe(spec, &dir.path().join("frames"), dir.path()).await {
            Ok(report) => report,
            Err(error) if error.to_string().contains("Executable doesn't exist") => {
                eprintln!("skipped: no Chromium for playwright");
                return;
            }
            Err(error) => panic!("{error:#}"),
        };
        let report: Value = serde_json::from_str(&report).unwrap();

        assert_eq!(report["clicked"], json!(["go"]));
        assert_eq!(report["frames"].as_array().unwrap().len(), 3);
        assert!(
            report["frames"][1]["changed_pixels_pct"].as_f64().unwrap() > 50.0,
            "{report}"
        );
    }

    #[tokio::test]
    async fn test_browse_with_real_obscura_when_installed() {
        let base = forge_config::ConfigReader::base_path();
        let Ok(resolved) = forge_domain::resolve_binary(
            "obscura",
            &base,
            &std::env::var("PATH").unwrap_or_default(),
        ) else {
            eprintln!("skipped: obscura not installed");
            return;
        };
        let page = "<html><body><h1>Hello</h1><a href=\"/next\">next</a></body></html>";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();

        let actual = browse(
            resolved.path(),
            &format!("http://{addr}/p"),
            "markdown",
            None,
            dir.path(),
        )
        .await
        .unwrap();

        assert!(actual.contains("Hello"), "{actual}");
        assert!(
            actual.contains(&format!("(http://{addr}/next)")),
            "{actual}"
        );
    }
}
