use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write;
use crate::core::output::truncate_head;
use crate::file_mutation_queue::{get_mutation_queue_key, with_file_mutation_queue};

const MAX_DIFF_BYTES: usize = 64 * 1024;
const MAX_SNAPSHOTS_PER_FILE: usize = 20;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL_UNDO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum FileSnapshot {
    Existed {
        bytes: Vec<u8>,
        timestamp_ms: i64,
    },
    DidNotExist,
}

#[derive(Debug, Clone)]
struct SnapshotEntry {
    snapshot: FileSnapshot,
    size: usize,
}

#[derive(Debug, Default)]
struct EditUndoStoreInner {
    snapshots: HashMap<String, Vec<SnapshotEntry>>,
    total_bytes: usize,
    order: Vec<String>,
}

#[derive(Debug, Default)]
pub struct EditUndoStore {
    inner: Mutex<EditUndoStoreInner>,
}

impl EditUndoStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(EditUndoStoreInner::default()),
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, EditUndoStoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_snapshot(&self, path: &Path, snapshot: FileSnapshot) {
        let size = match &snapshot {
            FileSnapshot::Existed { bytes, .. } => {
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return;
                }
                bytes.len()
            }
            FileSnapshot::DidNotExist => 0,
        };

        let key = get_mutation_queue_key(path);
        let mut inner = self.lock_inner();

        while inner.total_bytes + size > MAX_TOTAL_UNDO_BYTES && !inner.order.is_empty() {
            let oldest_key = inner.order.remove(0);
            if let Some(history) = inner.snapshots.get_mut(&oldest_key) {
                if !history.is_empty() {
                    let evicted = history.remove(0);
                    inner.total_bytes = inner.total_bytes.saturating_sub(evicted.size);
                }
            }
            inner.snapshots.retain(|_, v| !v.is_empty());
        }

        let history = inner.snapshots.entry(key.clone()).or_default();
        let evicted_size = if history.len() >= MAX_SNAPSHOTS_PER_FILE {
            Some(history.remove(0).size)
        } else {
            None
        };
        history.push(SnapshotEntry { snapshot, size });

        if let Some(es) = evicted_size {
            inner.total_bytes = inner.total_bytes.saturating_sub(es);
        }
        inner.total_bytes += size;
        inner.order.push(key);
    }

    /// Read the most recent snapshot without consuming it. Callers that write
    /// first and pop afterwards keep the undo history if the write fails.
    pub fn peek_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        let inner = self.lock_inner();
        inner
            .snapshots
            .get(&key)
            .and_then(|history| history.last())
            .map(|entry| entry.snapshot.clone())
    }

    pub fn pop_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        let mut inner = self.lock_inner();
        let (entry, is_empty) = {
            if let Some(history) = inner.snapshots.get_mut(&key) {
                let popped = history.pop();
                let empty = history.is_empty();
                (popped, empty)
            } else {
                (None, false)
            }
        };

        if let Some(entry) = entry {
            inner.total_bytes = inner.total_bytes.saturating_sub(entry.size);
            if let Some(pos) = inner.order.iter().rposition(|k| k == &key) {
                inner.order.remove(pos);
            }
            if is_empty {
                inner.snapshots.remove(&key);
            }
            Some(entry.snapshot)
        } else {
            None
        }
    }

    pub fn has_snapshot(&self, path: &Path) -> bool {
        let key = get_mutation_queue_key(path);
        let inner = self.lock_inner();
        inner.snapshots.get(&key).is_some_and(|h| !h.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct UndoEditTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Arc<EditUndoStore>,
}

impl UndoEditTool {
    pub fn new(cwd: PathBuf, undo_store: Arc<EditUndoStore>) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store,
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
impl AgentTool for UndoEditTool {
    fn name(&self) -> &str {
        "undo_edit"
    }

    fn description(&self) -> &str {
        "Revert the most recent edit or write applied to a file, restoring its previous byte-accurate content (preserving exact line endings and BOM)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to revert (relative or absolute)"
                }
            },
            "required": ["path"]
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

        let full_path = self.resolve_path(path_str)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();

        with_file_mutation_queue(&full_path, async {
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }

            let snapshot = undo_store
                .peek_snapshot(&full_path)
                .ok_or_else(|| format!("No previous edit history found for {}", path_str))?;

            match snapshot {
                FileSnapshot::DidNotExist => {
                    if full_path.exists() {
                        tokio::fs::remove_file(&full_path)
                            .await
                            .map_err(|e| format!("Failed to remove {}: {}", path_str, e))?;
                    }
                    undo_store.pop_snapshot(&full_path);
                    Ok(AgentToolResult::text(format!(
                        "Reverted creation of {}: file deleted.",
                        path_str
                    )))
                }
                FileSnapshot::Existed { bytes: original_bytes, .. } => {
                    let current_str = if full_path.exists() {
                        tokio::fs::read(&full_path)
                            .await
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok())
                    } else {
                        None
                    };

                    atomic_write(&full_path, &original_bytes).await?;
                    undo_store.pop_snapshot(&full_path);

                    let restored_str = String::from_utf8_lossy(&original_bytes);
                    let diff = if let Some(ref curr) = current_str {
                        let raw_diff =
                            similar::TextDiff::from_lines(curr.as_str(), restored_str.as_ref())
                                .unified_diff()
                                .context_radius(3)
                                .header(path_str, path_str)
                                .to_string();
                        truncate_head(&raw_diff, MAX_DIFF_BYTES, "\n... [diff truncated]")
                    } else {
                        format!("Restored {} bytes", original_bytes.len())
                    };

                    let details = json!({
                        "path": path_str,
                        "diff": diff,
                    });

                    Ok(AgentToolResult {
                        content: vec![ToolResultContentBlock::Text(TextContent {
                            text: format!("Successfully reverted {}.\n\n{}", path_str, diff),
                            text_signature: None,
                        })],
                        details: Some(details),
                        ..Default::default()
                    })
                }
            }
        })
        .await
    }
}
