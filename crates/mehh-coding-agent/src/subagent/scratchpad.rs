use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};

const SCRATCHPAD_MAX_KEYS: usize = 64;
const SCRATCHPAD_MAX_VALUE_CHARS: usize = 4096;

/// Publishes a short note to the batch scratchpad. Memory-only: no workspace
/// side effects, hence annotated read-only so research/review children get it.
pub(super) struct NoteWriteTool {
    pub(super) pad: super::Scratchpad,
}

#[async_trait]
impl AgentTool for NoteWriteTool {
    fn name(&self) -> &str {
        "note_write"
    }
    fn description(&self) -> &str {
        "Save a short note (key, value) to the scratchpad shared with sibling tasks in this batch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Note identifier." },
                "value": { "type": "string", "description": "Short finding to share (truncated to 4096 chars)." }
            },
            "required": ["key", "value"]
        })
    }
    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'key' parameter".to_string())?;
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required 'value' parameter".to_string())?;
        let value: String = value.chars().take(SCRATCHPAD_MAX_VALUE_CHARS).collect();
        let mut pad = self.pad.write().await;
        if !pad.contains_key(key) && pad.len() >= SCRATCHPAD_MAX_KEYS {
            return Err(format!("Scratchpad full ({SCRATCHPAD_MAX_KEYS} keys)"));
        }
        pad.insert(key.to_string(), value);
        Ok(AgentToolResult::text(format!("note '{key}' saved")))
    }
}

/// Reads a note from the batch scratchpad.
pub(super) struct NoteReadTool {
    pub(super) pad: super::Scratchpad,
}

#[async_trait]
impl AgentTool for NoteReadTool {
    fn name(&self) -> &str {
        "note_read"
    }
    fn description(&self) -> &str {
        "Read a note by key from the scratchpad shared with sibling tasks in this batch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Note identifier." }
            },
            "required": ["key"]
        })
    }
    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'key' parameter".to_string())?;
        let pad = self.pad.read().await;
        match pad.get(key) {
            Some(v) => Ok(AgentToolResult::text(v.clone())),
            None => Ok(AgentToolResult::text(format!("(no note for key '{key}')"))),
        }
    }
}
