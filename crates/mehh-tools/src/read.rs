use std::path::PathBuf;

use async_trait::async_trait;
use base64::prelude::*;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, ToolResultContentBlock};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};

const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TEXT_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_READ_LINES: usize = 2000;
const MAX_READ_OUTPUT_BYTES: usize = 100 * 1024;
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct ReadTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file contents. Supports text files and images (png, jpg, jpeg, gif, webp). Text output is bounded by line count and total bytes; binary files are rejected."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (max 2000)"
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let offset = params
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(MAX_READ_LINES as i64)
            .clamp(1, MAX_READ_LINES as i64) as usize;

        let full_path = self.resolve_path(path_str)?;

        let meta = match tokio::fs::metadata(&full_path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("File not found: {}", path_str));
            }
            Err(e) => return Err(format!("Failed to stat file {}: {}", path_str, e)),
        };
        if meta.is_dir() {
            return Err(format!("Path is a directory: {}", path_str));
        }
        if meta.len() == 0 {
            return Ok(AgentToolResult::text(""));
        }

        let ext = full_path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let image_mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };

        if let Some(mime_type) = image_mime {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                return Err(format!(
                    "Image file {} is too large ({} bytes exceeds {} byte limit)",
                    path_str,
                    meta.len(),
                    MAX_IMAGE_FILE_BYTES
                ));
            }
            let bytes = tokio::fs::read(&full_path)
                .await
                .map_err(|e| format!("Failed to read image file {}: {}", path_str, e))?;
            return Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Image(ImageContent {
                    data: BASE64_STANDARD.encode(&bytes),
                    mime_type: mime_type.to_string(),
                })],
                details: None,
                ..Default::default()
            });
        }

        if meta.len() > MAX_TEXT_FILE_BYTES {
            return Err(format!(
                "File {} is too large ({} bytes exceeds {} byte limit)",
                path_str,
                meta.len(),
                MAX_TEXT_FILE_BYTES
            ));
        }

        // Sniff for NUL bytes before decoding the whole file; a 9 MB binary
        // must not be turned into a multi-million-character text block.
        let mut first = tokio::fs::File::open(&full_path)
            .await
            .map_err(|e| format!("Failed to open {}: {e}", path_str))?;
        let mut sniff = [0u8; 1024];
        let sniff_len = first
            .read(&mut sniff)
            .await
            .map_err(|e| format!("Failed to read {}: {e}", path_str))?;
        if sniff[..sniff_len].contains(&0) {
            return Err(format!("File {} appears to be binary", path_str));
        }
        drop(first);

        let file = tokio::fs::File::open(&full_path)
            .await
            .map_err(|e| format!("Failed to open {}: {e}", path_str))?;
        let mut reader = BufReader::new(file);

        let start_line = offset - 1;
        let stop_line = start_line + limit;
        let mut line_no = 0usize;
        let mut total_lines = 0usize;
        let mut shown_lines = 0usize;
        let mut output_bytes = 0usize;
        let mut output_capped = false;
        let mut output = String::new();
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            let read = reader.read_line(&mut line_buf).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    format!("File {} is not valid UTF-8 and appears to be binary", path_str)
                } else {
                    format!("Failed to read {}: {e}", path_str)
                }
            })?;
            if read == 0 {
                break;
            }
            total_lines += 1;

            if line_no >= start_line && line_no < stop_line {
                let mut line = line_buf.strip_suffix('\n').unwrap_or(&line_buf);
                line = line.strip_suffix('\r').unwrap_or(line);
                if line_no == 0 {
                    line = line.strip_prefix('\u{feff}').unwrap_or(line);
                }
                let formatted = match line.char_indices().nth(MAX_LINE_CHARS) {
                    Some((cut, _)) => format!("{}... [truncated]", &line[..cut]),
                    None => line.to_string(),
                };
                if !output_capped && output_bytes + formatted.len() + 1 > MAX_READ_OUTPUT_BYTES {
                    output_capped = true;
                } else if !output_capped {
                    if shown_lines > 0 {
                        output.push('\n');
                    }
                    output.push_str(&formatted);
                    output_bytes += formatted.len() + 1;
                    shown_lines += 1;
                }
            }

            line_no += 1;
            if line_no.is_multiple_of(1024) && cancel.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }
        }

        if start_line >= total_lines && total_lines > 0 {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                offset, total_lines
            ));
        }

        let end_line = start_line + shown_lines;
        if end_line < total_lines || start_line > 0 {
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to read more.]",
                start_line + 1,
                end_line,
                total_lines,
                end_line + 1
            ));
        }
        if output_capped {
            output.push_str(&format!(
                "\n\n[Output truncated at {} byte limit]",
                MAX_READ_OUTPUT_BYTES
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}
