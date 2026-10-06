use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use bstr::ByteSlice;
use forge_app::{
    CommandInfra, Content, EnvironmentInfra, FileInfoInfra, FileReaderInfra as InfraFsReadService,
    FsReadService, ReadOutput, ReadRequest, compute_hash,
};
use forge_domain::{FileInfo, Image, ReadKey, ToolCallContext};
use serde_json::json;

use crate::range::resolve_range;
use crate::tool_services::notebook;
use crate::tool_services::provenance::{hash_bytes, key};
use crate::utils::assert_absolute_path;

/// Detects the MIME type of a file based on extension and content
fn detect_mime_type(path: &Path, content: &[u8]) -> String {
    // Try infer crate first (checks magic numbers)
    if let Some(file_type) = infer::get(content) {
        return file_type.mime_type().to_string();
    }

    // Fallback to extension-based detection
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext.to_lowercase().as_str() {
            "txt" | "md" | "rs" | "toml" | "yaml" | "yml" | "json" | "js" | "ts" | "py" | "sh" => {
                "text/plain"
            }
            "ipynb" => "application/json",
            "pdf" => "application/pdf",
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "text/plain", // Default to text
        })
        .unwrap_or("text/plain")
        .to_string()
}

/// Checks if a MIME type represents visual content (images or PDFs)
fn is_visual_content(mime_type: &str) -> bool {
    mime_type.starts_with("image/") || mime_type == "application/pdf"
}

/// Validates that file size does not exceed the maximum allowed file size.
///
/// # Arguments
/// * `infra` - The infrastructure instance providing file metadata services
/// * `path` - The file path to check
/// * `max_file_size` - Maximum allowed file size in bytes
///
/// # Returns
/// * `Ok(())` if file size is within limits
/// * `Err(anyhow::Error)` if file exceeds max_file_size
pub async fn assert_file_size<F: FileInfoInfra>(
    infra: &F,
    path: &Path,
    max_file_size: u64,
) -> anyhow::Result<()> {
    let file_size = infra.file_size(path).await?;
    if file_size > max_file_size {
        return Err(anyhow::anyhow!(
            "File size ({file_size} bytes) exceeds the maximum allowed size of {max_file_size} bytes"
        ));
    }
    Ok(())
}

/// Opens `path` for reading without blocking on a FIFO, socket or device and
/// refuses anything that is not a regular file.
fn open_regular(path: &Path) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc_nonblock());
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("not a regular file: {}", path.display());
    }
    Ok(file)
}

#[cfg(unix)]
fn libc_nonblock() -> i32 {
    // O_NONBLOCK: 0o4000 on Linux; 0x0004 on macOS/BSD.
    if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )) {
        0x0004
    } else {
        0o4000
    }
}

/// Reads all bytes of a regular file without blocking on special files.
fn read_regular(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = open_regular(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

/// JSON arguments for a follow-up `read` call.
fn read_call(path: &Path, range: serde_json::Value) -> String {
    json!({"file_path": path.display().to_string(), "range": range}).to_string()
}

/// Moves `index` back to the first byte of the UTF-8 character it lands in.
fn char_start(buf: &[u8], mut index: usize) -> usize {
    let mut back = 0;
    while index > 0 && index < buf.len() && back < 3 && (buf[index] & 0xC0) == 0x80 {
        index -= 1;
        back += 1;
    }
    index
}

/// Lines of `data` (1-based) shown whole by the byte window `start..end`.
fn lines_within(data: &[u8], start: usize, end: usize) -> Option<(u64, u64)> {
    let mut first = None;
    let mut last = None;
    let mut offset = 0usize;
    let mut number = 0u64;
    for line in data.split_inclusive(|b| *b == b'\n') {
        number += 1;
        let line_end = offset + line.len();
        if offset >= start && line_end <= end {
            first.get_or_insert(number);
            last = Some(number);
        }
        offset = line_end;
        if offset >= end {
            break;
        }
    }
    first.zip(last)
}

/// Text file heuristic: no NUL byte in the first 8 KiB and valid UTF-8.
fn looks_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|b| *b == 0)
}

/// Service for reading files.
pub struct ForgeFsRead<F> {
    infra: Arc<F>,
}

impl<F> ForgeFsRead<F> {
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra }
    }
}

impl<
    F: FileInfoInfra
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + InfraFsReadService
        + CommandInfra,
> ForgeFsRead<F>
{
    /// One page of a byte-range read: half of the tighter result cap.
    fn byte_page(config: &forge_config::ForgeConfig) -> usize {
        let caps: Vec<u64> = [
            config.max_tool_result_inline_bytes,
            config.max_tool_result_bytes,
        ]
        .into_iter()
        .filter(|cap| *cap > 0)
        .collect();
        caps.into_iter()
            .min()
            .map(|cap| (cap / 2) as usize)
            .unwrap_or(0)
    }

    async fn read_bytes(
        &self,
        path: &Path,
        request: &ReadRequest,
        context: &ToolCallContext,
    ) -> anyhow::Result<ReadOutput> {
        let config = self.infra.get_config()?;
        let size = std::fs::metadata(path)?.len() as usize;
        if size == 0 {
            return Ok(rendered(format!("{} is empty", path.display()), None));
        }
        let (mut start, mut end) = (request.start_byte.unwrap_or(0) as usize, request.end_byte);
        if let Some(stop) = end
            && (stop as usize) < start
        {
            end = Some(start as u64);
            start = stop as usize;
        }
        if start >= size {
            return Ok(rendered(
                format!(
                    "{} has {size} bytes; requested start_byte={start} is past EOF",
                    path.display()
                ),
                None,
            ));
        }
        let page = Self::byte_page(&config).max(1);
        let stop = size
            .min(start + page)
            .min(end.map(|e| e as usize).unwrap_or(size));
        if stop <= start {
            anyhow::bail!("empty byte range {start}-{}", end.unwrap_or_default());
        }
        let small = size as u64 <= config.max_file_size_bytes;
        let (base, buf) = if small {
            (0, read_regular(path)?)
        } else {
            use std::io::{Read, Seek, SeekFrom};
            let base = start.saturating_sub(3);
            let mut file = open_regular(path)?;
            file.seek(SeekFrom::Start(base as u64))?;
            let mut buf = vec![0; (size.min(stop + 1)) - base];
            file.read_exact(&mut buf)?;
            (base, buf)
        };
        if looks_binary(&buf) {
            anyhow::bail!(
                "byte ranges read text files; {} is not text",
                path.display()
            );
        }
        let first = base + char_start(&buf, start - base);
        let mut last = if stop >= size {
            size
        } else {
            base + char_start(&buf, stop - base)
        };
        if last <= first {
            last = stop.min(size);
        }
        let text = buf[first - base..last - base].to_str_lossy().into_owned();
        if small {
            let hash = hash_bytes(&buf);
            let total_lines = buf.split_inclusive(|b| *b == b'\n').count() as u64;
            let shown = lines_within(&buf, first, last);
            let whole = shown == Some((1, total_lines));
            let ledger = context.reads()?;
            ledger.with(|l| {
                l.record_read(
                    &key(path),
                    &hash,
                    shown,
                    Some(total_lines),
                    whole,
                    context.call_id(),
                );
                l.remember_content(&key(path), &hash, buf.clone());
            });
        }
        let footer = if last < size {
            format!(
                "[bytes {first}-{last} of {size}; continue with {}]",
                read_call(path, json!({"start_byte": last}))
            )
        } else {
            format!("[bytes {first}-{last} of {size}; end of file]")
        };
        Ok(rendered(format!("{text}\n{footer}"), None))
    }

    async fn read_pdf(
        &self,
        path: &Path,
        data: Vec<u8>,
        request: &ReadRequest,
        context: &ToolCallContext,
    ) -> anyhow::Result<ReadOutput> {
        let config = self.infra.get_config()?;
        let hash = hash_bytes(&data);
        let extracted = self
            .infra
            .execute_command_raw_output("pdftotext", &["-layout", &path.display().to_string(), "-"])
            .await;
        match extracted {
            Some(text) if !text.trim().is_empty() => {
                context.reads()?.with(|l| {
                    l.record_read(&key(path), &hash, None, None, true, context.call_id())
                });
                Ok(rendered(text, None))
            }
            _ if request.vision => {
                assert_file_size(&*self.infra, path, config.max_image_size_bytes)
                    .await
                    .context("PDF exceeds size limit. Use a smaller PDF or increase FORGE_MAX_IMAGE_SIZE.")?;
                context.reads()?.with(|l| {
                    l.record_read(&key(path), &hash, None, None, true, context.call_id())
                });
                let image = Image::new_bytes(data, "application/pdf".to_string());
                Ok(ReadOutput {
                    content: Content::image(image),
                    info: FileInfo::new(0, 0, 0, hash),
                })
            }
            None => anyhow::bail!(
                "pdftotext not installed: install poppler-utils (pdftotext) to read PDFs as text"
            ),
            Some(_) => anyhow::bail!("pdftotext found no extractable text in {}", path.display()),
        }
    }
}

fn rendered(text: String, numbered_window: Option<(u64, u64)>) -> ReadOutput {
    ReadOutput {
        content: Content::rendered(text, numbered_window),
        info: FileInfo::new(0, 0, 0, String::new()),
    }
}

/// Formats `lines` as `N:text`, numbered from `start`.
fn number_lines(lines: &[&str], start: u64) -> String {
    let width = (start + lines.len().saturating_sub(1) as u64)
        .to_string()
        .len();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}:{line}", start + i as u64, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extension trait so the read service can run a binary and capture stdout.
#[async_trait::async_trait]
trait RawOutput {
    /// Runs `program args` and returns stdout when it exits 0; `None` when the
    /// program is missing or fails.
    async fn execute_command_raw_output(&self, program: &str, args: &[&str]) -> Option<String>;
}

#[async_trait::async_trait]
impl<T: Send + Sync> RawOutput for T {
    async fn execute_command_raw_output(&self, program: &str, args: &[&str]) -> Option<String> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()?;
        output
            .status
            .success()
            .then(|| output.stdout.to_str_lossy().into_owned())
    }
}

#[async_trait::async_trait]
impl<
    F: FileInfoInfra
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + InfraFsReadService
        + CommandInfra,
> FsReadService for ForgeFsRead<F>
{
    async fn read(
        &self,
        path: String,
        start_line: Option<u64>,
        end_line: Option<u64>,
    ) -> anyhow::Result<ReadOutput> {
        let path = Path::new(&path);
        assert_absolute_path(path)?;

        let config = self.infra.get_config()?;

        // Validate with the larger limit initially since we don't know file
        // type yet
        let initial_size_limit = config.max_file_size_bytes.max(config.max_image_size_bytes);
        assert_file_size(&*self.infra, path, initial_size_limit).await?;

        // Read file content to detect MIME type
        let raw_content = self
            .infra
            .read(path)
            .await
            .with_context(|| format!("Failed to read file from {}", path.display()))?;

        // Detect MIME type
        let mime_type = detect_mime_type(path, &raw_content);

        // Handle visual content (PDFs and images)
        if is_visual_content(&mime_type) {
            assert_file_size(&*self.infra, path, config.max_image_size_bytes)
                .await
                .with_context(|| {
                    if mime_type == "application/pdf" {
                        "PDF exceeds size limit. Use a smaller PDF or increase FORGE_MAX_IMAGE_SIZE."
                    } else {
                        "Image exceeds size limit. Compress the image or increase FORGE_MAX_IMAGE_SIZE."
                    }
                })?;

            let image = Image::new_bytes(raw_content, mime_type.clone());
            let hash = compute_hash(image.url());

            return Ok(ReadOutput {
                content: Content::image(image),
                info: FileInfo::new(0, 0, 0, hash),
            });
        }

        let (start_line, end_line) = resolve_range(start_line, end_line, config.max_read_lines);

        // Convert bytes to UTF-8 string
        let full_content = String::from_utf8(raw_content)
            .with_context(|| format!("Failed to read file as UTF-8 from {}", path.display()))?;

        let hash = compute_hash(&full_content);

        // Now extract the requested range from the content we already have
        let lines: Vec<&str> = full_content.lines().collect();
        let total_lines = lines.len() as u64;

        // Convert to 0-based indexing and clamp to valid range
        let start_pos = start_line
            .saturating_sub(1)
            .min(total_lines.saturating_sub(1));
        let end_pos = end_line
            .saturating_sub(1)
            .min(total_lines.saturating_sub(1));

        let content = if total_lines == 0 {
            String::new()
        } else {
            lines
                .get(start_pos as usize..=end_pos as usize)
                .map(|slice| slice.join("\n"))
                .unwrap_or_default()
        };

        let file_info = FileInfo::new(start_line, end_line, total_lines, hash);

        Ok(ReadOutput { content: Content::file(content), info: file_info })
    }

    async fn read_tool(
        &self,
        request: ReadRequest,
        context: &ToolCallContext,
    ) -> anyhow::Result<ReadOutput> {
        let path = Path::new(&request.path);
        assert_absolute_path(path)?;
        let config = self.infra.get_config()?;
        let line_ranged = request.start_line.is_some() || request.end_line.is_some();
        let byte_ranged = request.start_byte.is_some() || request.end_byte.is_some();
        if line_ranged && byte_ranged {
            anyhow::bail!(
                "pass a line range (start_line/end_line) or a byte range (start_byte/end_byte), not both"
            );
        }
        let meta =
            std::fs::metadata(path).with_context(|| format!("no such file: {}", path.display()))?;
        if !meta.is_file() {
            anyhow::bail!("not a regular file: {}", path.display());
        }
        if byte_ranged {
            return self.read_bytes(path, &request, context).await;
        }

        let size = meta.len();
        let is_notebook = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ipynb"));
        let header = {
            use std::io::Read;
            let mut head = vec![0u8; 16];
            let n = open_regular(path)?.read(&mut head)?;
            head.truncate(n);
            head
        };
        let mime = detect_mime_type(path, &header);

        if mime.starts_with("image/") {
            assert_file_size(&*self.infra, path, config.max_image_size_bytes)
                .await
                .context("Image exceeds size limit. Compress the image or increase FORGE_MAX_IMAGE_SIZE.")?;
            let data = read_regular(path)?;
            let hash = hash_bytes(&data);
            context
                .reads()?
                .with(|l| l.record_read(&key(path), &hash, None, None, true, context.call_id()));
            if !request.vision {
                return Ok(rendered(
                    format!(
                        "VISUAL_FILE {} mime={mime} size={size} bytes (this model has no image input; no image bytes returned)",
                        path.display()
                    ),
                    None,
                ));
            }
            let image = Image::new_bytes(data, mime);
            return Ok(ReadOutput {
                content: Content::image(image),
                info: FileInfo::new(0, 0, 0, hash),
            });
        }
        if mime == "application/pdf" {
            if size > config.max_file_size_bytes {
                anyhow::bail!(
                    "File size ({size} bytes) exceeds the maximum allowed size of {} bytes",
                    config.max_file_size_bytes
                );
            }
            let data = read_regular(path)?;
            return self.read_pdf(path, data, &request, context).await;
        }

        if is_notebook && !line_ranged {
            if size > config.max_file_size_bytes {
                anyhow::bail!(
                    "File size ({size} bytes) exceeds the maximum allowed size of {} bytes",
                    config.max_file_size_bytes
                );
            }
            let data = read_regular(path)?;
            if let Ok(view) = notebook::render_view(path, &data) {
                let hash = hash_bytes(&data);
                let ledger = context.reads()?;
                ledger.with(|l| {
                    l.record_read(&key(path), &hash, None, None, false, context.call_id());
                    l.remember_content(&key(path), &hash, data.clone());
                });
                return Ok(rendered(view, None));
            }
        }

        if !line_ranged && config.max_read_bytes > 0 && size > config.max_read_bytes {
            anyhow::bail!(
                "file size ({size} bytes) exceeds max_read_bytes ({}) for a whole-file read. Read a range instead, e.g. {} — ranged reads are not subject to this cap.",
                config.max_read_bytes,
                read_call(
                    path,
                    json!({"start_line": 1, "end_line": config.max_read_lines})
                )
            );
        }
        if size > config.max_file_size_bytes {
            anyhow::bail!(
                "File size ({size} bytes) exceeds the maximum allowed size of {} bytes",
                config.max_file_size_bytes
            );
        }
        let data = read_regular(path)?;
        if looks_binary(&data) {
            anyhow::bail!(
                "{} is a binary file; read it with a byte range only if it is text",
                path.display()
            );
        }
        let text = std::str::from_utf8(&data)
            .with_context(|| format!("Failed to read file as UTF-8 from {}", path.display()))?;
        let hash = hash_bytes(&data);
        let all: Vec<&str> = text.lines().collect();
        let total = all.len() as u64;
        let ledger = context.reads()?;
        if total == 0 {
            ledger.with(|l| {
                l.record_read(&key(path), &hash, None, Some(0), true, context.call_id());
                l.remember_content(&key(path), &hash, data.clone());
            });
            return Ok(rendered(
                format!(
                    "{} is empty (hash {})",
                    path.display(),
                    forge_domain::short_hash(&hash)
                ),
                None,
            ));
        }

        let mut start = request.start_line.unwrap_or(1).max(1);
        let mut end_request = request.end_line;
        if let Some(end) = end_request
            && end < start
        {
            end_request = Some(start);
            start = end.max(1);
        }
        if start > total {
            return Ok(rendered(
                format!(
                    "{} has {total} total lines; requested start_line={start} is past EOF (hash {})",
                    path.display(),
                    forge_domain::short_hash(&hash)
                ),
                None,
            ));
        }
        let page_end = start + config.max_read_lines.max(1) - 1;
        let end = total.min(page_end).min(end_request.unwrap_or(page_end));
        let read_key = ReadKey {
            hash: hash.clone(),
            start,
            end,
            numbered: request.show_line_numbers,
        };
        let earlier = ledger.with(|l| l.shown_read(&key(path), &read_key).map(str::to_string));
        ledger.with(|l| {
            l.record_read(
                &key(path),
                &hash,
                Some((start, end)),
                Some(total),
                start == 1 && end == total,
                context.call_id(),
            );
            l.remember_content(&key(path), &hash, data.clone());
        });
        let footer = if end < total {
            format!(
                "\n[{total} total lines; continue with {}]",
                read_call(path, json!({"start_line": end + 1}))
            )
        } else {
            String::new()
        };
        if let Some(earlier) = earlier {
            return Ok(rendered(
                format!(
                    "Lines {start}-{end} are unchanged since read call {earlier} returned them (hash {}); that result is current.{footer}",
                    forge_domain::short_hash(&hash)
                ),
                None,
            ));
        }
        let selected = &all[(start - 1) as usize..end as usize];
        let body = if request.show_line_numbers {
            number_lines(selected, start)
        } else {
            selected.join("\n")
        };
        if let Some(call_id) = context.call_id() {
            ledger.with(|l| l.offer_read(call_id, &key(path), read_key));
        }
        let window = request.show_line_numbers.then_some((start, end));
        let mut output = rendered(format!("{body}{footer}"), window);
        output.info = FileInfo::new(start, end, total, hash);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use forge_domain::Metrics;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;
    use tokio::fs;

    use super::*;
    use crate::attachment::tests::MockFileService;

    // Helper to create a temporary file with specific content size
    async fn create_test_file_with_size(size: usize) -> anyhow::Result<NamedTempFile> {
        let file = NamedTempFile::new()?;
        let content = "x".repeat(size);
        fs::write(file.path(), content).await?;
        Ok(file)
    }

    #[tokio::test]
    async fn test_assert_file_size_within_limit() {
        let fixture = create_test_file_with_size(13).await.unwrap();
        let infra = MockFileService::new();
        infra.add_file(fixture.path().to_path_buf(), "x".repeat(13));
        let actual = assert_file_size(&infra, fixture.path(), 20u64).await;
        assert!(actual.is_ok());
    }

    #[tokio::test]
    async fn test_assert_file_size_exceeds_limit() {
        let fixture = create_test_file_with_size(45).await.unwrap();
        let infra = MockFileService::new();
        infra.add_file(fixture.path().to_path_buf(), "x".repeat(45));
        let actual = assert_file_size(&infra, fixture.path(), 10u64).await;
        assert!(actual.is_err());
    }

    #[tokio::test]
    async fn test_assert_file_size_error_message() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), "too long content").await.unwrap(); // 16 bytes
        let infra = MockFileService::new();
        infra.add_file(file.path().to_path_buf(), "too long content".to_string());
        let actual = assert_file_size(&infra, file.path(), 5u64).await;
        let expected = "File size (16 bytes) exceeds the maximum allowed size of 5 bytes";
        assert!(actual.is_err());
        assert_eq!(actual.unwrap_err().to_string(), expected);
    }

    #[test]
    fn test_detect_mime_type_for_png() {
        let path = Path::new("image.png");
        let content = b"\x89PNG\r\n\x1a\n";
        let actual = detect_mime_type(path, content);
        assert_eq!(actual, "image/png");
    }

    #[test]
    fn test_detect_mime_type_for_pdf_with_magic() {
        let path = Path::new("document.pdf");
        let content = b"%PDF-1.4";
        let actual = detect_mime_type(path, content);
        assert_eq!(actual, "application/pdf");
    }

    #[test]
    fn test_is_visual_content() {
        let actual = (
            is_visual_content("image/png"),
            is_visual_content("application/pdf"),
            is_visual_content("text/plain"),
        );
        let expected = (true, true, false);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_char_start_snaps_to_utf8_boundary() {
        let fixture = "aé".as_bytes(); // 'é' is 2 bytes at offset 1..3
        let actual = (
            char_start(fixture, 2),
            char_start(fixture, 1),
            char_start(fixture, 0),
        );
        let expected = (1, 1, 0);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_lines_within_counts_whole_lines_only() {
        let fixture = b"one\ntwo\nthree\n";
        let actual = (
            lines_within(fixture, 0, 8),
            lines_within(fixture, 2, 8),
            lines_within(fixture, 0, 3),
        );
        let expected = (Some((1, 2)), Some((2, 2)), None);
        assert_eq!(actual, expected);
    }

    // --- read_tool against real files ---

    #[derive(Clone)]
    struct RealInfra {
        config: forge_config::ForgeConfig,
    }

    impl EnvironmentInfra for RealInfra {
        type Config = forge_config::ForgeConfig;
        fn get_env_var(&self, _key: &str) -> Option<String> {
            None
        }
        fn get_env_vars(&self) -> std::collections::BTreeMap<String, String> {
            Default::default()
        }
        fn get_environment(&self) -> forge_domain::Environment {
            use fake::{Fake, Faker};
            Faker.fake()
        }
        fn get_config(&self) -> anyhow::Result<Self::Config> {
            Ok(self.config.clone())
        }
        async fn update_environment(
            &self,
            _ops: Vec<forge_domain::ConfigOperation>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl FileInfoInfra for RealInfra {
        async fn is_binary(&self, _path: &Path) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn is_file(&self, path: &Path) -> anyhow::Result<bool> {
            Ok(path.is_file())
        }
        async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
            Ok(path.exists())
        }
        async fn file_size(&self, path: &Path) -> anyhow::Result<u64> {
            Ok(std::fs::metadata(path)?.len())
        }
    }

    #[async_trait::async_trait]
    impl InfraFsReadService for RealInfra {
        async fn read_utf8(&self, path: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(path)?)
        }
        fn read_batch_utf8(
            &self,
            _batch_size: usize,
            _paths: Vec<PathBuf>,
        ) -> impl futures::Stream<Item = (PathBuf, anyhow::Result<String>)> + Send {
            futures::stream::empty()
        }
        async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            Ok(std::fs::read(path)?)
        }
        async fn range_read_utf8(
            &self,
            _path: &Path,
            _start_line: u64,
            _end_line: u64,
        ) -> anyhow::Result<(String, FileInfo)> {
            unimplemented!()
        }
    }

    #[async_trait::async_trait]
    impl CommandInfra for RealInfra {
        async fn execute_command(
            &self,
            _command: String,
            _working_dir: PathBuf,
            _silent: bool,
            _env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<forge_domain::CommandOutput> {
            unimplemented!()
        }
        async fn execute_command_raw(
            &self,
            _command: &str,
            _working_dir: PathBuf,
            _env_vars: Option<Vec<String>>,
        ) -> anyhow::Result<std::process::ExitStatus> {
            unimplemented!()
        }
    }

    fn service(max_read_lines: u64) -> ForgeFsRead<RealInfra> {
        let config = forge_config::ForgeConfig {
            max_read_lines,
            max_read_bytes: 1024,
            max_file_size_bytes: 1 << 20,
            max_image_size_bytes: 1 << 20,
            max_tool_result_inline_bytes: 40,
            max_tool_result_bytes: 1000,
            ..Default::default()
        };
        ForgeFsRead::new(Arc::new(RealInfra { config }))
    }

    fn request(path: &Path) -> ReadRequest {
        ReadRequest {
            path: path.display().to_string(),
            show_line_numbers: true,
            ..Default::default()
        }
    }

    fn text(output: ReadOutput) -> String {
        output.content.file_content().to_string()
    }

    #[tokio::test]
    async fn test_read_tool_pages_with_footer_and_records_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();
        let context = ToolCallContext::new(Metrics::default()).with_call_id("c1");

        let actual = text(
            service(2)
                .read_tool(request(&file), &context)
                .await
                .unwrap(),
        );

        let expected = format!(
            "1:a\n2:b\n[5 total lines; continue with {}]",
            json!({"file_path": file.display().to_string(), "range": {"start_line": 3}})
        );
        assert_eq!(actual, expected);
        let coverage = context
            .reads()
            .unwrap()
            .with(|l| l.coverage(&key(&file)).cloned())
            .unwrap();
        assert_eq!(coverage.ranges, vec![(1, 2)]);
    }

    #[tokio::test]
    async fn test_read_tool_keeps_long_lines_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("long.txt");
        let line = "x".repeat(5000);
        std::fs::write(&file, format!("{line}\n")).unwrap();
        let config = forge_config::ForgeConfig {
            max_read_lines: 10,
            max_read_bytes: 1 << 20,
            max_file_size_bytes: 1 << 20,
            ..Default::default()
        };
        let service = ForgeFsRead::new(Arc::new(RealInfra { config }));
        let context = ToolCallContext::new(Metrics::default());

        let actual = text(service.read_tool(request(&file), &context).await.unwrap());

        let expected = format!("1:{line}");
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_read_tool_refuses_large_whole_file_and_suggests_range() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, "y\n".repeat(1000)).unwrap();
        let context = ToolCallContext::new(Metrics::default());

        let actual = service(10)
            .read_tool(request(&file), &context)
            .await
            .unwrap_err()
            .to_string();

        assert!(actual.contains("exceeds max_read_bytes (1024)"), "{actual}");
        assert!(actual.contains("\"start_line\":1"), "{actual}");
    }

    #[tokio::test]
    async fn test_read_tool_byte_range_snaps_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("b.txt");
        std::fs::write(&file, "héllo wörld, this is a long single line of text").unwrap();
        let context = ToolCallContext::new(Metrics::default());
        let request = request(&file).start_byte(2u64);

        let actual = text(service(10).read_tool(request, &context).await.unwrap());

        // page = min(40, 1000) / 2 = 20 bytes from the requested start 2 (stop
        // 22); the start snaps back to 1, the first byte of 'é'.
        assert!(actual.starts_with("éllo"), "{actual}");
        assert!(actual.contains("[bytes 1-22 of 49;"), "{actual}");
        assert!(actual.contains("\"start_byte\":22"), "{actual}");
    }

    #[tokio::test]
    async fn test_read_tool_repeat_read_returns_stub_after_settle() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let context = ToolCallContext::new(Metrics::default()).with_call_id("first");
        let service = service(10);
        let first = text(service.read_tool(request(&file), &context).await.unwrap());
        let ledger = context.reads().unwrap();
        ledger.with(|l| {
            l.settle_read("first", "digest");
            l.settle_windows();
        });

        let second_context = context.for_call(Some("second".to_string()));
        let actual = text(
            service
                .read_tool(request(&file), &second_context)
                .await
                .unwrap(),
        );

        assert_eq!(first, "1:one\n2:two");
        assert!(
            actual.starts_with("Lines 1-2 are unchanged since read call first returned them"),
            "{actual}"
        );
    }

    #[tokio::test]
    async fn test_read_tool_rejects_mixed_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.txt");
        std::fs::write(&file, "x\n").unwrap();
        let context = ToolCallContext::new(Metrics::default());
        let request = request(&file).start_line(1u64).start_byte(0u64);

        let actual = service(10)
            .read_tool(request, &context)
            .await
            .unwrap_err()
            .to_string();

        let expected = "pass a line range (start_line/end_line) or a byte range (start_byte/end_byte), not both";
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_read_tool_does_not_block_on_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let context = ToolCallContext::new(Metrics::default());

        let actual = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            service(10).read_tool(request(&fifo), &context),
        )
        .await
        .expect("read must not block on a FIFO")
        .unwrap_err()
        .to_string();

        assert!(actual.contains("not a regular file"), "{actual}");
    }

    #[tokio::test]
    async fn test_read_tool_notebook_view() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("n.ipynb");
        let notebook = json!({
            "nbformat": 4, "nbformat_minor": 5,
            "metadata": {"language_info": {"name": "python"}},
            "cells": [
                {"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# Title"]},
                {"cell_type": "code", "id": "c1", "execution_count": 1, "metadata": {},
                 "outputs": [{"output_type": "stream", "name": "stdout", "text": ["hi\n"]}],
                 "source": ["print('hi')"]}
            ]
        });
        std::fs::write(&file, serde_json::to_string_pretty(&notebook).unwrap()).unwrap();
        let context = ToolCallContext::new(Metrics::default());

        let actual = text(
            service(100)
                .read_tool(request(&file), &context)
                .await
                .unwrap(),
        );

        assert!(
            actual.contains("[cell 0 id=intro markdown]\n# Title"),
            "{actual}"
        );
        assert!(
            actual.contains("[cell 1 id=c1 code [1]]\nprint('hi')"),
            "{actual}"
        );
        assert!(
            actual.contains("[outputs of c1]\n  stdout:\n    hi"),
            "{actual}"
        );
    }
}
