use std::path::{Path, PathBuf};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct LsTool {
    pub cwd: PathBuf,
}

impl LsTool {
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
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return (default: 500)"
                }
            },
            "required": []
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
            .unwrap_or(".");

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
            .max(1) as usize;

        let dir_path = self.resolve_path(path_str);
        if !dir_path.exists() {
            return Err(format!("Path not found: {}", path_str));
        }

        if !dir_path.is_dir() {
            return Err(format!("Not a directory: {}", path_str));
        }

        let read_dir = std::fs::read_dir(&dir_path)
            .map_err(|e| format!("Cannot read directory: {}", e))?;

        let mut entries = Vec::new();
        for entry_res in read_dir {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

            if is_dir {
                entries.push(format!("{}/", name));
            } else {
                entries.push(name);
            }
        }

        if entries.is_empty() {
            return Ok(AgentToolResult::text("(empty directory)"));
        }

        entries.sort_by_key(|a| a.to_lowercase());

        let limit_reached = entries.len() > limit;
        if limit_reached {
            entries.truncate(limit);
        }

        let mut output = entries.join("\n");
        if limit_reached {
            output.push_str(&format!(
                "\n\n[{} entries limit reached. Use limit={} for more]",
                limit,
                limit * 2
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}
