use std::path::{Path, PathBuf};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct EditTool {
    pub cwd: PathBuf,
}

impl EditTool {
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

#[derive(Debug, Clone)]
struct SingleEdit {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone)]
struct MatchedEdit {
    index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

fn parse_edits(params: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<SingleEdit>, String> {
    let mut edits = Vec::new();

    if let Some(edits_val) = params.get("edits") {
        let items: Vec<serde_json::Value> = if let Some(arr) = edits_val.as_array() {
            arr.clone()
        } else if let Some(s) = edits_val.as_str() {
            serde_json::from_str(s).unwrap_or_default()
        } else {
            Vec::new()
        };

        for item in items {
            let old_text = item
                .get("oldText")
                .or_else(|| item.get("old_text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let new_text = item
                .get("newText")
                .or_else(|| item.get("new_text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            edits.push(SingleEdit { old_text, new_text });
        }
    }

    if edits.is_empty() {
        let old_text = params
            .get("oldText")
            .or_else(|| params.get("old_text"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let new_text = params
            .get("newText")
            .or_else(|| params.get("new_text"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let (Some(old), Some(new)) = (old_text, new_text) {
            edits.push(SingleEdit {
                old_text: old,
                new_text: new,
            });
        }
    }

    if edits.is_empty() {
        return Err("Edit tool requires either 'edits' array or 'oldText' and 'newText' parameters.".to_string());
    }

    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("edits[{}].oldText must not be empty.", i));
        }
    }

    Ok(edits)
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text. Every edits[].oldText must match a unique, non-overlapping region of the file."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact text to replace (for single edit)"
                },
                "newText": {
                    "type": "string",
                    "description": "Replacement text (for single edit)"
                },
                "edits": {
                    "type": "array",
                    "description": "List of targeted replacements",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text to replace"
                            },
                            "newText": {
                                "type": "string",
                                "description": "Replacement text"
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
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

        let edits = parse_edits(&params)?;

        let full_path = self.resolve_path(path_str);
        if !full_path.exists() {
            return Err(format!("File not found: {}", path_str));
        }

        let raw_bytes = tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path_str, e))?;

        let raw_content = String::from_utf8(raw_bytes)
            .map_err(|e| format!("File {} is not valid UTF-8: {}", path_str, e))?;

        let is_crlf = raw_content.contains("\r\n");
        let normalized_content = raw_content.replace("\r\n", "\n");

        let mut matched_edits: Vec<MatchedEdit> = Vec::with_capacity(edits.len());

        for (i, edit) in edits.iter().enumerate() {
            let old_norm = edit.old_text.replace("\r\n", "\n");
            let new_norm = edit.new_text.replace("\r\n", "\n");

            let matches: Vec<(usize, &str)> = normalized_content.match_indices(&old_norm).collect();

            if matches.is_empty() {
                return Err(format!(
                    "Could not find edits[{}] in {}. The oldText must match exactly including all whitespace and newlines.",
                    i, path_str
                ));
            }

            if matches.len() > 1 {
                return Err(format!(
                    "Found {} occurrences of edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
                    matches.len(), i, path_str
                ));
            }

            let start = matches[0].0;
            let end = start + old_norm.len();

            matched_edits.push(MatchedEdit {
                index: i,
                start,
                end,
                new_text: new_norm,
            });
        }

        matched_edits.sort_by_key(|m| m.start);
        for i in 1..matched_edits.len() {
            let prev = &matched_edits[i - 1];
            let curr = &matched_edits[i];
            if prev.end > curr.start {
                return Err(format!(
                    "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
                    prev.index, curr.index, path_str
                ));
            }
        }

        let mut new_normalized = String::with_capacity(normalized_content.len());
        let mut last_idx = 0;
        for m in &matched_edits {
            new_normalized.push_str(&normalized_content[last_idx..m.start]);
            new_normalized.push_str(&m.new_text);
            last_idx = m.end;
        }
        new_normalized.push_str(&normalized_content[last_idx..]);

        if new_normalized == normalized_content {
            return Err(format!(
                "No changes made to {}. Replacement produced identical content.",
                path_str
            ));
        }

        let final_content = if is_crlf {
            new_normalized.replace('\n', "\r\n")
        } else {
            new_normalized
        };

        tokio::fs::write(&full_path, final_content.as_bytes())
            .await
            .map_err(|e| format!("Failed to write {}: {}", path_str, e))?;

        let diff = similar::TextDiff::from_lines(&raw_content, &final_content)
            .unified_diff()
            .context_radius(3)
            .header(path_str, path_str)
            .to_string();

        let details = json!({
            "diff": diff,
            "path": path_str,
        });

        Ok(AgentToolResult {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: format!("Successfully edited {}.\n\n{}", path_str, diff),
                text_signature: None,
            })],
            details: Some(details),
        })
    }
}
