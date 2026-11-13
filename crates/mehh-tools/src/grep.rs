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
    if s.chars().count() > max_len {
        let prefix: String = s.chars().take(max_len).collect();
        format!("{}... [truncated]", prefix)
    } else {
        s.to_string()
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
            let end = (idx + context + 1).min(lines.len());
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
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let pattern = params
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'pattern'".to_string())?;

        let search_path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

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

        let target_path = self.resolve_path(search_path_str);
        if !target_path.exists() {
            return Err(format!("Path not found: {}", search_path_str));
        }

        let mut output_lines = Vec::new();
        let mut total_matches = 0;

        if target_path.is_file() {
            let bytes = tokio::fs::read(&target_path)
                .await
                .map_err(|e| format!("Failed to read {}: {}", search_path_str, e))?;

            if !bytes.iter().take(1024).any(|&b| b == 0) {
                if let Ok(content) = std::str::from_utf8(&bytes) {
                    let rel_display = target_path
                        .strip_prefix(&self.cwd)
                        .unwrap_or(&target_path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let (lines, count) = search_in_file(&rel_display, content, &re, context, limit);
                    output_lines.extend(lines);
                    total_matches += count;
                }
            }
        } else {
            let mut walker = ignore::WalkBuilder::new(&target_path);
            walker.hidden(false);
            walker.git_ignore(true);

            for entry_res in walker.build() {
                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }

                let file_path = entry.path();
                let rel_path = file_path
                    .strip_prefix(&self.cwd)
                    .unwrap_or(file_path);
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

                let (lines, count) = search_in_file(&rel_path_str, content, &re, context, remaining);
                output_lines.extend(lines);
                total_matches += count;

                if total_matches >= limit {
                    break;
                }
            }
        }

        if total_matches == 0 {
            return Ok(AgentToolResult::text("No matches found"));
        }

        let mut output = output_lines.join("\n");
        if total_matches >= limit {
            output.push_str(&format!("\n\n[Reached limit of {} matches]", limit));
        }

        Ok(AgentToolResult::text(output))
    }
}
