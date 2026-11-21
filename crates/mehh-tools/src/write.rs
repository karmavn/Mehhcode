use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write;
use crate::file_mutation_queue::with_file_mutation_queue;
use crate::undo_edit::{EditUndoStore, FileSnapshot};

#[derive(Debug, Clone)]
pub struct WriteTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Option<Arc<EditUndoStore>>,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store: None,
        }
    }

    pub fn with_undo_store(mut self, undo_store: Arc<EditUndoStore>) -> Self {
        self.undo_store = Some(undo_store);
        self
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

        let full_path = self.resolve_path(path_str)?;
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

            // Snapshot must be captured before the replacement, but only
            // recorded after the atomic write succeeds.
            let snapshot = if undo_store.is_some() {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if full_path.exists() {
                    tokio::fs::read(&full_path)
                        .await
                        .ok()
                        .map(|bytes| FileSnapshot::Existed { bytes, timestamp_ms })
                } else {
                    Some(FileSnapshot::DidNotExist)
                }
            } else {
                None
            };

            atomic_write(&full_path, content.as_bytes()).await?;

            if let (Some(store), Some(snapshot)) = (undo_store.as_ref(), snapshot) {
                store.push_snapshot(&full_path, snapshot);
            }

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
