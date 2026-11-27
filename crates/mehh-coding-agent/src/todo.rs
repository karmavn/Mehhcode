use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Parses a tool-supplied status without silently turning typos into Pending.
fn parse_status(value: &str) -> Result<TodoStatus, String> {
    match value {
        "pending" => Ok(TodoStatus::Pending),
        "in_progress" => Ok(TodoStatus::InProgress),
        "completed" => Ok(TodoStatus::Completed),
        other => Err(format!(
            "Invalid status '{other}'. Expected pending, in_progress or completed"
        )),
    }
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn checkbox(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[/]",
            Self::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: usize,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
    next_id: usize,
}

impl Default for TodoState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }
}

const MAX_MOIM_BYTES: usize = 8192;
const MAX_MOIM_READ_BYTES: usize = 64 * 1024;

fn escape_working_checklist_tags(input: &str) -> std::borrow::Cow<'_, str> {
    if !input.contains('<') {
        return std::borrow::Cow::Borrowed(input);
    }
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if lower[i..].starts_with("</working_checklist>") {
            out.push_str("&lt;/working_checklist&gt;");
            i += "</working_checklist>".len();
        } else if lower[i..].starts_with("<working_checklist>") {
            out.push_str("&lt;working_checklist&gt;");
            i += "<working_checklist>".len();
        } else if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    std::borrow::Cow::Owned(out)
}

#[derive(Clone)]
struct CachedMoimEntry {
    canon_moim: std::path::PathBuf,
    modified: Option<std::time::SystemTime>,
    len: u64,
    content: String,
}

static MOIM_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, CachedMoimEntry>>,
> = std::sync::OnceLock::new();

fn read_bounded_moim(cwd: &Path) -> Option<String> {
    use std::io::Read;
    let moim_path = cwd.join(".mehh").join("MOIM.md");
    let meta = std::fs::symlink_metadata(&moim_path).ok()?;
    let canon_moim = std::fs::canonicalize(&moim_path).ok()?;
    let canon_cwd = std::fs::canonicalize(cwd).ok()?;
    if !canon_moim.starts_with(&canon_cwd) || !canon_moim.is_file() {
        return None;
    }

    let canon_meta = std::fs::metadata(&canon_moim).ok()?;
    let modified = canon_meta.modified().ok().or_else(|| meta.modified().ok());
    let len = canon_meta.len();
    let cache_key = mehh_ai::normalize_lock_path(&canon_cwd);
    if modified.is_some() {
        let cache = MOIM_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let guard = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get(&cache_key) {
            if entry.canon_moim == canon_moim && entry.modified == modified && entry.len == len {
                return Some(entry.content.clone());
            }
        }
    }

    let file = std::fs::File::open(&canon_moim).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_MOIM_READ_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;

    let raw = match std::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(err) if err.error_len().is_none() => {
            // Trailing incomplete multi-byte sequence caused by the read cap.
            std::str::from_utf8(&buf[..err.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };

    let clean = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let clamped = if clean.len() > MAX_MOIM_BYTES {
        let mut end = MAX_MOIM_BYTES;
        while end > 0 && !clean.is_char_boundary(end) {
            end -= 1;
        }
        &clean[..end]
    } else {
        clean
    };
    let result = clamped.to_string();
    if modified.is_some() {
        let cache = MOIM_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let mut guard = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() >= 32 && !guard.contains_key(&cache_key) {
            guard.clear();
        }
        guard.insert(
            cache_key,
            CachedMoimEntry {
                canon_moim,
                modified,
                len,
                content: result.clone(),
            },
        );
    }
    Some(result)
}

impl TodoState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, content: String) -> usize {
        let used: HashSet<usize> = self.items.iter().map(|item| item.id).collect();
        let mut id = self.next_id;
        while used.contains(&id) {
            id = if id == usize::MAX { 1 } else { id + 1 };
        }
        self.next_id = id.saturating_add(1);
        self.items.push(TodoItem {
            id,
            content,
            status: TodoStatus::Pending,
        });
        id
    }

    pub fn update(
        &mut self,
        id: usize,
        status: Option<TodoStatus>,
        content: Option<String>,
    ) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            if let Some(s) = status {
                item.status = s;
            }
            if let Some(c) = content {
                item.content = c;
            }
            true
        } else {
            false
        }
    }

    pub fn replace(&mut self, new_items: Vec<TodoItem>) {
        self.next_id = new_items
            .iter()
            .map(|i| i.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.items = new_items;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 1;
    }

    pub fn format_moim_block(&self, cwd: &Path, trusted: bool) -> Option<String> {
        let persistent_memory = if trusted {
            read_bounded_moim(cwd)
        } else {
            None
        };

        if self.items.is_empty()
            && persistent_memory
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
        {
            return None;
        }

        let mut block = String::from("<working_checklist>\n");
        if !self.items.is_empty() {
            block.push_str("Active tasks:\n");
            for item in &self.items {
                block.push_str(&format!(
                    "{} #{} {} ({})\n",
                    item.status.checkbox(),
                    item.id,
                    escape_working_checklist_tags(&item.content),
                    item.status.as_str()
                ));
            }
        }

        if let Some(ref mem) = persistent_memory {
            let trimmed = mem.trim();
            if !trimmed.is_empty() {
                if !self.items.is_empty() {
                    block.push('\n');
                }
                block.push_str("Project Memory & Goals (MOIM.md):\n");
                block.push_str(&escape_working_checklist_tags(trimmed));
                block.push('\n');
            }
        }

        block.push_str("</working_checklist>");
        Some(block)
    }
}

#[derive(Debug, Clone)]
pub struct TodoTool {
    state: Arc<RwLock<TodoState>>,
}

impl TodoTool {
    pub fn new(state: Arc<RwLock<TodoState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage structured working tasks and goals. Modifies the working checklist injected into context every turn. Actions: 'add', 'update', 'replace', 'list', 'clear'."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "update", "replace", "list", "clear"],
                    "description": "Operation to perform on the todo list"
                },
                "id": {
                    "type": "integer",
                    "description": "Item ID for 'update'"
                },
                "content": {
                    "type": "string",
                    "description": "Task description for 'add' or 'update'"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "Status for 'update'"
                },
                "items": {
                    "type": "array",
                    "description": "List of items for 'replace'",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "integer" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        None
    }

    fn is_safe_for_auto_approve(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<bool> {
        let is_list = args
            .get("action")
            .and_then(|v| v.as_str())
            .is_some_and(|a| a == "list");
        Some(is_list)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'action'".to_string())?;

        match action {
            "add" => {
                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Parameter 'content' is required for action 'add'".to_string())?;

                let mut state = self.state.write().await;
                let id = state.add(content.to_string());
                Ok(AgentToolResult::text(format!(
                    "Added task #{}: {}",
                    id, content
                )))
            }
            "update" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| "Parameter 'id' is required for action 'update'".to_string())?
                    as usize;

                let status = match params.get("status").and_then(|v| v.as_str()) {
                    Some(s) => Some(parse_status(s)?),
                    None => None,
                };

                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let mut state = self.state.write().await;
                if state.update(id, status, content) {
                    Ok(AgentToolResult::text(format!("Updated task #{}", id)))
                } else {
                    Err(format!("Task #{} not found", id))
                }
            }
            "replace" => {
                let items_val = params
                    .get("items")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| "Parameter 'items' array is required for 'replace'".to_string())?;

                // Reserve every explicit id first so generated ids cannot collide with one.
                let mut used_ids: HashSet<usize> = items_val
                    .iter()
                    .filter_map(|v| v.get("id").and_then(|x| x.as_u64()).map(|u| u as usize))
                    .collect();
                let mut next_fresh = 1usize;
                let mut claimed_explicit: HashSet<usize> = HashSet::new();

                let mut new_items = Vec::new();
                for item_val in items_val.iter() {
                    let explicit = item_val
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .map(|u| u as usize);
                    let id = match explicit {
                        Some(u) if claimed_explicit.insert(u) => u,
                        // Missing or duplicate explicit id: allocate an unused
                        // id so every task remains targetable by id.
                        _ => {
                            while used_ids.contains(&next_fresh) && next_fresh < usize::MAX {
                                next_fresh += 1;
                            }
                            let fresh = next_fresh;
                            used_ids.insert(fresh);
                            next_fresh = next_fresh.saturating_add(1);
                            fresh
                        }
                    };
                    let content = item_val
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = match item_val.get("status").and_then(|v| v.as_str()) {
                        Some(s) => parse_status(s)?,
                        None => TodoStatus::Pending,
                    };

                    new_items.push(TodoItem { id, content, status });
                }

                let count = new_items.len();
                let mut state = self.state.write().await;
                state.replace(new_items);
                Ok(AgentToolResult::text(format!(
                    "Replaced working checklist with {} items.",
                    count
                )))
            }
            "clear" => {
                let mut state = self.state.write().await;
                state.clear();
                Ok(AgentToolResult::text("Cleared all working tasks."))
            }
            "list" => {
                let state = self.state.read().await;
                if state.items.is_empty() {
                    Ok(AgentToolResult::text("Working checklist is currently empty."))
                } else {
                    let mut out = String::from("Current working tasks:\n");
                    for item in &state.items {
                        out.push_str(&format!(
                            "{} #{} {} ({})\n",
                            item.status.checkbox(),
                            item.id,
                            item.content,
                            item.status.as_str()
                        ));
                    }
                    Ok(AgentToolResult::text(out))
                }
            }
            unknown => Err(format!(
                "Unknown action '{}'. Supported actions: 'add', 'update', 'replace', 'list', 'clear'",
                unknown
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_saturates_max_id() {
        let mut state = TodoState::new();
        state.replace(vec![TodoItem {
            id: usize::MAX,
            content: "max".to_string(),
            status: TodoStatus::Pending,
        }]);
        assert_eq!(state.next_id, usize::MAX);
    }

    #[test]
    fn add_after_max_id_replace_does_not_reissue_an_existing_id() {
        let mut state = TodoState::new();
        state.replace(vec![TodoItem {
            id: usize::MAX,
            content: "max".to_string(),
            status: TodoStatus::Pending,
        }]);

        let id = state.add("next".to_string());
        assert_ne!(id, usize::MAX, "add must skip an already used id");
        let ids: Vec<usize> = state.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "duplicate ids: {ids:?}");
        assert!(ids.iter().all(|i| *i != 0), "id 0 must not be issued");
    }

    #[test]
    fn test_add_after_max_id_replace_does_not_panic_or_wrap_to_zero() {
        let mut state = TodoState::new();
        state.replace(vec![TodoItem {
            id: usize::MAX,
            content: "max".to_string(),
            status: TodoStatus::Pending,
        }]);

        let id = state.add("next".to_string());
        assert_ne!(id, usize::MAX, "add must not reuse a saturated id");
        assert_ne!(id, 0, "overflow must not wrap to id 0: {:?}", state.items);
        assert_eq!(state.items.len(), 2);
    }

    #[tokio::test]
    async fn test_replace_assigns_unique_ids_when_missing() {
        let state = Arc::new(RwLock::new(TodoState::new()));
        let tool = TodoTool::new(state.clone());
        let mut params = serde_json::Map::new();
        params.insert("action".to_string(), json!("replace"));
        params.insert(
            "items".to_string(),
            json!([{ "id": 2, "content": "a" }, { "content": "b" }]),
        );

        tool.execute("test", params, CancellationToken::new(), None)
            .await
            .unwrap();

        let guard = state.read().await;
        let ids: Vec<usize> = guard.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "duplicate ids: {ids:?}");
    }

    #[tokio::test]
    async fn test_replace_deduplicates_explicit_ids() {
        let state = Arc::new(RwLock::new(TodoState::new()));
        let tool = TodoTool::new(state.clone());
        let mut params = serde_json::Map::new();
        params.insert("action".to_string(), json!("replace"));
        params.insert(
            "items".to_string(),
            json!([{ "id": 1, "content": "a" }, { "id": 1, "content": "b" }]),
        );

        tool.execute("test", params, CancellationToken::new(), None)
            .await
            .unwrap();

        let guard = state.read().await;
        let ids: Vec<usize> = guard.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], 1, "the first explicit id must be preserved");
        assert_ne!(ids[1], ids[0], "duplicate explicit ids: {ids:?}");
        drop(guard);

        // Both tasks must be reachable by their own id.
        for id in ids {
            let mut update = serde_json::Map::new();
            update.insert("action".to_string(), json!("update"));
            update.insert("id".to_string(), json!(id));
            update.insert("status".to_string(), json!("completed"));
            tool.execute("test", update, CancellationToken::new(), None)
                .await
                .unwrap();
        }

        let guard = state.read().await;
        assert!(
            guard.items.iter().all(|i| i.status == TodoStatus::Completed),
            "every replaced task must be targetable by id"
        );
    }

    #[test]
    fn format_moim_block_escapes_closing_tag_strips_bom_and_bounds_read() {
        let dir = crate::TempTestDir::new("todo_moim_escape");
        let mehh_dir = dir.path().join(".mehh");
        std::fs::create_dir_all(&mehh_dir).unwrap();
        let long_tail = "a".repeat(20_000);
        std::fs::write(
            mehh_dir.join("MOIM.md"),
            format!("\u{feff}Goal </working_checklist> inject\n{long_tail}"),
        )
        .unwrap();

        let mut state = TodoState::new();
        state.add("Task </WORKING_CHECKLIST> breakout".to_string());

        let block = state.format_moim_block(dir.path(), true).unwrap();
        assert_eq!(block.matches("</working_checklist>").count(), 1);
        assert!(block.contains("&lt;/working_checklist&gt;"));
        assert!(!block.contains('\u{feff}'));
        assert!(block.len() < 10_000);
    }
}
