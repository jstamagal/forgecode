//! Jupyter notebooks: `read` shows an `.ipynb` as cells (each with its id,
//! type, source and an output summary), and `notebook_edit` changes one cell.
//!
//! A cell's id is its nbformat `id`; a notebook older than nbformat 4.5 has
//! none and its cells are named `cell-N` by 0-based index.

use std::path::Path;

use bstr::ByteSlice;
use serde_json::Value;

/// Lines of one output's text shown per output.
const OUTPUT_LINES: usize = 10;
/// Characters of one output line shown.
const CHARS_PER_LINE: usize = 200;

/// Parses notebook JSON.
///
/// # Errors
/// Returns an error when the bytes are not a JSON object with a `cells` list
/// of objects.
pub fn load(data: &[u8]) -> anyhow::Result<serde_json::Map<String, Value>> {
    let value: Value = serde_json::from_slice(data)
        .map_err(|error| anyhow::anyhow!("not valid notebook JSON: {error}"))?;
    let Value::Object(map) = value else {
        anyhow::bail!("not a notebook: top level is not an object");
    };
    match map.get("cells") {
        Some(Value::Array(cells)) if cells.iter().all(Value::is_object) => Ok(map),
        _ => anyhow::bail!("not a notebook: no cells list"),
    }
}

/// Joins a notebook string-or-list value.
pub fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Each cell's id: its `id`, or `cell-N` when it has none.
pub fn cell_ids(cells: &[Value]) -> Vec<String> {
    cells
        .iter()
        .enumerate()
        .map(
            |(index, cell)| match cell.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => format!("cell-{index}"),
            },
        )
        .collect()
}

fn strip_ansi(text: &str) -> String {
    strip_ansi_escapes::strip(text.as_bytes())
        .to_str_lossy()
        .into_owned()
}

fn clip(text: &str) -> Vec<String> {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean.trim_end_matches('\n').lines().collect();
    let mut shown: Vec<String> = lines
        .iter()
        .take(OUTPUT_LINES)
        .map(|line| {
            if line.chars().count() > CHARS_PER_LINE {
                format!(
                    "{} [...]",
                    line.chars().take(CHARS_PER_LINE).collect::<String>()
                )
            } else {
                line.to_string()
            }
        })
        .collect();
    if lines.len() > OUTPUT_LINES {
        shown.push(format!("[+{} more lines]", lines.len() - OUTPUT_LINES));
    }
    shown
}

fn summarize_output(output: &Value) -> Vec<String> {
    let kind = output
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("output");
    if kind == "stream" {
        let name = output
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("stream");
        let mut out = vec![format!("{name}:")];
        out.extend(
            clip(&text_of(output.get("text")))
                .into_iter()
                .map(|l| format!("  {l}")),
        );
        return out;
    }
    if kind == "error" {
        let ename = output.get("ename").and_then(Value::as_str).unwrap_or("");
        let evalue = strip_ansi(output.get("evalue").and_then(Value::as_str).unwrap_or(""));
        return vec![
            format!("error {ename}: {evalue}")
                .trim_end_matches([':', ' '])
                .to_string(),
        ];
    }
    let data = output.get("data").and_then(Value::as_object);
    let others: Vec<String> = data
        .map(|data| {
            data.iter()
                .filter(|(mime, _)| mime.as_str() != "text/plain")
                .map(|(mime, payload)| format!("{mime} ({} B)", text_of(Some(payload)).len()))
                .collect()
        })
        .unwrap_or_default();
    let label = if kind == "execute_result" {
        "result"
    } else {
        kind
    };
    let plain = data.and_then(|data| data.get("text/plain"));
    let mut kinds = Vec::new();
    if plain.is_some() {
        kinds.push("text/plain".to_string());
    }
    kinds.extend(others);
    let lines = plain.map(|p| clip(&text_of(Some(p)))).unwrap_or_default();
    let mut heading = format!("{label}: {}", kinds.join(", "));
    heading = heading.trim_end_matches([':', ' ']).to_string();
    if !lines.is_empty() {
        heading.push(':');
    }
    let mut out = vec![heading];
    out.extend(lines.into_iter().map(|l| format!("  {l}")));
    out
}

/// Renders a notebook the way the `read` tool shows it.
///
/// # Errors
/// Returns an error when the bytes are not a notebook.
pub fn render_view(path: &Path, data: &[u8]) -> anyhow::Result<String> {
    let notebook = load(data)?;
    let cells = notebook
        .get("cells")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let metadata = notebook.get("metadata");
    let language = metadata
        .and_then(|m| m.get("language_info"))
        .and_then(|i| i.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            metadata
                .and_then(|m| m.get("kernelspec"))
                .and_then(|k| k.get("language"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let version = format!(
        "nbformat {}.{}",
        notebook
            .get("nbformat")
            .map_or("?".to_string(), Value::to_string),
        notebook
            .get("nbformat_minor")
            .map_or("?".to_string(), Value::to_string)
    );
    let about = [language, version.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let count = format!(
        "{} cell{}",
        cells.len(),
        if cells.len() == 1 { "" } else { "s" }
    );
    let hash = crate::tool_services::provenance::hash_bytes(data);
    let mut out = vec![format!(
        "{}: notebook, {count} ({about}), hash {}",
        path.display(),
        forge_domain::short_hash(&hash)
    )];
    for (index, (cell, id)) in cells.iter().zip(cell_ids(&cells)).enumerate() {
        let kind = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code");
        let mut head = format!("[cell {index} id={id} {kind}");
        if kind == "code" {
            let count = cell
                .get("execution_count")
                .and_then(Value::as_i64)
                .map_or(" ".to_string(), |n| n.to_string());
            head.push_str(&format!(" [{count}]"));
        }
        out.push(String::new());
        out.push(format!("{head}]"));
        let source = text_of(cell.get("source"));
        out.push(if source.is_empty() {
            "(empty)".to_string()
        } else {
            source
        });
        if let Some(outputs) = cell.get("outputs").and_then(Value::as_array)
            && !outputs.is_empty()
        {
            out.push(format!("[outputs of {id}]"));
            for output in outputs.iter().filter(|o| o.is_object()) {
                out.extend(
                    summarize_output(output)
                        .into_iter()
                        .map(|l| format!("  {l}")),
                );
            }
        }
    }
    Ok(out.join("\n"))
}
