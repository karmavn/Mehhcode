use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use crate::core::context::{self, PathPolicy};
use crate::core::fs::{atomic_write_checked, read_file_bounded};
use crate::file_mutation_queue::with_file_mutation_queue_key;
use crate::undo_edit::{EditUndoStore, FileSnapshot, UNDO_UNAVAILABLE_WARNING};

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
        // Bound to the path the security inspector approved, not to whatever
        // the path happens to resolve to now.
        let inspected_target = crate::core::fs::inspected_target(&params, &full_path).await;
        // The requested path passed policy, but a dangling symlink can resolve
        // to a target outside the allowed roots. Validate the target itself.
        self.path_policy.validate(&inspected_target)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();
        let queue_target = inspected_target.clone();

        with_file_mutation_queue_key(&queue_target, |queue_key| async move {
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            let current_target = crate::core::fs::resolve_write_target(&full_path).await;
            if !crate::core::fs::targets_match(&current_target, &inspected_target) {
                return Err(format!(
                    "Refusing to write {}: target changed since it was inspected",
                    path_str
                ));
            }

            if let Ok(meta) = tokio::fs::metadata(&full_path).await {
                if meta.is_dir() {
                    return Err(format!("Path is a directory: {}", path_str));
                }
            }

            if let Some(parent) = full_path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("Failed to create directories for {}: {}", path_str, e))?;
                }
            }

            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            // Snapshot must be captured before the replacement, but only
            // recorded after the atomic write succeeds. Stat first so huge
            // files are not read into RAM just to be rejected by push_snapshot.
            let mut undo_unavailable = false;
            let snapshot = if undo_store.is_some() {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                match tokio::fs::metadata(&full_path).await {
                    Ok(meta) if meta.len() > crate::undo_edit::MAX_SNAPSHOT_BYTES as u64 => {
                        undo_unavailable = true;
                        None
                    }
                    Ok(_) => match read_file_bounded(&full_path, crate::undo_edit::MAX_SNAPSHOT_BYTES as u64).await {
                        Ok(Some(bytes)) => Some(FileSnapshot::Existed {
                            bytes,
                            timestamp_ms,
                            target: inspected_target.clone(),
                        }),
                        Ok(None) => {
                            undo_unavailable = true;
                            None
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Some(FileSnapshot::DidNotExist {
                                target: inspected_target.clone(),
                            })
                        }
                        Err(e) => {
                            return Err(format!(
                                "Failed to read {} for undo snapshot: {}",
                                path_str, e
                            ));
                        }
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Some(FileSnapshot::DidNotExist {
                            target: inspected_target.clone(),
                        })
                    }
                    Err(e) => {
                        return Err(format!(
                            "Failed to stat {} for undo snapshot: {}",
                            path_str, e
                        ));
                    }
                }
            } else {
                None
            };

            atomic_write_checked(&full_path, &inspected_target, content.as_bytes()).await?;

            if let Some(store) = undo_store.as_ref() {
                if let Some(snapshot) = snapshot {
                    if !store.push_snapshot_with_key(queue_key.clone(), snapshot) {
                        undo_unavailable = true;
                    }
                }
                if undo_unavailable {
                    store.invalidate_file_with_key(&queue_key);
                }
            }

            let byte_count = content.len();
            let line_count = content.lines().count();
            let warning = if undo_unavailable {
                format!("\n\n{}", UNDO_UNAVAILABLE_WARNING)
            } else {
                String::new()
            };

            Ok(AgentToolResult::text(format!(
                "Successfully wrote {} bytes ({} lines) to {}{}",
                byte_count, line_count, path_str, warning
            )))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_refuses_target_mismatch_before_creating_parent_dirs() {
        let dir = crate::TempTestDir::new("write_target_mismatch");
        let missing_parent = dir.path().join("nested_missing");
        let file = missing_parent.join("file.txt");
        let other = dir.path().join("other.txt");

        let tool = WriteTool::new(dir.path().to_path_buf());
        let mut params = serde_json::Map::new();
        params.insert("path".into(), json!("nested_missing/file.txt"));
        params.insert("content".into(), json!("data"));
        params.insert(
            mehh_agent::INSPECTED_TARGET_ARG.into(),
            json!(other.to_string_lossy()),
        );

        let err = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("changed since it was inspected"), "got: {err}");
        assert!(
            !missing_parent.exists(),
            "parent directory must not be created when target mismatches"
        );
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn write_over_large_file_skips_snapshot_and_warns_undo_unavailable() {
        let dir = crate::TempTestDir::new("write_oversized_stat");
        let file = dir.path().join("huge.bin");
        {
            let f = std::fs::File::create(&file).unwrap();
            f.set_len(crate::undo_edit::MAX_SNAPSHOT_BYTES as u64 + 1024)
                .unwrap();
        }

        let store = Arc::new(EditUndoStore::new());
        let tool = WriteTool::new(dir.path().to_path_buf()).with_undo_store(store.clone());
        let params = json!({ "path": "huge.bin", "content": "small" })
            .as_object()
            .unwrap()
            .clone();

        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("undo is unavailable"), "got: {text}");
        assert!(!store.has_snapshot(&file));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "small");
    }

    #[tokio::test]
    async fn write_small_then_oversized_then_overwrite_invalidates_stale_snapshot() {
        let dir = crate::TempTestDir::new("write_c15_stale_snapshot");
        let file = dir.path().join("f.txt");
        let store = Arc::new(EditUndoStore::new());
        let write_tool = WriteTool::new(dir.path().to_path_buf()).with_undo_store(store.clone());

        let p1 = json!({ "path": "f.txt", "content": "small" })
            .as_object()
            .unwrap()
            .clone();
        write_tool
            .execute("tc1", p1, CancellationToken::new(), None)
            .await
            .unwrap();

        let huge = "x".repeat(crate::undo_edit::MAX_SNAPSHOT_BYTES + 128);
        let p2 = json!({ "path": "f.txt", "content": huge })
            .as_object()
            .unwrap()
            .clone();
        write_tool
            .execute("tc2", p2, CancellationToken::new(), None)
            .await
            .unwrap();

        let p3 = json!({ "path": "f.txt", "content": "after" })
            .as_object()
            .unwrap()
            .clone();
        let res3 = write_tool
            .execute("tc3", p3, CancellationToken::new(), None)
            .await
            .unwrap();
        let text3 = match &res3.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text3.contains("undo is unavailable"), "got: {text3}");

        let undo = crate::UndoEditTool::new(dir.path().to_path_buf(), store);
        let up = json!({ "path": "f.txt" }).as_object().unwrap().clone();
        let err = undo
            .execute("tc_undo", up, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.to_lowercase().contains("unavailable"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    }

    #[tokio::test]
    async fn write_rejects_existing_directory_upfront() {
        let dir = crate::TempTestDir::new("write_dir_reject");
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        let store = Arc::new(EditUndoStore::new());
        let tool = WriteTool::new(dir.path().to_path_buf()).with_undo_store(store);
        let params = json!({ "path": "subdir", "content": "data" })
            .as_object()
            .unwrap()
            .clone();
        let err = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("Path is a directory"), "got: {err}");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["subdir".to_string()]);
    }
}
