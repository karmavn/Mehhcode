use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{TextContent, ToolResultContentBlock};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::fs::atomic_write_checked;
use crate::core::output::truncate_head;
use crate::undo_edit::{EditUndoStore, FileSnapshot, UNDO_UNAVAILABLE_WARNING};

const MAX_DIFF_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct EditTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
    pub undo_store: Option<Arc<EditUndoStore>>,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
            undo_store: None,
        }
    }

    pub fn with_undo_store(mut self, undo_store: Arc<EditUndoStore>) -> Self {
        self.undo_store = Some(undo_store);
        self
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
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

fn split_bom(s: &str) -> (&str, &str) {
    if let Some(stripped) = s.strip_prefix('\u{feff}') {
        ("\u{feff}", stripped)
    } else {
        ("", s)
    }
}

fn apply_replacements_to_original(original: &str, replacements: &[MatchedEdit]) -> String {
    let mut crlf_before = 0usize;
    let crlf_indices: Vec<usize> = original
        .match_indices("\r\n")
        .map(|(idx, _)| idx)
        .collect();
    let mut next_crlf_idx = 0usize;

    let mut to_original = |offset: usize| -> usize {
        while next_crlf_idx < crlf_indices.len() && crlf_indices[next_crlf_idx] - crlf_before < offset {
            crlf_before += 1;
            next_crlf_idx += 1;
        }
        offset + crlf_before
    };

    // Find the terminator of the line containing `offset`, ignoring newline
    // bytes inside string/char literals. A plain `find('\n')` can pick up the
    // CRLF inside `let s = "\r\n";` and wrongly rewrite new_text to CRLF.
    let get_line_ending_at = |content: &str, offset: usize| -> &'static str {
        let line_start = content[..offset].rfind('\n').map_or(0, |pos| pos + 1);
        let bytes = content.as_bytes();
        let mut i = line_start;
        let mut quote: Option<u8> = None;
        let mut escaped = false;

        while i < bytes.len() {
            let byte = bytes[i];
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match byte {
                b'"' => {
                    quote = Some(byte);
                    i += 1;
                }
                b'\'' => {
                    // Closed same-line `'...'` covers Rust char literals ('"', '\'', '\n')
                    // and JS/Python/SQL/Shell single-quoted strings ('say "hi').
                    // Unclosed quotes (Rust lifetimes `&'a`, `<'a`, `'static`, labels
                    // `'outer:`, comments `// don't`) stay literal so they never swallow `\n`.
                    let is_rust_lifetime_prefix = i > line_start
                        && matches!(bytes[i - 1], b'&' | b'<');
                    let mut next_idx = i + 1;
                    if !is_rust_lifetime_prefix {
                        let mut j = i + 1;
                        let mut esc = false;
                        while j < bytes.len() && bytes[j] != b'\n' {
                            if esc {
                                esc = false;
                            } else if bytes[j] == b'\\' {
                                esc = true;
                            } else if bytes[j] == b'\'' {
                                next_idx = j + 1;
                                break;
                            }
                            j += 1;
                        }
                    }
                    i = next_idx;
                }
                b'\n' => {
                    return if i > line_start && bytes[i - 1] == b'\r' {
                        "\r\n"
                    } else {
                        "\n"
                    };
                }
                _ => i += 1,
            }
        }

        // Last line has no terminator: use the file's dominant style.
        let crlf_count = content.matches("\r\n").count();
        let lf_count = content.matches('\n').count();
        if crlf_count > 0 && crlf_count == lf_count {
            "\r\n"
        } else {
            "\n"
        }
    };

    let mut parts = String::with_capacity(original.len());
    let mut position = 0usize;

    for m in replacements {
        let start = to_original(m.start);
        parts.push_str(&original[position..start]);
        let ending = get_line_ending_at(original, start);
        if ending == "\n" {
            parts.push_str(&m.new_text);
        } else {
            parts.push_str(&m.new_text.replace('\n', ending));
        }
        position = to_original(m.end);
    }
    parts.push_str(&original[position..]);
    parts
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    aliases: &[&str],
    label: &str,
) -> Result<&'a str, String> {
    aliases
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .ok_or_else(|| format!("{label} must be a string"))
}

fn parse_edits(params: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<SingleEdit>, String> {
    let mut edits = Vec::new();

    if let Some(edits_val) = params.get("edits") {
        let items: Vec<serde_json::Value> = if let Some(arr) = edits_val.as_array() {
            arr.clone()
        } else if let Some(s) = edits_val.as_str() {
            serde_json::from_str(s)
                .map_err(|e| format!("Failed to parse 'edits' JSON string: {e}"))?
        } else {
            return Err("Parameter 'edits' must be an array or a JSON string".to_string());
        };

        for (i, item) in items.iter().enumerate() {
            if !item.is_object() {
                return Err(format!("edits[{i}] must be an object"));
            }
            let old_text = required_string(
                item,
                &["oldText", "old_text", "old_string"],
                &format!("edits[{i}].oldText"),
            )?;
            let new_text = required_string(
                item,
                &["newText", "new_text", "new_string"],
                &format!("edits[{i}].newText"),
            )?;
            edits.push(SingleEdit {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            });
        }
    }

    if edits.is_empty() {
        let old = params
            .get("oldText")
            .or_else(|| params.get("old_text"))
            .or_else(|| params.get("old_string"))
            .and_then(|v| v.as_str());
        let new = params
            .get("newText")
            .or_else(|| params.get("new_text"))
            .or_else(|| params.get("new_string"))
            .and_then(|v| v.as_str());

        if let (Some(old), Some(new)) = (old, new) {
            edits.push(SingleEdit {
                old_text: old.to_string(),
                new_text: new.to_string(),
            });
        } else if old.is_some() || new.is_some() {
            return Err("Both 'oldText' and 'newText' are required for a single edit".to_string());
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

use crate::file_mutation_queue::with_file_mutation_queue;

const CLOSEST_HINT_MIN_SCORE: f64 = 0.4;

/// Normalizes CRLF line endings and strips trailing horizontal whitespace per line.
pub fn normalize_for_fuzzy_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for line in s.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_end_matches([' ', '\t']);
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(trimmed);
    }
    out
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
    let pat_lines: Vec<&str> = pat_body
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect();
    let content_lines = split_lines_with_offsets(content);

    let mut matches = Vec::new();
    if pat_lines.is_empty() {
        return matches;
    }

    if pat_lines.len() == 1 && !had_trailing_nl {
        let pat = pat_lines[0];
        if pat.is_empty() {
            return matches;
        }
        for &(l_start, l_end, l_text) in &content_lines {
            let trimmed = l_text.trim_end_matches([' ', '\t']);
            for (rel_idx, _) in trimmed.match_indices(pat) {
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
            trimmed == *pat_line
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

fn char_bigrams(s: &str) -> std::collections::HashSet<(char, char)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        std::collections::HashSet::new()
    } else {
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    }
}

fn line_similarity(
    a: &str,
    b: &str,
    a_bigrams: &std::collections::HashSet<(char, char)>,
) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    if b.contains(a) || a.contains(b) {
        let a_len = if a.is_ascii() { a.len() } else { a.chars().count() };
        let b_len = if b.is_ascii() { b.len() } else { b.chars().count() };
        let min_len = a_len.min(b_len) as f64;
        let max_len = a_len.max(b_len) as f64;
        return (min_len / max_len).max(0.6);
    }
    if a_bigrams.is_empty() {
        return 0.0;
    }
    let b_bigrams = char_bigrams(b);
    if b_bigrams.is_empty() {
        return 0.0;
    }
    let shared = a_bigrams.intersection(&b_bigrams).count() as f64;
    (2.0 * shared) / ((a_bigrams.len() + b_bigrams.len()) as f64)
}

fn closest_line_hint(content: &str, old_norm: &str) -> Option<String> {
    let target = old_norm
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let target_bigrams = char_bigrams(target);
    let mut best: Option<(usize, f64, &str)> = None;
    for (idx, line) in content.lines().enumerate() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        let score = line_similarity(target, candidate, &target_bigrams);
        if score >= CLOSEST_HINT_MIN_SCORE
            && best.is_none_or(|(_, best_s, _)| score > best_s)
        {
            best = Some((idx + 1, score, candidate));
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

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
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

        let full_path = self.resolve_path(path_str)?;
        // Bound to the path the security inspector approved, not to whatever
        // the path happens to resolve to now.
        let inspected_target = crate::core::fs::inspected_target(&params, &full_path).await;
        // The requested path passed policy, but a dangling symlink can resolve
        // to a target outside the allowed roots. Validate the target itself.
        self.path_policy.validate(&inspected_target)?;
        let undo_store = self.undo_store.clone();
        let cancel_inner = cancel.clone();
        with_file_mutation_queue(&full_path, async {
            // The queue await may have been waiting while the caller cancelled.
            if cancel_inner.is_cancelled() {
                return Err("Tool execution cancelled".to_string());
            }
            if !full_path.exists() {
                return Err(format!("File not found: {}", path_str));
            }

            let raw_bytes = tokio::fs::read(&full_path)
                .await
                .map_err(|e| format!("Failed to read {}: {}", path_str, e))?;

            let raw_content = String::from_utf8(raw_bytes.clone())
                .map_err(|e| format!("File {} is not valid UTF-8: {}", path_str, e))?;

            let (bom, content) = split_bom(&raw_content);
            let normalized_content = content.replace("\r\n", "\n");

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

            let final_body = if content == normalized_content {
                new_normalized
            } else {
                apply_replacements_to_original(content, &matched_edits)
            };
            let final_content = format!("{bom}{final_body}");

            atomic_write_checked(&full_path, &inspected_target, final_content.as_bytes()).await?;

            // Record undo history only after the write actually succeeded;
            // otherwise a failed write would leave a phantom snapshot.
            let mut undo_unavailable = false;
            if let Some(ref store) = undo_store {
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                if !store.push_snapshot(
                    &full_path,
                    FileSnapshot::Existed {
                        bytes: raw_bytes,
                        timestamp_ms,
                        target: inspected_target.clone(),
                    },
                ) {
                    undo_unavailable = true;
                }
            }

            let diff = similar::TextDiff::from_lines(&raw_content, &final_content)
                .unified_diff()
                .context_radius(3)
                .header(path_str, path_str)
                .to_string();
            let diff = truncate_head(
                &diff,
                MAX_DIFF_BYTES,
                "\n... [diff truncated]",
            );

            let warning = if undo_unavailable {
                format!("\n\n{}", UNDO_UNAVAILABLE_WARNING)
            } else {
                String::new()
            };

            let details = json!({
                "diff": diff,
                "path": path_str,
            });

            Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: format!("Successfully edited {}.\n\n{}{}", path_str, diff, warning),
                    text_signature: None,
                })],
                details: Some(details),
                ..Default::default()
            })
        })
        .await
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

    #[tokio::test]
    async fn concurrent_edits_on_same_file_do_not_clobber() {
        let temp_dir = crate::TempTestDir::new("concurrent_edits");
        let file = temp_dir.path().join("concurrent.txt");
        std::fs::write(&file, "SECTION_A_START\nalpha\nSECTION_A_END\nSECTION_B_START\nbeta\nSECTION_B_END\n").unwrap();

        let tool = EditTool::new(temp_dir.path().to_path_buf());
        let tool_a = tool.clone();
        let tool_b = tool.clone();

        let task_a = tokio::spawn(async move {
            let params = serde_json::json!({
                "path": "concurrent.txt",
                "oldText": "alpha",
                "newText": "ALPHA_EDITED"
            })
            .as_object()
            .unwrap()
            .clone();
            tool_a.execute("tc_a", params, CancellationToken::new(), None).await
        });

        let task_b = tokio::spawn(async move {
            let params = serde_json::json!({
                "path": "concurrent.txt",
                "oldText": "beta",
                "newText": "BETA_EDITED"
            })
            .as_object()
            .unwrap()
            .clone();
            tool_b.execute("tc_b", params, CancellationToken::new(), None).await
        });

        let (res_a, res_b) = tokio::join!(task_a, task_b);
        assert!(res_a.unwrap().is_ok());
        assert!(res_b.unwrap().is_ok());

        let final_content = std::fs::read_to_string(&file).unwrap();
        assert!(final_content.contains("ALPHA_EDITED"));
        assert!(final_content.contains("BETA_EDITED"));
    }

    #[tokio::test]
    async fn edit_oversized_file_warns_that_undo_is_unavailable() {
        let dir = crate::TempTestDir::new("edit_oversized");
        let file = dir.path().join("huge.txt");
        // Snapshot cap is 4 MiB; make the original file larger than that so
        // push_snapshot cannot record it.
        let mut content = "fn keep() {}\n".to_string();
        content.push_str(&"x".repeat(5 * 1024 * 1024));
        content.push_str("\nUNIQUE_TARGET old\n");
        std::fs::write(&file, content.as_bytes()).unwrap();

        let undo_store = Arc::new(EditUndoStore::new());
        let tool = EditTool::new(dir.path().to_path_buf()).with_undo_store(undo_store.clone());
        let params = json!({
            "path": "huge.txt",
            "oldText": "UNIQUE_TARGET old",
            "newText": "UNIQUE_TARGET new"
        })
        .as_object()
        .unwrap()
        .clone();

        let res = tool
            .execute("tc_edit", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("undo") && text.contains("unavailable"),
            "edit of an oversized file must warn that undo is unavailable, got: {}",
            &text[..text.len().min(200)]
        );

        // No snapshot was recorded, so undo_edit cannot revert the change.
        let undo = crate::UndoEditTool::new(dir.path().to_path_buf(), undo_store);
        let mut undo_params = serde_json::Map::new();
        undo_params.insert("path".to_string(), serde_json::json!("huge.txt"));
        let undo_err = undo
            .execute("tc_undo", undo_params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        let lower = undo_err.to_lowercase();
        assert!(
            lower.contains("undo") && lower.contains("unavailable"),
            "undo_edit must say undo is unavailable, got: {}",
            undo_err
        );
        assert!(
            !undo_err.contains("No previous edit history found"),
            "undo_edit must not report the old cryptic message, got: {}",
            undo_err
        );
    }

    #[test]
    fn apply_replacements_preserves_lf_on_rust_lifetime_line_followed_by_crlf_later() {
        // Odd number of `'a` lifetimes on the target line itself must not bleed
        // past `\n` into a later `\r\n` line and misclassify the line ending.
        let original = "fn foo<'a>(s: &'a str, b: &'a str) {\n    println!(\"{s}\");\n}\n// trailing\r\n";
        let needle = "b: &'a str";
        let start = original.find(needle).unwrap();
        let replacements = vec![MatchedEdit {
            index: 0,
            start,
            end: start + needle.len(),
            new_text: "b: &'a str,\n    c: &'a str".to_string(),
        }];
        let out = apply_replacements_to_original(original, &replacements);
        assert!(
            out.contains("b: &'a str,\n    c: &'a str"),
            "must preserve LF, got: {out:?}"
        );
        assert!(
            !out.contains("b: &'a str,\r\n"),
            "must not convert LF to CRLF on lifetime line: {out:?}"
        );
    }

    #[test]
    fn apply_replacements_preserves_lf_on_single_quoted_string_with_double_quote() {
        let original = "const s = 'say \"hello';\n// trailing\r\n";
        let needle = "const s = 'say \"hello';";
        let start = original.find(needle).unwrap();
        let replacements = vec![MatchedEdit {
            index: 0,
            start,
            end: start + needle.len(),
            new_text: "const s = 'a';\nconst t = 'b';".to_string(),
        }];
        let out = apply_replacements_to_original(original, &replacements);
        assert!(
            out.contains("const s = 'a';\nconst t = 'b';"),
            "must preserve LF after closed single-quoted string with inner double quote, got: {out:?}"
        );
        assert!(
            !out.contains("const s = 'a';\r\nconst t = 'b';"),
            "must not bleed past single-quoted string into later CRLF: {out:?}"
        );
    }
}
