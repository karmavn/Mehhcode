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

const FIND_MAX_OUTPUT_BYTES: usize = 100 * 1024;

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
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let pattern_str = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'pattern'".to_string())?
            .to_string();

        let search_path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .max(1) as usize;

        let search_path = self.resolve_path(&search_path_str);
        let cancel_clone = cancel.clone();

        tokio::task::spawn_blocking(move || {
            if !search_path.exists() {
                return Err(format!("Path not found: {}", search_path_str));
            }

            let has_glob_magic = pattern_str.chars().any(|c| matches!(c, '*' | '?' | '[' | '{'));

            let pat = if !pattern_str.contains('/') && !pattern_str.contains('\\') {
                format!("**/{}", pattern_str)
            } else {
                pattern_str.clone()
            };
            let glob_builder = globset::GlobBuilder::new(&pat)
                .case_insensitive(true)
                .build();
            let glob_matcher = if has_glob_magic {
                let g = glob_builder
                    .map_err(|e| format!("Invalid glob pattern '{}': {}", pattern_str, e))?;
                Some(g.compile_matcher())
            } else {
                glob_builder.ok().map(|g| g.compile_matcher())
            };

            let mut walker = ignore::WalkBuilder::new(&search_path);
            walker.hidden(false);
            walker.git_ignore(true);
            walker.filter_entry(|e| e.file_name().to_str() != Some(".git"));

            let mut results = Vec::new();
            let mut total_bytes = 0usize;
            let mut byte_cap_reached = false;
            let pat_lower = if !has_glob_magic {
                Some(pattern_str.to_lowercase())
            } else {
                None
            };

            for entry_res in walker.build() {
                if cancel_clone.is_cancelled() {
                    return Err("Tool execution cancelled".to_string());
                }

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

                if !matched {
                    if let Some(ref pat_lower) = pat_lower {
                        if file_name.to_lowercase().contains(pat_lower)
                            || rel_str.to_lowercase().contains(pat_lower)
                        {
                            matched = true;
                        }
                    }
                }

                if matched {
                    let add_bytes = rel_str.len().saturating_add(1);
                    if total_bytes.saturating_add(add_bytes) > FIND_MAX_OUTPUT_BYTES {
                        byte_cap_reached = true;
                        break;
                    }
                    total_bytes += add_bytes;
                    results.push(rel_str);
                    if results.len() >= limit {
                        break;
                    }
                }
            }

            if results.is_empty() {
                return Ok(AgentToolResult::text("No files found"));
            }

            results.sort_by_cached_key(|a| a.to_lowercase());

            let mut output = results.join("\n");
            if byte_cap_reached {
                output.push_str(&format!(
                    "\n\n[Output truncated at {} byte limit]",
                    FIND_MAX_OUTPUT_BYTES
                ));
            } else if results.len() >= limit {
                output.push_str(&format!("\n\n[Reached limit of {} results]", limit));
            }

            Ok(AgentToolResult::text(output))
        })
        .await
        .map_err(|e| format!("Find task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_find_skips_git_dir_and_honors_cancel() {
        let dir = crate::TempTestDir::new("find_git");
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let tool = FindTool::new(dir.path().to_path_buf());
        let params = json!({ "pattern": "*" }).as_object().unwrap().clone();
        let res = tool
            .execute("tc", params.clone(), CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("main.rs"));
        assert!(!txt.contains(".git"));
        assert!(!txt.contains("HEAD"));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(tool.execute("tc2", params, cancelled, None).await.is_err());

        let bad_glob = json!({ "pattern": "[abc" }).as_object().unwrap().clone();
        let err = tool
            .execute("tc3", bad_glob, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("Invalid glob"), "unexpected err: {err}");
    }
}
