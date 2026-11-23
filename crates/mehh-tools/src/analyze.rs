use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use ignore::WalkBuilder;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use regex::Regex;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};
use crate::core::output::truncate_head;

const MAX_ANALYZE_CHARS: usize = 24_000;
const MAX_FOCUS_LINES: usize = 250;
const MAX_ANALYZE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 5_000;

static RUST_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:pub(?:\([^)]+\))?[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+(?:\x22[^\x22]+\x22[ \t]+)?)?(fn|struct|enum|trait|impl|type|const|static)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    )
    .unwrap()
});
static TS_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(function\*?|class|interface|type|const|let|var)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    )
    .unwrap()
});
static PY_SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([ \t]*)(?:async[ \t]+)?(def|class)[ \t]+([A-Za-z0-9_]+)").unwrap()
});
static GO_FUNC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*func\b(?:[ \t]+\([^)]+\))?[ \t]+([A-Za-z0-9_]+)").unwrap()
});
static GO_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ \t]*type[ \t]+([A-Za-z0-9_]+)[ \t]+(struct|interface)").unwrap()
});

#[derive(Debug, Clone)]
pub struct AnalyzeTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl AnalyzeTool {
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

#[derive(Debug, Clone)]
struct SymbolSpan {
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    signature: String,
}

/// Length of a closed Rust char literal at `start` (`'x'`, `'\n'`, `'\xNN'`,
/// `'\u{...}'`), or `None` for lifetimes, labels and prose apostrophes.
fn char_literal_len(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'\'') {
        return None;
    }
    let first = *chars.get(start + 1)?;
    let second = *chars.get(start + 2)?;
    // `'ident` is a lifetime/label, never a char literal. Only a single char
    // followed by the closing quote or a backslash escape qualifies.
    if first != '\\' && second != '\'' {
        return None;
    }
    let mut k = start + 2;
    let mut escaped = first == '\\';
    while let Some(&ch) = chars.get(k) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '\'' {
            return Some(k + 1 - start);
        }
        if k - start > 12 {
            return None;
        }
        k += 1;
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
enum ApostropheMode {
    CharLiteral,
    SingleQuoteString,
}

/// Find the closing brace of a declaration without counting braces inside
/// strings, character literals or comments.
fn track_brace_end(lines: &[&str], start_idx: usize, apostrophes: ApostropheMode) -> usize {
    let mut depth: i32 = 0;
    let mut saw_open = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut block_comment = false;

    for (i, line) in lines.iter().enumerate().skip(start_idx) {
        let chars: Vec<char> = line.chars().collect();
        let mut k = 0usize;
        let mut line_comment = false;

        while k < chars.len() {
            let ch = chars[k];
            let next = chars.get(k + 1).copied();

            if line_comment {
                break;
            }
            if block_comment {
                if ch == '*' && next == Some('/') {
                    block_comment = false;
                    k += 2;
                } else {
                    k += 1;
                }
                continue;
            }
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
                k += 1;
                continue;
            }

            match ch {
                '/' if next == Some('/') => {
                    line_comment = true;
                    k += 2;
                }
                '/' if next == Some('*') => {
                    block_comment = true;
                    k += 2;
                }
                '"' => {
                    quote = Some('"');
                    k += 1;
                }
                '\'' => match apostrophes {
                    ApostropheMode::CharLiteral => {
                        if let Some(len) = char_literal_len(&chars, k) {
                            k += len;
                        } else {
                            k += 1;
                        }
                    }
                    ApostropheMode::SingleQuoteString => {
                        quote = Some('\'');
                        k += 1;
                    }
                },
                '{' => {
                    depth += 1;
                    saw_open = true;
                    k += 1;
                }
                '}' => {
                    depth -= 1;
                    k += 1;
                    if saw_open && depth <= 0 {
                        return i + 1;
                    }
                }
                _ => k += 1,
            }
        }

        if !saw_open && line.trim().ends_with(';') {
            return i + 1;
        }
    }

    if saw_open {
        lines.len()
    } else {
        start_idx + 1
    }
}

fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn track_python_indent_end(lines: &[&str], start_idx: usize, base_indent: usize) -> usize {
    let mut end_line = start_idx + 1;
    let mut in_docstring = false;
    let mut marker = "";

    for (i, line) in lines.iter().enumerate().skip(start_idx + 1) {
        let trimmed = line.trim();
        if in_docstring {
            end_line = i + 1;
            if trimmed.contains(marker) {
                in_docstring = false;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            marker = if trimmed.starts_with("\"\"\"") {
                "\"\"\""
            } else {
                "'''"
            };
            if trimmed.matches(marker).count() < 2 {
                in_docstring = true;
            }
            end_line = i + 1;
            continue;
        }
        if indent_width(line) <= base_indent {
            return end_line;
        }
        end_line = i + 1;
    }
    end_line
}

fn extract_rust_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = RUST_SYMBOL_RE.captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(2).map_or_else(
                || {
                    line.split_whitespace()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                |m| m.as_str().to_string(),
            );
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx, ApostropheMode::CharLiteral),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_ts_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = TS_SYMBOL_RE.captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let trimmed = line.trim_start();
            // Local `const`/`let`/`var` are not high-level declarations.
            if matches!(kind.as_str(), "const" | "let" | "var")
                && trimmed.len() != line.len()
                && !trimmed.starts_with("export")
            {
                continue;
            }
            let name = caps.get(2).map_or_else(
                || {
                    trimmed
                        .split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                |m| m.as_str().to_string(),
            );
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx, ApostropheMode::SingleQuoteString),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_py_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = PY_SYMBOL_RE.captures(line) {
            let indent_str = caps.get(1).map_or("", |m| m.as_str());
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(3).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_python_indent_end(&lines, idx, indent_width(indent_str)),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_go_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = GO_FUNC_RE.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind: "func".to_string(),
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx, ApostropheMode::CharLiteral),
                signature: line.trim().to_string(),
            });
        } else if let Some(caps) = GO_TYPE_RE.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line: track_brace_end(&lines, idx, ApostropheMode::CharLiteral),
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_symbols_for_file(path: &Path, content: &str) -> Vec<SymbolSpan> {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => extract_rust_symbols(content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => extract_ts_symbols(content),
        "py" => extract_py_symbols(content),
        "go" => extract_go_symbols(content),
        _ => Vec::new(),
    }
}

fn analyze_structure(
    dir_path: &Path,
    max_depth: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    if !dir_path.exists() {
        return Err(format!("Directory not found: {}", dir_path.display()));
    }
    if !dir_path.is_dir() {
        return Err(format!("Path is not a directory: {}", dir_path.display()));
    }

    let mut total_files: usize = 0;
    let mut total_dirs: usize = 0;
    let mut ext_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tree_lines = Vec::new();
    let mut truncated_walk = false;

    let walker = WalkBuilder::new(dir_path)
        .max_depth(Some(max_depth))
        .standard_filters(true)
        // .gitignore must apply even when the directory was exported without
        // its .git metadata, matching grep/find behaviour.
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    let root_canon = dir_path.canonicalize().unwrap_or_else(|_| dir_path.to_path_buf());

    for result in walker {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = entry.path();
        if p == dir_path {
            continue;
        }

        let rel = match p.strip_prefix(&root_canon) {
            Ok(r) => r,
            Err(_) => p.strip_prefix(dir_path).unwrap_or(p),
        };

        let depth = rel.components().count();
        let indent = "  ".repeat(depth.saturating_sub(1));
        let file_name = p.file_name().and_then(|f| f.to_str()).unwrap_or("");

        if tree_lines.len() >= MAX_WALK_ENTRIES {
            truncated_walk = true;
            break;
        }

        if p.is_dir() {
            total_dirs += 1;
            tree_lines.push(format!("{indent}├── {file_name}/"));
        } else {
            total_files += 1;
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("<none>");
            *ext_counts.entry(ext.to_string()).or_insert(0) += 1;
            tree_lines.push(format!("{indent}├── {file_name}"));
        }
    }

    let mut out = format!(
        "Directory: {}\nTotal: {} files, {} directories\n",
        dir_path.display(),
        total_files,
        total_dirs
    );

    if !ext_counts.is_empty() {
        let mut sorted_exts: Vec<(&String, &usize)> = ext_counts.iter().collect();
        sorted_exts.sort_by(|a, b| b.1.cmp(a.1));
        let ext_summary: Vec<String> = sorted_exts
            .into_iter()
            .take(8)
            .map(|(ext, count)| format!(".{ext} ({count})"))
            .collect();
        out.push_str(&format!("Extensions: {}\n", ext_summary.join(", ")));
    }

    out.push_str("\nTree hierarchy:\n");
    for line in tree_lines.iter().take(300) {
        out.push_str(line);
        out.push('\n');
    }
    if tree_lines.len() > 300 {
        out.push_str(&format!("... [{} more items not shown]\n", tree_lines.len() - 300));
    }
    if truncated_walk {
        out.push_str(&format!("... [stopped after {MAX_WALK_ENTRIES} entries]\n"));
    }

    Ok(out)
}

fn analyze_symbols(path: &Path, content: &str) -> Result<String, String> {
    let symbols = extract_symbols_for_file(path, content);
    if symbols.is_empty() {
        return Ok(format!(
            "No high-level declarations recognized for {}. Total lines: {}",
            path.display(),
            content.lines().count()
        ));
    }

    let mut out = format!("Symbols in {} (total: {}):\n", path.display(), symbols.len());
    for sym in symbols {
        if sym.start_line == sym.end_line {
            out.push_str(&format!("  L{}: {}\n", sym.start_line, sym.signature));
        } else {
            out.push_str(&format!(
                "  L{}-{}: {}\n",
                sym.start_line, sym.end_line, sym.signature
            ));
        }
    }
    Ok(out)
}

fn analyze_focus(path: &Path, content: &str, symbol: &str) -> Result<String, String> {
    if symbol.trim().is_empty() {
        return Err("Parameter 'symbol' is required for focus mode".to_string());
    }

    let symbols = extract_symbols_for_file(path, content);
    // An exact declaration name wins over a coincidental signature substring.
    let matched = symbols
        .iter()
        .find(|s| s.name == symbol)
        .or_else(|| symbols.iter().find(|s| s.signature.contains(symbol)));

    let lines: Vec<&str> = content.lines().collect();

    if let Some(target) = matched {
        let start = target.start_line.saturating_sub(1);
        let end = target.end_line.min(lines.len());
        let count = end.saturating_sub(start);
        let slice_len = count.min(MAX_FOCUS_LINES);

        let mut out = format!(
            "Focus on '{}' ({}) in {} (L{}-L{}):\n\n",
            target.name, target.kind, path.display(), target.start_line, end
        );
        for i in 0..slice_len {
            out.push_str(&format!("{:4} | {}\n", start + i + 1, lines[start + i]));
        }
        if count > MAX_FOCUS_LINES {
            out.push_str(&format!(
                "\n... [Truncated remaining {} lines of symbol '{}']\n",
                count - MAX_FOCUS_LINES,
                target.name
            ));
        }
        Ok(out)
    } else {
        let matching_lines: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(symbol))
            .take(10)
            .map(|(idx, line)| (idx + 1, *line))
            .collect();

        if matching_lines.is_empty() {
            Err(format!(
                "Symbol or keyword '{}' not found in {}",
                symbol,
                path.display()
            ))
        } else {
            let mut out = format!(
                "Symbol '{}' not recognized as declaration. Matching occurrences in {}:\n",
                symbol,
                path.display()
            );
            for (line_no, text) in matching_lines {
                out.push_str(&format!("{:4} | {}\n", line_no, text.trim()));
            }
            Ok(out)
        }
    }
}

async fn read_source_for_analysis(path: &Path, path_str: &str) -> Result<String, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Failed to stat file {}: {}", path_str, e))?;
    if meta.len() > MAX_ANALYZE_FILE_BYTES {
        return Err(format!(
            "File {} is too large to analyze ({} bytes exceeds {} byte limit)",
            path_str,
            meta.len(),
            MAX_ANALYZE_FILE_BYTES
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path_str, e))?;
    // The file may be up to MAX_ANALYZE_FILE_BYTES, so sniff the whole bounded
    // slice; a NUL past byte 1024 still means the file is binary.
    if bytes.contains(&0) {
        return Err(format!("File {} appears to be binary", path_str));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[async_trait]
impl AgentTool for AnalyzeTool {
    fn name(&self) -> &str {
        "analyze"
    }

    fn description(&self) -> &str {
        "High-level structural code analyzer with zero external dependencies. Supports 'symbols' (class/function/trait signatures and line spans), 'structure' (directory tree, file counts, and extension distribution), and 'focus' (extract implementation of a specific symbol)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file or directory to analyze (relative or absolute)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["symbols", "structure", "focus"],
                    "description": "Analysis mode: 'symbols', 'structure', or 'focus'"
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol name to focus on (required when mode is 'focus')"
                },
                "focus": {
                    "type": "string",
                    "description": "Alias for 'symbol'"
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum tree depth for 'structure' mode (1-5, default 2)"
                }
            },
            "required": ["path"]
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
        arguments: serde_json::Map<String, serde_json::Value>,
        cancellation_token: CancellationToken,
        _update_callback: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancellation_token.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let resolved_path = self.resolve_path(path_str)?;
        let mode = arguments
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                if resolved_path.is_dir() {
                    "structure"
                } else {
                    "symbols"
                }
            });

        let output = match mode {
            "structure" => {
                let depth = arguments
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2)
                    .clamp(1, 5) as usize;
                let path = resolved_path.clone();
                let token = cancellation_token.clone();
                tokio::task::spawn_blocking(move || analyze_structure(&path, depth, &token))
                    .await
                    .map_err(|e| format!("Analyze task failed: {e}"))??
            }
            "symbols" => {
                if !resolved_path.exists() {
                    return Err(format!("File not found: {}", resolved_path.display()));
                }
                if resolved_path.is_dir() {
                    return Err(format!(
                        "Path {} is a directory. Use mode 'structure' instead.",
                        resolved_path.display()
                    ));
                }
                let content = read_source_for_analysis(&resolved_path, path_str).await?;
                analyze_symbols(&resolved_path, &content)?
            }
            "focus" => {
                if !resolved_path.exists() {
                    return Err(format!("File not found: {}", resolved_path.display()));
                }
                let symbol = arguments
                    .get("symbol")
                    .or_else(|| arguments.get("focus"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = read_source_for_analysis(&resolved_path, path_str).await?;
                analyze_focus(&resolved_path, &content, symbol)?
            }
            unknown => {
                return Err(format!(
                    "Unknown analysis mode '{}'. Supported modes: 'symbols', 'structure', 'focus'",
                    unknown
                ));
            }
        };

        let output = truncate_head(
            &output,
            MAX_ANALYZE_CHARS,
            "\n... [Output truncated to 24000 characters. Use focus mode or specific file paths]",
        );
        Ok(AgentToolResult::text(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_focus_handles_rust_function_with_odd_number_of_lifetimes() {
        let content = "fn f<'a>(s: &'a str) -> &'a str {\n    s\n}\n";
        let out = analyze_focus(Path::new("lib.rs"), content, "f").expect("focus output");

        assert!(
            out.contains("(L1-L3):"),
            "focus span must cover the whole function, got: {out}"
        );
        assert!(out.contains("     s\n"), "body line missing from focus: {out}");
        assert!(out.contains("}\n"), "closing brace missing from focus: {out}");
    }

    #[test]
    fn analyze_focus_keeps_ts_single_quoted_string_braces_out_of_depth() {
        let content = "const open = 'function () {';\nfunction real() {\n  return 1;\n}\n";
        let out = analyze_focus(Path::new("app.ts"), content, "open").expect("focus output");
        assert!(
            out.contains("(L1-L1):"),
            "single-quoted TS string must not leave a brace open: {out}"
        );
    }

    #[tokio::test]
    async fn analyze_rejects_binary_with_nul_after_first_1024_bytes() {
        let dir = crate::TempTestDir::new("analyze_binary");
        let file = dir.path().join("mixed.rs");

        // First 1024 bytes are clean text; the NUL sits past the old sniff window.
        let mut data = b"pub fn visible_symbol() {}\n".to_vec();
        data.resize(2000, b' ');
        data.push(0);
        data.extend_from_slice(b"\npub fn after_nul() {}\n");
        std::fs::write(&file, &data).unwrap();

        let tool = AnalyzeTool::new(dir.path().to_path_buf());
        let mut args = serde_json::Map::new();
        args.insert("path".to_string(), serde_json::json!("mixed.rs"));
        args.insert("mode".to_string(), serde_json::json!("symbols"));

        let err = tool
            .execute("tc", args, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(
            err.contains("binary"),
            "a file with an embedded NUL must be refused as binary, got: {}",
            err
        );
    }
}
