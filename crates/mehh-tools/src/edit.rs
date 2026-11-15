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

const CLOSEST_HINT_MIN_SCORE: f64 = 0.4;

/// Normalizes CRLF line endings and strips trailing horizontal whitespace per line.
pub fn normalize_for_fuzzy_match(s: &str) -> String {
    s.replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
}

fn split_lines_with_offsets(s: &str) -> Vec<(usize, usize, &str)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for part in s.split('\n') {
        let end = start + part.len();
        lines.push((start, end, part));
        start = end + 1;
    }
    lines
}

fn find_fuzzy_matches(content: &str, old_norm: &str) -> Vec<(usize, usize)> {
    let had_trailing_nl = old_norm.ends_with('\n');
    let pat_body = if had_trailing_nl {
        &old_norm[..old_norm.len() - 1]
    } else {
        old_norm
    };
    let pat_lines: Vec<String> = pat_body
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']).to_string())
        .collect();
    let content_lines = split_lines_with_offsets(content);

    let mut matches = Vec::new();
    if pat_lines.is_empty() {
        return matches;
    }

    if pat_lines.len() == 1 && !had_trailing_nl {
        let pat = &pat_lines[0];
        if pat.is_empty() {
            return matches;
        }
        for &(l_start, l_end, l_text) in &content_lines {
            let trimmed = l_text.trim_end_matches([' ', '\t']);
            for (rel_idx, _) in trimmed.match_indices(pat.as_str()) {
                let abs_start = l_start + rel_idx;
                let abs_end = if rel_idx + pat.len() == trimmed.len() {
                    // Consume trailing whitespace on that line when matching up to line end
                    l_end
                } else {
                    abs_start + pat.len()
                };
                matches.push((abs_start, abs_end));
            }
        }
        return matches;
    }

    if pat_lines.len() > content_lines.len() {
        return matches;
    }

    for r in 0..=content_lines.len() - pat_lines.len() {
        let all_match = pat_lines.iter().enumerate().all(|(k, pat_line)| {
            let trimmed = content_lines[r + k].2.trim_end_matches([' ', '\t']);
            trimmed == pat_line
        });
        if all_match {
            let start = content_lines[r].0;
            let last_line_end = content_lines[r + pat_lines.len() - 1].1;
            let end = if had_trailing_nl && last_line_end < content.len() {
                last_line_end + 1
            } else {
                last_line_end
            };
            matches.push((start, end));
        }
    }
    matches
}

fn line_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    if b.contains(a) || a.contains(b) {
        let min_len = a.chars().count().min(b.chars().count()) as f64;
        let max_len = a.chars().count().max(b.chars().count()) as f64;
        return (min_len / max_len).max(0.6);
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.len() < 2 || b_chars.len() < 2 {
        return 0.0;
    }
    let a_bigrams: std::collections::HashSet<(char, char)> =
        a_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let b_bigrams: std::collections::HashSet<(char, char)> =
        b_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let shared = a_bigrams.intersection(&b_bigrams).count() as f64;
    (2.0 * shared) / ((a_bigrams.len() + b_bigrams.len()) as f64)
}

fn closest_line_hint(content: &str, old_norm: &str) -> Option<String> {
    let target = old_norm
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let mut best: Option<(usize, f64, &str)> = None;
    for (idx, line) in content.lines().enumerate() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        let score = line_similarity(target, candidate);
        if score >= CLOSEST_HINT_MIN_SCORE {
            if best.is_none_or(|(_, best_s, _)| score > best_s) {
                best = Some((idx + 1, score, candidate));
            }
        }
    }
    best.map(|(line_num, _, preview)| {
        let clipped: String = preview.chars().take(120).collect();
        format!("Closest match near line {line_num}: `{clipped}`")
    })
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text (with trailing-whitespace fuzzy fallback). Every edits[].oldText must match a unique, non-overlapping region of the file."
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

            let exact_matches: Vec<(usize, usize)> = normalized_content
                .match_indices(&old_norm)
                .map(|(start, _)| (start, start + old_norm.len()))
                .collect();

            let (start, end) = if exact_matches.len() == 1 {
                exact_matches[0]
            } else if exact_matches.len() > 1 {
                return Err(format!(
                    "Found {} occurrences of edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
                    exact_matches.len(), i, path_str
                ));
            } else {
                let fuzzy_matches = find_fuzzy_matches(&normalized_content, &old_norm);
                if fuzzy_matches.len() == 1 {
                    fuzzy_matches[0]
                } else if fuzzy_matches.len() > 1 {
                    return Err(format!(
                        "Found {} fuzzy matches for edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
                        fuzzy_matches.len(), i, path_str
                    ));
                } else {
                    let hint = closest_line_hint(&normalized_content, &old_norm)
                        .map(|h| format!("\n{h}"))
                        .unwrap_or_default();
                    return Err(format!(
                        "Could not find edits[{}] in {}. The oldText must match exact or trailing-whitespace-normalized text.{hint}",
                        i, path_str
                    ));
                }
            };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_tier2_fuzzy_trailing_whitespace_and_crlf() {
        let dir = crate::TempTestDir::new("edit_tier2");
        let file = dir.path().join("sample.rs");
        std::fs::write(&file, "fn a() {   \r\n    let x = 1;  \r\n}\r\n").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        let params = json!({
            "path": "sample.rs",
            "oldText": "fn a() {\n    let x = 1;\n}",
            "newText": "fn a() {\n    let x = 2;\n}"
        })
        .as_object()
        .unwrap()
        .clone();

        tool.execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let updated = std::fs::read_to_string(&file).unwrap();
        assert_eq!(updated, "fn a() {\r\n    let x = 2;\r\n}\r\n");
    }

    #[tokio::test]
    async fn test_edit_rejects_ambiguous_fuzzy_match_and_provides_hint_on_miss() {
        let dir = crate::TempTestDir::new("edit_ambig");
        let file = dir.path().join("dup.txt");
        std::fs::write(&file, "let value = 1;  \nlet value = 1;\n").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        let dup_params = json!({
            "path": "dup.txt",
            "oldText": "let value = 1;",
            "newText": "let value = 2;"
        })
        .as_object()
        .unwrap()
        .clone();
        let err_dup = tool
            .execute("tc1", dup_params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err_dup.contains("2"));

        std::fs::write(&file, "fn compute_total(items: &[i32]) -> i32 {\n    0\n}\n").unwrap();
        let miss_params = json!({
            "path": "dup.txt",
            "oldText": "fn compute_total(items: &[i64]) -> i64 {",
            "newText": "fn compute_total() {"
        })
        .as_object()
        .unwrap()
        .clone();
        let err_miss = tool
            .execute("tc2", miss_params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err_miss.contains("Closest match near line 1"));
    }
}
