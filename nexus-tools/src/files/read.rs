//! `Read`: numbered lines, as `cat -n`.

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};

use super::{FileConfig, file_state, pdf};
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// Characters returned at most. Claude Code's real cap is in tokens (about 25 000); a
/// tokenizer is not available in pure Rust, so this approximates it from the recording
/// (1 250 lines / 56 392 characters were returned, the next line was not). Unlike Claude
/// Code, a cut is **announced**.
pub const MAX_OUTPUT_CHARS: usize = 56_400;

/// Largest picture returned as an image block.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

fn image_type(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The type the bytes really are: an extension is a claim, the first bytes are the fact.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The `Read` tool.
#[derive(Debug)]
pub struct ReadTool {
    config: FileConfig,
}

impl ReadTool {
    /// A `Read` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. Lines come back numbered (`N<TAB>text`). \
         `offset` is the number of the first line to return and `limit` how many lines. \
         Jupyter notebooks come back as cells."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "description": "Path of the file to read"},
                "offset": {"type": "integer", "minimum": 0, "description": "Number of the first line to return"},
                "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return"},
                "pages": {"type": "string", "description": "PDF page range, such as \"3\", \"1-5\" or \"2,4-6\" (at most 20 pages per request)"}
            },
            "required": ["file_path"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations::read_only()
    }

    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult {
        let Some(given) = arguments.get("file_path").and_then(Value::as_str) else {
            return ToolResult::error("`file_path` is required and must be a string");
        };
        let offset = arguments.get("offset").and_then(Value::as_u64);
        let limit = arguments.get("limit").and_then(Value::as_u64);
        let path = match self.config.scope.resolve(given) {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                return ToolResult::error(format!(
                    "File does not exist. Note: your current working directory is {}.",
                    self.config.scope.cwd().display()
                ));
            },
        };
        if meta.is_dir() {
            return ToolResult::error(format!(
                "EISDIR: illegal operation on a directory, read '{given}'"
            ));
        }
        // A picture comes back as an image block, with a text line that stands in for it where the
        // consumer cannot show images (the native harness has none in v1).
        if let Some(mime) = image_type(&path)
            && meta.len() <= MAX_IMAGE_BYTES
            && let Ok(bytes) = std::fs::read(&path)
            && sniff(&bytes) == Some(mime)
        {
            file_state(context).record(&path);
            return ToolResult::ok(format!("[image: {given}, {mime}, {} bytes]", bytes.len()))
                .with_image(crate::tool::ImageBlock {
                    mime_type: mime.to_owned(),
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                });
        }
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            return match pdf::read(&path, arguments.get("pages").and_then(Value::as_str)) {
                Ok(text) => {
                    file_state(context).record(&path);
                    ToolResult::ok(text)
                },
                Err(message) => ToolResult::error(message),
            };
        }
        if meta.len() > self.config.max_text_bytes {
            return self.config.too_large(given, meta.len(), "Read");
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return ToolResult::error(format!("{error}: read '{given}'")),
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolResult::error(format!(
                "This file is not valid UTF-8 text and cannot be read as lines: {given}"
            ));
        };
        // Reading it is what makes it editable, even if the answer is a warning.
        file_state(context).record(&path);
        if text.is_empty() {
            return ToolResult::ok(
                "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>",
            );
        }
        if path.extension().is_some_and(|e| e == "ipynb")
            && let Some(rendered) = notebook(&text)
        {
            return ToolResult::ok(rendered);
        }
        ToolResult::ok(number_lines(&text, offset, limit))
    }
}

/// `cat -n` of `text`, the way Claude Code does it: split on `\n`, so a file that ends with
/// a newline has a last, empty, numbered line. `offset` is the number given to the first
/// line returned (0 is taken literally and shifts the numbering).
fn number_lines(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let offset = offset.unwrap_or(1);
    let start = usize::try_from(offset.saturating_sub(1)).unwrap_or(usize::MAX);
    if start >= lines.len() {
        return format!(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {} lines.</system-reminder>",
            lines.len()
        );
    }
    let take = limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut out = String::new();
    for (returned, (number, line)) in (offset..).zip(lines[start..].iter().take(take)).enumerate() {
        let row = format!("{number}\t{line}");
        // A single line is always returned whole, however long: only whole lines are dropped.
        if returned > 0 && out.len() + row.len() + 1 > MAX_OUTPUT_CHARS {
            out.push_str(&format!(
                "\n[output truncated: stopped at line {} of {}; call Read again with offset {} to continue]",
                number - 1,
                lines.len(),
                number
            ));
            return out;
        }
        if returned > 0 {
            out.push('\n');
        }
        out.push_str(&row);
    }
    out
}

/// A notebook as `<cell id="…">source</cell id="…">` blocks; markdown cells say so.
fn notebook(text: &str) -> Option<String> {
    let document: Value = serde_json::from_str(text).ok()?;
    let cells = document.get("cells")?.as_array()?;
    let blocks: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let id = cell
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| index.to_string(), str::to_owned);
            let kind = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("code");
            let marker = if kind == "code" {
                String::new()
            } else {
                format!("<cell_type>{kind}</cell_type>")
            };
            format!(
                "<cell id=\"{id}\">{marker}{}</cell id=\"{id}\">",
                source(cell)
            )
        })
        .collect();
    Some(blocks.join("\n"))
}

/// A cell's source, whether stored as a string or as a list of lines.
pub(crate) fn source(cell: &Value) -> String {
    match cell.get("source") {
        Some(Value::String(one)) => one.clone(),
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}
