use std::path::{Path, PathBuf};
use async_trait::async_trait;
use base64::prelude::*;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ReadTool {
    pub cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    fn resolve_path(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file contents. Supports text files and images (png, jpg, jpeg, gif, webp). For text files, output is limited by offset and limit."
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
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
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
            .unwrap_or(2000)
            .max(1) as usize;

        let full_path = self.resolve_path(path_str);

        if !full_path.exists() {
            return Err(format!("File not found: {}", path_str));
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
            let bytes = tokio::fs::read(&full_path)
                .await
                .map_err(|e| format!("Failed to read image file {}: {}", path_str, e))?;
            let data = BASE64_STANDARD.encode(&bytes);
            return Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Image(ImageContent {
                    data,
                    mime_type: mime_type.to_string(),
                })],
                details: None,
            });
        }

        let bytes = tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read file {}: {}", path_str, e))?;

        if bytes.is_empty() {
            return Ok(AgentToolResult::text(""));
        }

        let content_str = String::from_utf8_lossy(&bytes);

        let all_lines: Vec<&str> = content_str
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();

        let total_lines = all_lines.len();

        let start_line = offset - 1;
        if start_line >= total_lines {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                offset, total_lines
            ));
        }

        let end_line = (start_line + limit).min(total_lines);

        let mut formatted_lines = Vec::with_capacity(end_line - start_line);
        for line in &all_lines[start_line..end_line] {
            if line.chars().count() > 2000 {
                let truncated: String = line.chars().take(2000).collect();
                formatted_lines.push(format!("{}... [truncated]", truncated));
            } else {
                formatted_lines.push(line.to_string());
            }
        }

        let mut output = formatted_lines.join("\n");

        if end_line < total_lines || start_line > 0 {
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to read more.]",
                start_line + 1,
                end_line,
                total_lines,
                end_line + 1
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}
