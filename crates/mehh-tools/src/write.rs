use std::path::{Path, PathBuf};
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use crate::file_mutation_queue::with_file_mutation_queue;
use crate::undo_edit::{EditUndoStore, FileSnapshot};

#[derive(Debug, Clone)]
pub struct WriteTool {
    pub cwd: PathBuf,
    pub undo_store: Option<Arc<EditUndoStore>>,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            undo_store: None,
        }
    }

    pub fn with_undo_store(mut self, undo_store: Arc<EditUndoStore>) -> Self {
        self.undo_store = Some(undo_store);
        self
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
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
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

        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'content'".to_string())?;

        let full_path = self.resolve_path(path_str);
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();

        with_file_mutation_queue(&full_path, async {
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("Failed to create directories for {}: {}", path_str, e))?;
            }

            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            if let Some(ref store) = undo_store {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if full_path.exists() {
                    if let Ok(bytes) = tokio::fs::read(&full_path).await {
                        store.push_snapshot(&full_path, FileSnapshot::Existed { bytes, timestamp_ms });
                    }
                } else {
                    store.push_snapshot(&full_path, FileSnapshot::DidNotExist);
                }
            }

            tokio::fs::write(&full_path, content.as_bytes())
                .await
                .map_err(|e| format!("Failed to write to {}: {}", path_str, e))?;

            let byte_count = content.len();
            let line_count = content.lines().count();

            Ok(AgentToolResult::text(format!(
                "Successfully wrote {} bytes ({} lines) to {}",
                byte_count, line_count, path_str
            )))
        })
        .await
    }
}
