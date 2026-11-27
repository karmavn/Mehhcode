use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write_checked;
use crate::core::output::truncate_head;
use crate::file_mutation_queue::{get_mutation_queue_key, with_file_mutation_queue_key};

const MAX_DIFF_BYTES: usize = 64 * 1024;
const MAX_SNAPSHOTS_PER_FILE: usize = 20;
const MAX_TOTAL_UNDO_ENTRIES: usize = 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL_UNDO_BYTES: usize = 16 * 1024 * 1024;

/// Appended to edit/write results when no snapshot could be recorded, so the
/// caller is not left believing `undo_edit` will be able to revert the change.
pub(crate) const UNDO_UNAVAILABLE_WARNING: &str = "Warning: undo is unavailable for this change (the previous content was too large to snapshot); undo_edit cannot revert it.";

#[derive(Debug, Clone)]
pub enum FileSnapshot {
    Existed {
        bytes: Vec<u8>,
        timestamp_ms: i64,
        /// Target approved when the snapshot was taken. Without it a later
        /// symlink swap would make undo write the old bytes into a new target.
        target: PathBuf,
    },
    DidNotExist {
        /// Target approved when the creation was recorded.
        target: PathBuf,
    },
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
    order: VecDeque<String>,
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

    /// Records a snapshot, returning `true` only when it was actually stored.
    /// Returns `false` when the snapshot exceeds [`MAX_SNAPSHOT_BYTES`], so
    /// callers can warn that undo is unavailable instead of reporting success.
    pub fn push_snapshot(&self, path: &Path, snapshot: FileSnapshot) -> bool {
        let key = get_mutation_queue_key(path);
        self.push_snapshot_with_key(key, snapshot)
    }

    pub(crate) fn push_snapshot_with_key(&self, key: String, snapshot: FileSnapshot) -> bool {
        let size = match &snapshot {
            FileSnapshot::Existed { bytes, .. } => {
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return false;
                }
                bytes.len()
            }
            FileSnapshot::DidNotExist { .. } => 0,
        };

        let mut inner = self.lock_inner();

        while inner.order.len() >= MAX_TOTAL_UNDO_ENTRIES
            || inner.total_bytes + size > MAX_TOTAL_UNDO_BYTES
        {
            let Some(oldest_key) = inner.order.pop_front() else {
                break;
            };
            let (evicted_size, became_empty) =
                if let Some(history) = inner.snapshots.get_mut(&oldest_key) {
                    let evicted_size = if !history.is_empty() {
                        history.remove(0).size
                    } else {
                        0
                    };
                    (evicted_size, history.is_empty())
                } else {
                    (0, false)
                };
            inner.total_bytes = inner.total_bytes.saturating_sub(evicted_size);
            if became_empty {
                inner.snapshots.remove(&oldest_key);
            }
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
            if let Some(pos) = inner.order.iter().position(|k| k == &key) {
                inner.order.remove(pos);
            }
        }
        inner.total_bytes += size;
        inner.order.push_back(key);
        true
    }

    /// Read the most recent snapshot without consuming it. Callers that write
    /// first and pop afterwards keep the undo history if the write fails.
    pub fn peek_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        self.peek_snapshot_with_key(&key)
    }

    pub(crate) fn peek_snapshot_with_key(&self, key: &str) -> Option<FileSnapshot> {
        let inner = self.lock_inner();
        inner
            .snapshots
            .get(key)
            .and_then(|history| history.last())
            .map(|entry| entry.snapshot.clone())
    }

    pub fn pop_snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        let key = get_mutation_queue_key(path);
        self.pop_snapshot_with_key(&key)
    }

    pub(crate) fn pop_snapshot_with_key(&self, key: &str) -> Option<FileSnapshot> {
        let mut inner = self.lock_inner();
        let (entry, is_empty) = {
            if let Some(history) = inner.snapshots.get_mut(key) {
                let popped = history.pop();
                let empty = history.is_empty();
                (popped, empty)
            } else {
                (None, false)
            }
        };

        if let Some(entry) = entry {
            inner.total_bytes = inner.total_bytes.saturating_sub(entry.size);
            if let Some(pos) = inner.order.iter().rposition(|k| k == key) {
                inner.order.remove(pos);
            }
            if is_empty {
                inner.snapshots.remove(key);
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

    /// Clears all recorded undo snapshots for `path`, adjusting `total_bytes`
    /// and `order` accordingly.
    pub fn invalidate_file(&self, path: &Path) {
        let key = get_mutation_queue_key(path);
        self.invalidate_file_with_key(&key);
    }

    pub(crate) fn invalidate_file_with_key(&self, key: &str) {
        let mut inner = self.lock_inner();
        if let Some(history) = inner.snapshots.remove(key) {
            let removed_bytes: usize = history.iter().map(|e| e.size).sum();
            inner.total_bytes = inner.total_bytes.saturating_sub(removed_bytes);
            inner.order.retain(|k| k != key);
        }
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
        // Same revalidation as `write`/`edit`: bound to the inspector-approved
        // target so a swapped symlink cannot redirect the restore.
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
                    "Refusing to undo {}: target changed since it was inspected",
                    path_str
                ));
            }

            let snapshot = undo_store
                .peek_snapshot_with_key(&queue_key)
                .ok_or_else(|| {
                    format!(
                        "Undo is unavailable for {}: no previous edit snapshot was recorded (the file may be too large to snapshot)",
                        path_str
                    )
                })?;

            match snapshot {
                FileSnapshot::DidNotExist {
                    target: recorded_target,
                } => {
                    if tokio::fs::try_exists(&current_target)
                        .await
                        .unwrap_or(false)
                    {
                        // A creation through a (possibly dangling) symlink puts
                        // the new file at the resolved target, not at the link.
                        // Remove the target so the link itself survives, but
                        // only if the link still points where it did when the
                        // creation was recorded; otherwise a swapped symlink
                        // would delete an unrelated pre-existing file.
                        if !crate::core::fs::targets_match(&current_target, &recorded_target) {
                            return Err(format!(
                                "Refusing to undo creation of {}: target changed since it was recorded",
                                path_str
                            ));
                        }
                        tokio::fs::remove_file(&current_target)
                            .await
                            .map_err(|e| format!("Failed to remove {}: {}", path_str, e))?;
                    }
                    undo_store.pop_snapshot_with_key(&queue_key);
                    Ok(AgentToolResult::text(format!(
                        "Reverted creation of {}: file deleted.",
                        path_str
                    )))
                }
                FileSnapshot::Existed {
                    bytes: original_bytes,
                    target: recorded_target,
                    ..
                } => {
                    if !crate::core::fs::targets_match(&current_target, &recorded_target) {
                        return Err(format!(
                            "Refusing to undo {}: target changed since the edit was recorded",
                            path_str
                        ));
                    }
                    let current_str = if tokio::fs::try_exists(&current_target)
                        .await
                        .unwrap_or(false)
                    {
                        crate::core::fs::read_file_bounded(
                            &current_target,
                            MAX_TOTAL_UNDO_BYTES as u64,
                        )
                        .await
                        .ok()
                        .flatten()
                        .and_then(|b| String::from_utf8(b).ok())
                    } else {
                        None
                    };

                    atomic_write_checked(&full_path, &inspected_target, &original_bytes).await?;
                    undo_store.pop_snapshot_with_key(&queue_key);

                    let diff = if let Some(curr) = current_str {
                        let path_header = path_str.to_string();
                        tokio::task::spawn_blocking(move || {
                            let restored_str = String::from_utf8_lossy(&original_bytes);
                            let raw_diff =
                                similar::TextDiff::from_lines(curr.as_str(), restored_str.as_ref())
                                    .unified_diff()
                                    .context_radius(3)
                                    .header(&path_header, &path_header)
                                    .to_string();
                            truncate_head(&raw_diff, MAX_DIFF_BYTES, "\n... [diff truncated]")
                        })
                        .await
                        .map_err(|e| format!("Undo diff task failed: {e}"))?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_snapshot_oversized_returns_false_and_does_not_store() {
        let store = EditUndoStore::new();
        let path = Path::new("huge.bin");
        let too_big = vec![0u8; MAX_SNAPSHOT_BYTES + 1];

        let recorded = store.push_snapshot(
            path,
            FileSnapshot::Existed {
                bytes: too_big,
                timestamp_ms: 1,
                target: path.to_path_buf(),
            },
        );

        assert!(
            !recorded,
            "oversized snapshot must report that it was not recorded"
        );
        assert!(
            !store.has_snapshot(path),
            "oversized snapshot must not be stored"
        );
    }

    #[test]
    fn push_snapshot_keeps_order_bounded_when_evicting_per_file_cap() {
        let store = EditUndoStore::new();
        let path = Path::new("repeated.rs");
        for i in 0..(MAX_SNAPSHOTS_PER_FILE + 10) {
            assert!(store.push_snapshot(
                path,
                FileSnapshot::Existed {
                    bytes: vec![i as u8; 100],
                    timestamp_ms: i as i64,
                    target: path.to_path_buf(),
                },
            ));
        }
        let inner = store.lock_inner();
        assert_eq!(inner.order.len(), MAX_SNAPSHOTS_PER_FILE);
        assert_eq!(inner.total_bytes, MAX_SNAPSHOTS_PER_FILE * 100);
    }

    #[tokio::test]
    async fn test_undo_creation_through_dangling_symlink_removes_target_not_link() {
        fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(target, link)
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(target, link)
            }
            #[cfg(not(any(windows, unix)))]
            {
                let _ = (target, link);
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "symlinks are not supported on this platform",
                ))
            }
        }

        let dir = crate::TempTestDir::new("undo_dangling_symlink");
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");

        if let Err(e) = create_file_symlink(&target, &link) {
            eprintln!("skipping dangling symlink undo test: cannot create symlinks ({e})");
            return;
        }

        // The write resolves through the dangling link and creates the target,
        // recording DidNotExist for the requested path.
        std::fs::write(&target, b"new content").unwrap();
        let undo_store = Arc::new(EditUndoStore::new());
        undo_store.push_snapshot(
            &link,
            FileSnapshot::DidNotExist {
                target: target.clone(),
            },
        );

        let tool = UndoEditTool::new(dir.path().to_path_buf(), undo_store);
        let params = json!({ "path": "link.txt" }).as_object().unwrap().clone();
        tool.execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "undo must not delete the symlink"
        );
        assert!(
            !target.exists(),
            "undo must delete the file created through the symlink"
        );
    }

    #[test]
    fn invalidate_file_clears_stack_and_adjusts_total_bytes_and_order() {
        let store = EditUndoStore::new();
        let path_a = Path::new("a.rs");
        let path_b = Path::new("b.rs");

        assert!(store.push_snapshot(
            path_a,
            FileSnapshot::Existed {
                bytes: vec![1u8; 100],
                timestamp_ms: 1,
                target: path_a.to_path_buf(),
            },
        ));
        assert!(store.push_snapshot(
            path_b,
            FileSnapshot::Existed {
                bytes: vec![2u8; 50],
                timestamp_ms: 2,
                target: path_b.to_path_buf(),
            },
        ));
        assert!(store.push_snapshot(
            path_a,
            FileSnapshot::Existed {
                bytes: vec![3u8; 200],
                timestamp_ms: 3,
                target: path_a.to_path_buf(),
            },
        ));

        store.invalidate_file(path_a);

        assert!(!store.has_snapshot(path_a));
        assert!(store.peek_snapshot(path_a).is_none());
        assert!(store.has_snapshot(path_b));
        let inner = store.lock_inner();
        assert_eq!(inner.total_bytes, 50);
        assert_eq!(inner.order.len(), 1);
    }
}
