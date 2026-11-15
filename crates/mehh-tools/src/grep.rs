use std::path::{Path, PathBuf};
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct GrepTool {
    pub cwd: PathBuf,
}

impl GrepTool {
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

fn truncate_line(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    match s.char_indices().nth(max_len) {
        Some((idx, _)) => format!("{}... [truncated]", &s[..idx]),
        None => s.to_string(),
    }
}

fn search_in_file(
    path_display: &str,
    content: &str,
    re: &regex::Regex,
    context: usize,
    remaining_limit: usize,
) -> (Vec<String>, usize) {
    let lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut match_indices = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            match_indices.push(idx);
            if match_indices.len() >= remaining_limit {
                break;
            }
        }
    }

    if match_indices.is_empty() {
        return (Vec::new(), 0);
    }

    let match_count = match_indices.len();
    let mut output_lines = Vec::new();

    if context == 0 {
        for &idx in &match_indices {
            let line_num = idx + 1;
            let line_text = truncate_line(lines[idx], 500);
            output_lines.push(format!("{}:{}:{}", path_display, line_num, line_text));
        }
    } else {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &idx in &match_indices {
            let start = idx.saturating_sub(context);
            let end = idx.saturating_add(context).saturating_add(1).min(lines.len());
            if let Some(last) = ranges.last_mut() {
                if start <= last.1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            ranges.push((start, end));
        }

        let match_set: std::collections::HashSet<usize> = match_indices.iter().copied().collect();

        for (start, end) in ranges {
            for (offset, line) in lines[start..end].iter().enumerate() {
                let idx = start + offset;
                let line_num = idx + 1;
                let line_text = truncate_line(line, 500);
                if match_set.contains(&idx) {
                    output_lines.push(format!("{}:{}:{}", path_display, line_num, line_text));
                } else {
                    output_lines.push(format!("{}-{}-{}", path_display, line_num, line_text));
                }
            }
        }
    }

    (output_lines, match_count)
}

const GREP_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const GREP_MAX_OUTPUT_BYTES: usize = 100 * 1024;

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for patterns (respects .gitignore)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (regex or literal string)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob pattern, e.g. '*.rs' or 'src/**/*.rs'"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default: false)"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as literal string instead of regex (default: false)"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of lines to show before and after each match (default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default: 100)"
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

        let pattern = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'pattern'".to_string())?;

        let search_path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let glob_str = params
            .get("glob")
            .and_then(|v| v.as_str());

        let ignore_case = params
            .get("ignoreCase")
            .or_else(|| params.get("ignore_case"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let literal = params
            .get("literal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let context = params
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .max(1) as usize;

        let regex_pattern = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };

        let re = regex::RegexBuilder::new(&regex_pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| format!("Invalid regex pattern '{}': {}", pattern, e))?;

        let glob_matcher = if let Some(g) = glob_str {
            let pat = if !g.contains('/') && !g.contains('\\') {
                format!("**/{}", g)
            } else {
                g.to_string()
            };
            let glob = globset::GlobBuilder::new(&pat)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| format!("Invalid glob pattern '{}': {}", g, e))?;
            Some(glob.compile_matcher())
        } else {
            None
        };

        let target_path = self.resolve_path(&search_path_str);
        let cwd = self.cwd.clone();
        let cancel_clone = cancel.clone();

        tokio::task::spawn_blocking(move || {
            if !target_path.exists() {
                return Err(format!("Path not found: {}", search_path_str));
            }

            let mut output_lines = Vec::new();
            let mut total_matches = 0usize;
            let mut total_bytes = 0usize;
            let mut byte_cap_reached = false;

            let push_lines = |lines: Vec<String>,
                              output_lines: &mut Vec<String>,
                              total_bytes: &mut usize,
                              byte_cap_reached: &mut bool| {
                for line in lines {
                    let add_bytes = line.len().saturating_add(1);
                    if total_bytes.saturating_add(add_bytes) > GREP_MAX_OUTPUT_BYTES {
                        *byte_cap_reached = true;
                        break;
                    }
                    *total_bytes += add_bytes;
                    output_lines.push(line);
                }
            };

            if target_path.is_file() {
                if cancel_clone.is_cancelled() {
                    return Err("Tool execution cancelled".to_string());
                }
                let meta = std::fs::metadata(&target_path)
                    .map_err(|e| format!("Failed to stat {}: {}", search_path_str, e))?;
                if meta.len() > GREP_MAX_FILE_BYTES {
                    return Err(format!(
                        "File {} exceeds maximum searchable size of {} bytes",
                        search_path_str, GREP_MAX_FILE_BYTES
                    ));
                }
                let bytes = std::fs::read(&target_path)
                    .map_err(|e| format!("Failed to read {}: {}", search_path_str, e))?;

                if !bytes.iter().take(1024).any(|&b| b == 0) {
                    if let Ok(content) = std::str::from_utf8(&bytes) {
                        let rel_display = target_path
                            .strip_prefix(&cwd)
                            .unwrap_or(&target_path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        let (lines, count) =
                            search_in_file(&rel_display, content, &re, context, limit);
                        push_lines(lines, &mut output_lines, &mut total_bytes, &mut byte_cap_reached);
                        total_matches += count;
                    }
                }
            } else {
                let mut walker = ignore::WalkBuilder::new(&target_path);
                walker.hidden(false);
                walker.git_ignore(true);
                walker.filter_entry(|e| e.file_name().to_str() != Some(".git"));

                for entry_res in walker.build() {
                    if cancel_clone.is_cancelled() {
                        return Err("Tool execution cancelled".to_string());
                    }

                    let entry = match entry_res {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        continue;
                    }

                    let file_path = entry.path();
                    let rel_path = file_path.strip_prefix(&cwd).unwrap_or(file_path);
                    let rel_path_str = rel_path.to_string_lossy().replace('\\', "/");
                    let file_name = file_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");

                    if let Some(ref matcher) = glob_matcher {
                        if !matcher.is_match(&rel_path_str) && !matcher.is_match(file_name) {
                            continue;
                        }
                    }

                    if let Ok(meta) = entry.metadata() {
                        if meta.len() > GREP_MAX_FILE_BYTES {
                            continue;
                        }
                    }

                    let bytes = match std::fs::read(file_path) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };

                    if bytes.iter().take(1024).any(|&b| b == 0) {
                        continue;
                    }

                    let content = match std::str::from_utf8(&bytes) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    let remaining = limit.saturating_sub(total_matches);
                    if remaining == 0 {
                        break;
                    }

                    let (lines, count) =
                        search_in_file(&rel_path_str, content, &re, context, remaining);
                    push_lines(lines, &mut output_lines, &mut total_bytes, &mut byte_cap_reached);
                    total_matches += count;

                    if total_matches >= limit || byte_cap_reached {
                        break;
                    }
                }
            }

            if total_matches == 0 {
                return Ok(AgentToolResult::text("No matches found"));
            }

            let mut output = output_lines.join("\n");
            if byte_cap_reached {
                output.push_str(&format!(
                    "\n\n[Output truncated at {} byte limit]",
                    GREP_MAX_OUTPUT_BYTES
                ));
            } else if total_matches >= limit {
                output.push_str(&format!("\n\n[Reached limit of {} matches]", limit));
            }

            Ok(AgentToolResult::text(output))
        })
        .await
        .map_err(|e| format!("Grep task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_grep_skips_git_dir_and_honors_cancel() {
        let dir = crate::TempTestDir::new("grep_git");
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("config"), "needle_in_git").unwrap();
        std::fs::write(dir.path().join("src.rs"), "needle_in_src").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({ "pattern": "needle" })
            .as_object()
            .unwrap()
            .clone();
        let res = tool
            .execute("tc", params.clone(), CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("needle_in_src"));
        assert!(!txt.contains("needle_in_git"));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(tool.execute("tc2", params, cancelled, None).await.is_err());
    }
}
