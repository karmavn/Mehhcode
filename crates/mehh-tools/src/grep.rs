use std::path::PathBuf;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};

#[derive(Debug, Clone)]
pub struct GrepTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
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

/// Returns (formatted lines, number of matches, and the exclusive end index in
/// `lines` for keeping `1..=match_count` matches). Cutting at `match_cut_ends[k - 1]`
/// preserves trailing context for the `k`-th match without leaking leading context
/// lines belonging to the dropped `(k + 1)`-th match.
fn search_in_file(
    path_display: &str,
    content: &str,
    re: &regex::Regex,
    context: usize,
    remaining_limit: usize,
) -> (Vec<String>, usize, Vec<usize>) {
    let mut match_cut_ends = Vec::new();

    if context == 0 {
        let mut output_lines = Vec::new();
        let mut match_count = 0usize;
        for (idx, raw_line) in content.split_inclusive('\n').enumerate() {
            let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if re.is_match(line) {
                match_count += 1;
                let line_num = idx + 1;
                let line_text = truncate_line(line, 500);
                output_lines.push(format!("{}:{}:{}", path_display, line_num, line_text));
                match_cut_ends.push(output_lines.len());
                if match_count >= remaining_limit {
                    break;
                }
            }
        }
        return (output_lines, match_count, match_cut_ends);
    }

    let lines: Vec<&str> = content
        .split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l);
            l.strip_suffix('\r').unwrap_or(l)
        })
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
        return (Vec::new(), 0, match_cut_ends);
    }

    let match_count = match_indices.len();
    let mut output_lines = Vec::new();
    let mut emitted_src_lines = Vec::new();

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

    let mut match_cursor = 0usize;

    for (start, end) in ranges {
        for (offset, line) in lines[start..end].iter().enumerate() {
            let idx = start + offset;
            let line_num = idx + 1;
            let line_text = truncate_line(line, 500);
            emitted_src_lines.push(idx);
            while match_cursor < match_indices.len() && match_indices[match_cursor] < idx {
                match_cursor += 1;
            }
            if match_indices.get(match_cursor) == Some(&idx) {
                output_lines.push(format!("{}:{}:{}", path_display, line_num, line_text));
            } else {
                output_lines.push(format!("{}-{}-{}", path_display, line_num, line_text));
            }
        }
    }

    for (k, &match_line) in match_indices.iter().enumerate() {
        let max_src = match_line.saturating_add(context);
        let upper_bound = match_indices
            .get(k + 1)
            .copied()
            .map_or(max_src, |next_m| max_src.min(next_m.saturating_sub(1)));
        let cut = emitted_src_lines.partition_point(|&src| src <= upper_bound);
        match_cut_ends.push(cut);
    }

    (output_lines, match_count, match_cut_ends)
}

fn read_grep_file_bounded(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(GREP_MAX_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > GREP_MAX_FILE_BYTES {
        return Ok(None);
    }
    Ok(Some(buf))
}

const GREP_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const GREP_MAX_OUTPUT_BYTES: usize = 100 * 1024;
const GREP_MAX_FILES: usize = 20_000;
const GREP_MAX_LIMIT: u64 = 5_000;
const GREP_MAX_CONTEXT: u64 = 50;
const GREP_BATCH_SIZE: usize = 64;

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

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
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
            .filter(|v| !v.is_null())
            .or_else(|| params.get("ignore_case").filter(|v| !v.is_null()))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let literal = params
            .get("literal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let context = params
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(GREP_MAX_CONTEXT) as usize;

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .clamp(1, GREP_MAX_LIMIT) as usize;

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
            let normalized = g.replace('\\', "/");
            let pat = if !normalized.contains('/') {
                format!("**/{normalized}")
            } else {
                normalized
            };
            // ignoreCase controls content matching only; filename globs stay
            // case-sensitive unless they spell out their own case.
            let glob = globset::GlobBuilder::new(&pat)
                .case_insensitive(false)
                .build()
                .map_err(|e| format!("Invalid glob pattern '{}': {}", g, e))?;
            Some(glob.compile_matcher())
        } else {
            None
        };

        let target_path = self.resolve_path(&search_path_str)?;
        let explicit_protected =
            context::path_targets_protected_credential(&search_path_str, &target_path);
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
            let mut file_cap_reached = false;
            let mut skipped_large_files = false;

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
                // Check the whole bounded file for NUL bytes, not just the
                // first 1 KB: a NUL past byte 1024 still means it is binary.
                let bytes = match read_grep_file_bounded(&target_path)
                    .map_err(|e| format!("Failed to read {}: {}", search_path_str, e))?
                {
                    Some(b) => b,
                    None => {
                        return Err(format!(
                            "File {} exceeds maximum searchable size of {} bytes",
                            search_path_str, GREP_MAX_FILE_BYTES
                        ));
                    }
                };
                if bytes.contains(&0) {
                    return Err(format!("File {} appears to be binary", search_path_str));
                }
                let content = std::str::from_utf8(&bytes)
                    .map_err(|_| format!("File {} is not valid UTF-8 text", search_path_str))?;
                let rel_display = target_path
                    .strip_prefix(&cwd)
                    .unwrap_or(&target_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let (lines, count, _) = search_in_file(&rel_display, content, &re, context, limit);
                push_lines(lines, &mut output_lines, &mut total_bytes, &mut byte_cap_reached);
                total_matches += count;
            } else {
                let mut walker = ignore::WalkBuilder::new(&target_path);
                walker.hidden(false);
                walker.git_ignore(true);
                walker.require_git(false);
                walker.sort_by_file_name(|a, b| a.cmp(b));
                walker.filter_entry(move |e| {
                    if e.depth() == 0 {
                        return true;
                    }
                    let Some(name) = e.file_name().to_str() else {
                        return true;
                    };
                    if name == ".git" {
                        return false;
                    }
                    explicit_protected || !context::is_protected_credential_name(name)
                });

                // Walk in deterministic (sorted) order and scan lazily in
                // bounded waves of `GREP_BATCH_SIZE` so early match/byte limits
                // stop walking the tree immediately.
                struct WalkCursor<'a> {
                    walk_iter: ignore::Walk,
                    target_path: &'a std::path::Path,
                    glob_matcher: Option<&'a globset::GlobMatcher>,
                    cancel: &'a CancellationToken,
                    files_queued: usize,
                    walk_done: bool,
                    file_cap_reached: bool,
                    skipped_large_files: bool,
                }
                impl WalkCursor<'_> {
                    fn next_wave(&mut self, out: &mut Vec<(PathBuf, String)>) -> Result<(), String> {
                        out.clear();
                        while !self.walk_done && out.len() < GREP_BATCH_SIZE {
                            if self.cancel.is_cancelled() {
                                return Err("Tool execution cancelled".to_string());
                            }
                            let Some(entry_res) = self.walk_iter.next() else {
                                self.walk_done = true;
                                break;
                            };
                            let entry = match entry_res {
                                Ok(e) => e,
                                Err(_) => continue,
                            };
                            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                                continue;
                            }

                            let file_path = entry.path();
                            // glob is relative to the requested search path, not cwd.
                            let rel_path =
                                file_path.strip_prefix(self.target_path).unwrap_or(file_path);
                            let mut rel_path_str_opt = None;
                            if let Some(matcher) = self.glob_matcher {
                                let rel_s = rel_path.to_string_lossy().replace('\\', "/");
                                let file_name = file_path
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("");
                                if !matcher.is_match(&rel_s) && !matcher.is_match(file_name) {
                                    continue;
                                }
                                rel_path_str_opt = Some(rel_s);
                            }
                            if let Ok(meta) = entry.metadata() {
                                if meta.len() > GREP_MAX_FILE_BYTES {
                                    self.skipped_large_files = true;
                                    continue;
                                }
                            }

                            let rel_path_str = rel_path_str_opt
                                .unwrap_or_else(|| rel_path.to_string_lossy().replace('\\', "/"));
                            out.push((file_path.to_path_buf(), rel_path_str));
                            self.files_queued += 1;
                            if self.files_queued >= GREP_MAX_FILES {
                                self.file_cap_reached = true;
                                self.walk_done = true;
                                break;
                            }
                        }
                        Ok(())
                    }
                }

                let mut cursor = WalkCursor {
                    walk_iter: walker.build(),
                    target_path: &target_path,
                    glob_matcher: glob_matcher.as_ref(),
                    cancel: &cancel_clone,
                    files_queued: 0,
                    walk_done: false,
                    file_cap_reached: false,
                    skipped_large_files: false,
                };

                let mut wave: Vec<(PathBuf, String)> = Vec::with_capacity(GREP_BATCH_SIZE);
                cursor.next_wave(&mut wave)?;

                let max_threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1);
                let num_workers = if cursor.walk_done {
                    max_threads.min(wave.len()).min(GREP_BATCH_SIZE)
                } else {
                    max_threads.min(GREP_BATCH_SIZE)
                };
                let saw_large_in_read = std::sync::atomic::AtomicBool::new(false);

                let scan_one = |file_path: &std::path::Path,
                                display: &str,
                                batch_limit: usize|
                 -> Option<(Vec<String>, usize, Vec<usize>)> {
                    // Check the whole bounded file for NUL bytes,
                    // not just the first 1 KB: a NUL past byte
                    // 1024 still means the file is binary.
                    let bytes = match read_grep_file_bounded(file_path) {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            saw_large_in_read.store(true, std::sync::atomic::Ordering::Relaxed);
                            return None;
                        }
                        Err(_) => return None,
                    };
                    if bytes.contains(&0) {
                        return None;
                    }
                    let content = std::str::from_utf8(&bytes).ok()?;
                    Some(search_in_file(display, content, &re, context, batch_limit))
                };

                if num_workers <= 1 {
                    while !wave.is_empty() {
                        if cancel_clone.is_cancelled() {
                            return Err("Tool execution cancelled".to_string());
                        }
                        if byte_cap_reached || total_matches >= limit {
                            break;
                        }
                        let batch_limit = limit.saturating_sub(total_matches);
                        for (file_path, display) in &wave {
                            if cancel_clone.is_cancelled() {
                                return Err("Tool execution cancelled".to_string());
                            }
                            if byte_cap_reached || total_matches >= limit {
                                break;
                            }
                            let Some((mut lines, count, match_cut_ends)) =
                                scan_one(file_path, display, batch_limit)
                            else {
                                continue;
                            };
                            let remaining = limit.saturating_sub(total_matches);
                            if remaining == 0 {
                                break;
                            }
                            let keep = remaining.min(count);
                            if keep > 0 {
                                let cut = match_cut_ends
                                    .get(keep - 1)
                                    .copied()
                                    .unwrap_or(lines.len());
                                lines.truncate(cut);
                                push_lines(
                                    lines,
                                    &mut output_lines,
                                    &mut total_bytes,
                                    &mut byte_cap_reached,
                                );
                                total_matches += keep;
                            }
                        }
                        if byte_cap_reached || total_matches >= limit || cursor.walk_done {
                            break;
                        }
                        cursor.next_wave(&mut wave)?;
                    }
                } else {
                    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                    use std::sync::{Barrier, Mutex, RwLock};

                    let shared_wave: RwLock<Vec<(PathBuf, String)>> = RwLock::new(Vec::new());
                    let wave_len = AtomicUsize::new(0);
                    let wave_limit = AtomicUsize::new(0);
                    let wave_stop = AtomicBool::new(false);
                    let next_idx = AtomicUsize::new(0);
                    let per_file: Vec<Mutex<Option<(Vec<String>, usize, Vec<usize>)>>> =
                        (0..GREP_BATCH_SIZE).map(|_| Mutex::new(None)).collect();
                    let start_barrier = Barrier::new(num_workers + 1);
                    let end_barrier = Barrier::new(num_workers + 1);

                    let mut cancelled_mid_scan = false;
                    std::thread::scope(|scope| {
                        for _ in 0..num_workers {
                            scope.spawn(|| loop {
                                start_barrier.wait();
                                if wave_stop.load(Ordering::Acquire) {
                                    break;
                                }
                                let wl = wave_len.load(Ordering::Relaxed);
                                let blimit = wave_limit.load(Ordering::Relaxed);
                                let wave_guard =
                                    shared_wave.read().unwrap_or_else(|e| e.into_inner());
                                loop {
                                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                                    if idx >= wl || cancel_clone.is_cancelled() {
                                        break;
                                    }
                                    let (ref file_path, ref display) = wave_guard[idx];
                                    let res = scan_one(file_path, display, blimit);
                                    *per_file[idx].lock().unwrap_or_else(|e| e.into_inner()) = res;
                                }
                                drop(wave_guard);
                                end_barrier.wait();
                            });
                        }

                        while !wave.is_empty() {
                            if cancel_clone.is_cancelled() {
                                cancelled_mid_scan = true;
                                break;
                            }
                            if byte_cap_reached || total_matches >= limit {
                                break;
                            }
                            let wl = wave.len();
                            {
                                let mut dst =
                                    shared_wave.write().unwrap_or_else(|e| e.into_inner());
                                std::mem::swap(&mut *dst, &mut wave);
                            }
                            for slot in &per_file[..wl] {
                                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            }
                            wave_len.store(wl, Ordering::Relaxed);
                            wave_limit.store(limit.saturating_sub(total_matches), Ordering::Relaxed);
                            next_idx.store(0, Ordering::Relaxed);

                            start_barrier.wait();
                            end_barrier.wait();

                            if cancel_clone.is_cancelled() {
                                cancelled_mid_scan = true;
                                break;
                            }

                            for slot in &per_file[..wl] {
                                if byte_cap_reached {
                                    break;
                                }
                                let result = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                                let Some((mut lines, count, match_cut_ends)) = result else {
                                    continue;
                                };
                                let remaining = limit.saturating_sub(total_matches);
                                if remaining == 0 {
                                    break;
                                }
                                // `remaining` counts matches, not formatted lines: cut right
                                // after the last kept match's context so dropped matches do
                                // not leak leading context rows.
                                let keep = remaining.min(count);
                                if keep > 0 {
                                    let cut = match_cut_ends
                                        .get(keep - 1)
                                        .copied()
                                        .unwrap_or(lines.len());
                                    lines.truncate(cut);
                                    push_lines(
                                        lines,
                                        &mut output_lines,
                                        &mut total_bytes,
                                        &mut byte_cap_reached,
                                    );
                                    total_matches += keep;
                                }
                            }

                            if byte_cap_reached || total_matches >= limit || cursor.walk_done {
                                break;
                            }
                            if cursor.next_wave(&mut wave).is_err() {
                                cancelled_mid_scan = true;
                                break;
                            }
                        }

                        wave_stop.store(true, Ordering::Release);
                        start_barrier.wait();
                    });

                    if cancelled_mid_scan {
                        return Err("Tool execution cancelled".to_string());
                    }
                }
                file_cap_reached = cursor.file_cap_reached;
                skipped_large_files = cursor.skipped_large_files
                    || saw_large_in_read.load(std::sync::atomic::Ordering::Relaxed);
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
            if file_cap_reached {
                output.push_str(&format!(
                    "\n\n[Stopped after {} files; later files were not searched]",
                    GREP_MAX_FILES
                ));
            }
            if skipped_large_files {
                output.push_str(&format!(
                    "\n\n[Skipped files larger than {} bytes]",
                    GREP_MAX_FILE_BYTES
                ));
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

    #[tokio::test]
    async fn test_grep_single_file_rejects_nul_beyond_first_kb() {
        let dir = crate::TempTestDir::new("grep_binary_nul_file");
        let mut data = vec![b' '; 2000];
        data.push(0);
        data.extend_from_slice(b"needle\n");
        std::fs::write(dir.path().join("data.txt"), &data).unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({ "pattern": "needle", "path": "data.txt" })
            .as_object()
            .unwrap()
            .clone();
        let err = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(
            err.contains("appears to be binary"),
            "a NUL past byte 1024 must still be detected: {err}"
        );
    }

    #[tokio::test]
    async fn test_grep_directory_skips_nul_beyond_first_kb() {
        let dir = crate::TempTestDir::new("grep_binary_nul_dir");
        let mut data = vec![b' '; 2000];
        data.push(0);
        data.extend_from_slice(b"needle\n");
        std::fs::write(dir.path().join("binary.txt"), &data).unwrap();
        std::fs::write(dir.path().join("clean.txt"), "nothing relevant here\n").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({ "pattern": "needle" }).as_object().unwrap().clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(
            txt, "No matches found",
            "binary file must be skipped: {txt}"
        );
    }

    #[tokio::test]
    async fn test_grep_skips_protected_credentials_unless_explicitly_targeted() {
        let dir = crate::TempTestDir::new("grep_protected");
        std::fs::create_dir_all(dir.path().join(".mehh")).unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".mehh/secret.txt"), "token_value\n").unwrap();
        std::fs::write(dir.path().join(".ssh/id_rsa"), "token_value\n").unwrap();
        std::fs::write(dir.path().join(".env"), "API_KEY=token_value\n").unwrap();
        std::fs::write(dir.path().join(".env.local"), "API_KEY=token_value\n").unwrap();
        std::fs::write(dir.path().join("auth.json"), "token_value\n").unwrap();
        std::fs::write(dir.path().join("mcp-auth.json"), "token_value\n").unwrap();
        std::fs::write(dir.path().join("trust.json"), "token_value\n").unwrap();
        std::fs::write(dir.path().join("src.rs"), "let s = \"token_value\";\n").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({ "pattern": "token_value" })
            .as_object()
            .unwrap()
            .clone();
        let res = tool
            .execute("tc1", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("src.rs"), "expected src.rs in: {txt}");
        for hidden in [".mehh", ".ssh", ".env", "auth.json", "mcp-auth.json", "trust.json"] {
            assert!(!txt.contains(hidden), "must not leak {hidden}: {txt}");
        }

        // Explicitly targeting .mehh or .env must still work.
        let explicit_dir = json!({ "pattern": "token_value", "path": ".mehh" })
            .as_object()
            .unwrap()
            .clone();
        let res_dir = tool
            .execute("tc2", explicit_dir, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt_dir = match &res_dir.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt_dir.contains("secret.txt"), "got: {txt_dir}");

        let explicit_file = json!({ "pattern": "token_value", "path": ".env" })
            .as_object()
            .unwrap()
            .clone();
        let res_file = tool
            .execute("tc3", explicit_file, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt_file = match &res_file.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt_file.contains("API_KEY=token_value"), "got: {txt_file}");
    }

    #[tokio::test]
    async fn test_grep_clamps_limits_and_stops_across_batches() {
        let dir = crate::TempTestDir::new("grep_batches");
        for i in 0..80 {
            std::fs::write(
                dir.path().join(format!("file_{i:03}.txt")),
                format!("before\nneedle_match_{i}\nafter\n"),
            )
            .unwrap();
        }
        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({
            "pattern": "needle_match_",
            "limit": 5,
            "context": 999_999
        })
        .as_object()
        .unwrap()
        .clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("[Reached limit of 5 matches]"), "got: {txt}");

        let big_limit = json!({
            "pattern": "needle_match_",
            "limit": 999_999_999u64
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(tool
            .execute("tc2", big_limit, CancellationToken::new(), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_grep_context_limit_does_not_leak_dropped_match_leading_context() {
        let dir = crate::TempTestDir::new("grep_ctx_cut");
        std::fs::write(dir.path().join("a.txt"), "hit_one\n").unwrap();
        std::fs::write(
            dir.path().join("b.txt"),
            "b_l1\nb_hit_two\nb_after_two\nfiller1\nfiller2\nfiller3\ndropped_leading_ctx\nb_hit_three\n",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({
            "pattern": "hit_",
            "limit": 2,
            "context": 1
        })
        .as_object()
        .unwrap()
        .clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("b_hit_two"), "got: {txt}");
        assert!(txt.contains("b_after_two"), "trailing ctx of kept match missing: {txt}");
        assert!(
            !txt.contains("dropped_leading_ctx"),
            "leading context of dropped match leaked: {txt}"
        );
        assert!(!txt.contains("b_hit_three"), "dropped match leaked: {txt}");
    }

    #[tokio::test]
    async fn test_grep_ignores_null_ignore_case_alias() {
        let dir = crate::TempTestDir::new("grep_null_alias");
        std::fs::write(dir.path().join("case.txt"), "HelloWorld\n").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let params = json!({
            "pattern": "helloworld",
            "ignoreCase": null,
            "ignore_case": true
        })
        .as_object()
        .unwrap()
        .clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("HelloWorld"), "got: {txt}");
    }

    #[test]
    fn test_grep_trailing_newline_does_not_create_phantom_empty_line() {
        let re = regex::Regex::new("^$").unwrap();
        let (lines_no_ctx, count_no_ctx, _) = search_in_file("f.txt", "a\nb\n", &re, 0, 10);
        assert_eq!(count_no_ctx, 0, "unexpected matches: {lines_no_ctx:?}");

        let (lines_ctx, count_ctx, _) = search_in_file("f.txt", "a\nb\n", &regex::Regex::new("b").unwrap(), 1, 10);
        assert_eq!(count_ctx, 1);
        assert_eq!(
            lines_ctx,
            vec!["f.txt-1-a".to_string(), "f.txt:2:b".to_string()],
            "trailing newline must not emit phantom empty line 3"
        );
    }
}
