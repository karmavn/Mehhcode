use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
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

impl TodoState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, content: String) -> usize {
        let id = self.next_id;
        self.next_id += 1;
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
        self.next_id = new_items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        self.items = new_items;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 1;
    }

    pub fn format_moim_block(&self, cwd: &Path, trusted: bool) -> Option<String> {
        let persistent_memory = if trusted {
            let moim_path = cwd.join(".mehh").join("MOIM.md");
            if let Ok(raw) = std::fs::read_to_string(&moim_path) {
                let clamped = if raw.len() > 8192 {
                    let mut end = 8192;
                    while end > 0 && !raw.is_char_boundary(end) {
                        end -= 1;
                    }
                    raw[..end].to_string()
                } else {
                    raw
                };
                Some(clamped)
            } else {
                None
            }
        } else {
            None
        };

        if self.items.is_empty() && persistent_memory.as_ref().is_none_or(|s| s.trim().is_empty()) {
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
                    item.content,
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
                block.push_str(trimmed);
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

                let status = params.get("status").and_then(|v| v.as_str()).map(|s| {
                    match s {
                        "in_progress" => TodoStatus::InProgress,
                        "completed" => TodoStatus::Completed,
                        _ => TodoStatus::Pending,
                    }
                });

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

                let mut new_items = Vec::new();
                for (idx, item_val) in items_val.iter().enumerate() {
                    let id = item_val
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .map(|u| u as usize)
                        .unwrap_or(idx + 1);
                    let content = item_val
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = item_val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| match s {
                            "in_progress" => TodoStatus::InProgress,
                            "completed" => TodoStatus::Completed,
                            _ => TodoStatus::Pending,
                        })
                        .unwrap_or(TodoStatus::Pending);

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
