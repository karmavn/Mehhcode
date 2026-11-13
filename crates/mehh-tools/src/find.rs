use std::path::{Path, PathBuf};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct FindTool {
    pub cwd: PathBuf,
}

impl FindTool {
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
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern or name (respects .gitignore)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files, e.g. '*.rs' or 'src/**/*.rs'"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 200)"
                }
            },
            "required": ["pattern"]
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
        let pattern_str = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'pattern'".to_string())?;

        let search_path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .max(1) as usize;

        let search_path = self.resolve_path(search_path_str);
        if !search_path.exists() {
            return Err(format!("Path not found: {}", search_path_str));
        }

        let has_glob_magic = pattern_str.chars().any(|c| matches!(c, '*' | '?' | '[' | '{'));

        let glob_matcher = {
            let pat = if !pattern_str.contains('/') && !pattern_str.contains('\\') {
                format!("**/{}", pattern_str)
            } else {
                pattern_str.to_string()
            };
            globset::GlobBuilder::new(&pat)
                .case_insensitive(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        };

        let mut walker = ignore::WalkBuilder::new(&search_path);
        walker.hidden(false);
        walker.git_ignore(true);

        let mut results = Vec::new();

        for entry_res in walker.build() {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.depth() == 0 {
                continue;
            }

            let file_path = entry.path();
            let rel_path = file_path
                .strip_prefix(&search_path)
                .unwrap_or(file_path);
            let mut rel_str = rel_path.to_string_lossy().replace('\\', "/");
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
            if is_dir && !rel_str.ends_with('/') {
                rel_str.push('/');
            }

            let file_name = file_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            let mut matched = false;
            if let Some(ref matcher) = glob_matcher {
                if matcher.is_match(&rel_str) || matcher.is_match(file_name) {
                    matched = true;
                }
            }

            if !matched && !has_glob_magic {
                let pat_lower = pattern_str.to_lowercase();
                if file_name.to_lowercase().contains(&pat_lower)
                    || rel_str.to_lowercase().contains(&pat_lower)
                {
                    matched = true;
                }
            }

            if matched {
                results.push(rel_str);
                if results.len() >= limit {
                    break;
                }
            }
        }

        if results.is_empty() {
            return Ok(AgentToolResult::text("No files found"));
        }

        results.sort_by_key(|a| a.to_lowercase());

        let mut output = results.join("\n");
        if results.len() >= limit {
            output.push_str(&format!("\n\n[Reached limit of {} results]", limit));
        }

        Ok(AgentToolResult::text(output))
    }
}
