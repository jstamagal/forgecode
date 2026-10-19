use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use bstr::ByteSlice;
use forge_app::{FetchRequest, NetFetchService, is_binary_content_type};
use forge_domain::Image;
use reqwest::{Client, Url};

/// Most bytes of an unsaved response read into memory; `save` streams larger.
const INLINE_READ_LIMIT: usize = 32 * 1024 * 1024;
/// Bound on a whole unsaved request, including a body that keeps trickling.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a `save` download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// What a `fetch` tool call returns.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchOutcome {
    /// Model-facing text.
    Text(String),
    /// An image for a model with image input.
    Image(Image),
}

/// Retrieves content from URLs (T10).
///
/// HTML becomes markdown with absolute links unless `raw`; `save` streams the
/// body to a file with no size limit and resumes a partial download with a
/// Range request; `file://` URLs read local files; binary responses come back
/// as a short descriptor and images as image content when the model accepts
/// them. A GET without `save` honours robots.txt.
#[derive(Debug)]
pub struct ForgeFetch {
    client: Client,
}

impl Default for ForgeFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl ForgeFetch {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(concat!("forge/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

/// Media type without parameters, lower-cased.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// Whether bytes with no declared type look like binary data.
fn looks_binary(data: &[u8]) -> bool {
    let sample = &data[..data.len().min(4096)];
    if sample.contains(&0) || std::str::from_utf8(sample).is_err() {
        return true;
    }
    let controls = sample
        .iter()
        .filter(|b| **b < 32 && !matches!(**b, 9 | 10 | 12 | 13))
        .count();
    !sample.is_empty() && controls * 10 > sample.len() * 3
}

fn is_text(content_type: &str, data: &[u8]) -> bool {
    if media_type(content_type).is_empty() {
        return !looks_binary(data);
    }
    !is_binary_content_type(content_type)
}

fn image_mime(content_type: &str, data: &[u8]) -> Option<String> {
    let media = media_type(content_type);
    if media.starts_with("image/") && media != "image/svg+xml" {
        return Some(media);
    }
    infer::get(data)
        .map(|kind| kind.mime_type().to_string())
        .filter(|mime| mime.starts_with("image/"))
}

/// HTML to markdown with links made absolute against `base`.
fn html_to_markdown(html: &str, base: &Url) -> String {
    let markdown = html2md::parse_html(html);
    absolutize_links(&markdown, base)
}

/// Rewrites relative `](target)` markdown link targets to absolute URLs.
fn absolutize_links(markdown: &str, base: &Url) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(at) = rest.find("](") {
        out.push_str(&rest[..at + 2]);
        rest = &rest[at + 2..];
        let end = rest.find(')').unwrap_or(rest.len());
        let target = &rest[..end];
        let (link, title) = match target.split_once(' ') {
            Some((link, title)) => (link, Some(title)),
            None => (target, None),
        };
        let absolute = if link.is_empty() || link.starts_with('#') || Url::parse(link).is_ok() {
            link.to_string()
        } else {
            base.join(link)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| link.to_string())
        };
        out.push_str(&absolute);
        if let Some(title) = title {
            out.push(' ');
            out.push_str(title);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn descriptor(kind: &str, content_type: &str, size: usize) -> String {
    format!(
        "{kind} content-type={} size={size} bytes; pass save=<path> to download it",
        if content_type.is_empty() {
            "unknown"
        } else {
            content_type
        }
    )
}

impl ForgeFetch {
    async fn check_robots_txt(&self, url: &Url) -> anyhow::Result<()> {
        let robots_url = format!("{}://{}/robots.txt", url.scheme(), url.authority());
        let robots_response = self
            .client
            .get(&robots_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        if let Ok(robots) = robots_response
            && robots.status().is_success()
        {
            let robots_content = robots.text().await.unwrap_or_default();
            let path = url.path();
            for line in robots_content.lines() {
                if let Some(disallowed) = line.strip_prefix("Disallow: ") {
                    let disallowed = disallowed.trim();
                    let disallowed = if !disallowed.starts_with('/') {
                        format!("/{disallowed}")
                    } else {
                        disallowed.to_string()
                    };
                    let path = if !path.starts_with('/') {
                        format!("/{path}")
                    } else {
                        path.to_string()
                    };
                    if path.starts_with(&disallowed) {
                        return Err(anyhow!(
                            "URL {url} cannot be fetched due to robots.txt restrictions"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Streams `response` into `target`, resuming a partial file when the
    /// server honoured a Range request.
    async fn stream_to(
        &self,
        mut response: reqwest::Response,
        target: &Path,
        resume_from: u64,
    ) -> anyhow::Result<u64> {
        use tokio::io::AsyncWriteExt;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = partial_path(target);
        let resumed = resume_from > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&partial)
            .await?;
        let mut written = if resumed { resume_from } else { 0 };
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&partial, target).await?;
        Ok(written)
    }

    async fn fetch_file_url(
        &self,
        url: &Url,
        request: &FetchRequest,
    ) -> anyhow::Result<FetchOutcome> {
        if request.method != "GET" {
            bail!("file:// fetch only supports GET");
        }
        if let Some(host) = url.host_str()
            && !matches!(host, "" | "localhost" | "127.0.0.1")
        {
            bail!("unsupported file:// host {host:?}");
        }
        let path = url
            .to_file_path()
            .map_err(|_| anyhow!("invalid file:// URL {url}"))?;
        // X20: a file:// read follows the jail's path rules.
        if let Some(jail) = forge_domain::active_jail() {
            jail.confine(&path, false)
                .map_err(|e| anyhow::Error::new(forge_domain::ToolRefusal(e.to_string())))?;
        }
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("no such file: {}", path.display()))?;
        if !meta.is_file() {
            bail!("not a regular file: {}", path.display());
        }
        if let Some(target) = &request.save {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let size = std::fs::copy(&path, target)?;
            return Ok(FetchOutcome::Text(format!(
                "SAVED_RESPONSE path={} size={size} bytes",
                target.display()
            )));
        }
        if meta.len() as usize > INLINE_READ_LIMIT {
            bail!("file exceeds {INLINE_READ_LIMIT} byte read limit; use save= or read");
        }
        let data = std::fs::read(&path)?;
        let content_type = if looks_binary(&data) {
            "application/octet-stream"
        } else {
            "text/plain"
        };
        Ok(format_payload(&data, content_type, request, url))
    }
}

fn partial_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.forge-partial"))
}

/// Text, descriptor or image for an inline response body.
fn format_payload(
    data: &[u8],
    content_type: &str,
    request: &FetchRequest,
    base: &Url,
) -> FetchOutcome {
    if let Some(mime) = image_mime(content_type, data) {
        if request.vision {
            return FetchOutcome::Image(Image::new_bytes(data.to_vec(), mime));
        }
        return FetchOutcome::Text(descriptor("IMAGE_RESPONSE", &mime, data.len()));
    }
    if !is_text(content_type, data) {
        return FetchOutcome::Text(descriptor("BINARY_RESPONSE", content_type, data.len()));
    }
    let text = data.to_str_lossy().into_owned();
    let is_html = media_type(content_type).contains("html")
        || (media_type(content_type).is_empty() && text.trim_start().starts_with("<"));
    if is_html && !request.raw {
        FetchOutcome::Text(html_to_markdown(&text, base))
    } else {
        FetchOutcome::Text(text)
    }
}

#[async_trait::async_trait]
impl NetFetchService for ForgeFetch {
    async fn fetch_tool(&self, request: FetchRequest) -> anyhow::Result<forge_domain::ToolOutput> {
        let outcome = self.fetch_outcome(&request).await?;
        Ok(match outcome {
            FetchOutcome::Text(text) => forge_domain::ToolOutput::text(text),
            FetchOutcome::Image(image) => forge_domain::ToolOutput::image(image),
        })
    }
}

impl ForgeFetch {
    async fn fetch_outcome(&self, request: &FetchRequest) -> anyhow::Result<FetchOutcome> {
        let url = Url::parse(&request.url)
            .with_context(|| format!("Failed to parse URL: {}", request.url))?;
        if url.scheme() == "file" {
            return self.fetch_file_url(&url, request).await;
        }
        if !matches!(url.scheme(), "http" | "https") {
            bail!("unsupported URL scheme {:?}", url.scheme());
        }
        if request.body.is_some() && request.json_body.is_some() {
            bail!("pass either body or json_body, not both");
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| anyhow!("invalid HTTP method {:?}", request.method))?;
        if method == reqwest::Method::GET && request.save.is_none() {
            self.check_robots_txt(&url).await?;
        }

        let mut builder = self.client.request(method, url.clone());
        for (name, value) in &request.headers {
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                bail!("headers must not contain newlines");
            }
            builder = builder.header(name, value);
        }
        if let Some(json) = &request.json_body {
            builder = builder.json(json);
        } else if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        if let Some(target) = &request.save {
            let partial = partial_path(target);
            let resume_from = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
            if resume_from > 0 {
                builder = builder.header("Range", format!("bytes={resume_from}-"));
            }
            let response = builder
                .timeout(DOWNLOAD_TIMEOUT)
                .send()
                .await
                .map_err(|e| anyhow!("Failed to fetch URL {url}: {e}"))?;
            if !response.status().is_success() {
                bail!("Failed to fetch {url} - status code {}", response.status());
            }
            let content_type = header(&response, "content-type");
            let written = self.stream_to(response, target, resume_from).await?;
            return Ok(FetchOutcome::Text(format!(
                "SAVED_RESPONSE path={} size={written} bytes content-type={}",
                target.display(),
                if content_type.is_empty() {
                    "unknown"
                } else {
                    &content_type
                }
            )));
        }

        let mut response = builder
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to fetch URL {url}: {e}"))?;
        let status = response.status();
        let content_type = header(&response, "content-type");
        let final_url = response.url().clone();
        let mut data: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() > INLINE_READ_LIMIT {
                bail!(
                    "response exceeds the {INLINE_READ_LIMIT} byte inline read limit; use save= to stream it to disk"
                );
            }
        }
        if !status.is_success() {
            let body: String = data.to_str_lossy().chars().take(2000).collect();
            bail!("{url} returned status {status}\n{body}");
        }
        Ok(format_payload(&data, &content_type, request, &final_url))
    }
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn test_is_binary_content_type_text_types_are_not_binary() {
        assert!(!is_binary_content_type("text/html"));
        assert!(!is_binary_content_type("text/plain"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("")); // empty = unknown, allow
    }

    #[test]
    fn test_is_binary_content_type_binary_types_detected() {
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("image/png"));
    }

    #[test]
    fn test_absolutize_links() {
        let base = Url::parse("https://example.com/docs/page.html").unwrap();
        let actual = absolutize_links(
            "[a](other.html) [b](/root) [c](https://x.y/z) [d](#frag)",
            &base,
        );
        let expected = "[a](https://example.com/docs/other.html) [b](https://example.com/root) [c](https://x.y/z) [d](#frag)";
        assert_eq!(actual, expected);
    }

    fn request(url: &str) -> FetchRequest {
        FetchRequest {
            url: url.to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            json_body: None,
            raw: false,
            save: None,
            vision: false,
        }
    }

    /// A one-shot HTTP server on localhost; returns its base URL and the
    /// request it received.
    async fn serve(responses: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16384];
                let n = socket.read(&mut buf).await.unwrap();
                seen.push(buf[..n].to_str_lossy().into_owned());
                socket.write_all(&response).await.unwrap();
                socket.shutdown().await.ok();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    fn http(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn test_html_page_becomes_markdown_with_absolute_links() {
        let page = b"<html><body><h1>Title</h1><a href=\"/next\">next</a></body></html>";
        let (base, server) = serve(vec![
            http("404 Not Found", "text/plain", b""),
            http("200 OK", "text/html", page),
        ])
        .await;

        let actual = ForgeFetch::new()
            .fetch_outcome(&request(&format!("{base}/page")))
            .await
            .unwrap();

        let FetchOutcome::Text(text) = actual else {
            panic!("expected text")
        };
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains(&format!("({base}/next)")), "{text}");
        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("GET /robots.txt"), "{}", seen[0]);
    }

    #[tokio::test]
    async fn test_post_json_body_skips_robots_and_sends_body() {
        let (base, server) =
            serve(vec![http("200 OK", "application/json", b"{\"ok\":true}")]).await;
        let mut fixture = request(&format!("{base}/api"));
        fixture.method = "POST".to_string();
        fixture.json_body = Some(serde_json::json!({"q": 1}));

        let actual = ForgeFetch::new().fetch_outcome(&fixture).await.unwrap();

        assert_eq!(actual, FetchOutcome::Text("{\"ok\":true}".to_string()));
        let seen = server.await.unwrap();
        assert!(seen[0].starts_with("POST /api"), "{}", seen[0]);
        assert!(seen[0].contains("application/json"), "{}", seen[0]);
        assert!(seen[0].ends_with("{\"q\":1}"), "{}", seen[0]);
    }

    #[tokio::test]
    async fn test_binary_response_is_a_descriptor() {
        let (base, _server) = serve(vec![
            http("404 Not Found", "text/plain", b""),
            http("200 OK", "application/octet-stream", &[0u8, 1, 2, 3]),
        ])
        .await;

        let actual = ForgeFetch::new()
            .fetch_outcome(&request(&format!("{base}/blob")))
            .await
            .unwrap();

        let expected = FetchOutcome::Text(
            "BINARY_RESPONSE content-type=application/octet-stream size=4 bytes; pass save=<path> to download it".to_string(),
        );
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_save_streams_to_file_and_resumes_with_range() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        std::fs::write(partial_path(&target), b"hello ").unwrap();
        let (base, server) = serve(vec![http(
            "206 Partial Content",
            "application/octet-stream",
            b"world",
        )])
        .await;
        let mut fixture = request(&format!("{base}/file"));
        fixture.save = Some(target.clone());

        let actual = ForgeFetch::new().fetch_outcome(&fixture).await.unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
        let FetchOutcome::Text(text) = actual else {
            panic!("expected text")
        };
        assert!(
            text.starts_with(&format!(
                "SAVED_RESPONSE path={} size=11 bytes",
                target.display()
            )),
            "{text}"
        );
        let seen = server.await.unwrap();
        assert!(
            seen[0].to_lowercase().contains("range: bytes=6-"),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn test_file_url_reads_local_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "local text").unwrap();
        let url = Url::from_file_path(&file).unwrap();

        let actual = ForgeFetch::new()
            .fetch_outcome(&request(url.as_str()))
            .await
            .unwrap();

        assert_eq!(actual, FetchOutcome::Text("local text".to_string()));
    }
}
