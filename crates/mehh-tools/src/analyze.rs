use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use async_trait::async_trait;
use ignore::WalkBuilder;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use regex::Regex;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const MAX_ANALYZE_CHARS: usize = 24_000;
const MAX_FOCUS_LINES: usize = 250;

#[derive(Debug, Clone)]
pub struct AnalyzeTool {
    pub cwd: PathBuf,
}

impl AnalyzeTool {
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
struct SymbolSpan {
    name: String,
    kind: String,
    start_line: usize,
    end_line: usize,
    signature: String,
}

fn track_brace_end(lines: &[&str], start_idx: usize) -> usize {
    let mut depth: i32 = 0;
    let mut saw_open = false;

    for (i, line) in lines.iter().enumerate().skip(start_idx) {
        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                saw_open = true;
            } else if ch == '}' {
                depth -= 1;
                if saw_open && depth <= 0 {
                    return i + 1;
                }
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

fn track_python_indent_end(lines: &[&str], start_idx: usize, base_indent: usize) -> usize {
    let mut end_line = start_idx + 1;
    for (i, line) in lines.iter().enumerate().skip(start_idx + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= base_indent {
            return end_line;
        }
        end_line = i + 1;
    }
    end_line
}

fn extract_rust_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let re = Regex::new(
        r"^[ \t]*(?:pub(?:\([^)]+\))?[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+(?:\x22[^\x22]+\x22[ \t]+)?)?(fn|struct|enum|trait|impl|type|const|static)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    ).unwrap();

    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = re.captures(line) {
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
            let end_line = track_brace_end(&lines, idx);
            let sig = line.trim().to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line,
                signature: sig,
            });
        }
    }
    symbols
}

fn extract_ts_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let re = Regex::new(
        r"^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:async[ \t]+)?(function\*?|class|interface|type|const|let|var)\b(?:[ \t]+([A-Za-z0-9_]+))?",
    ).unwrap();

    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = re.captures(line) {
            let kind = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(2).map_or_else(
                || {
                    line.split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                },
                |m| m.as_str().to_string(),
            );
            let end_line = track_brace_end(&lines, idx);
            let sig = line.trim().to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line,
                signature: sig,
            });
        }
    }
    symbols
}

fn extract_py_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let re = Regex::new(r"^([ \t]*)(?:async[ \t]+)?(def|class)[ \t]+([A-Za-z0-9_]+)").unwrap();

    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = re.captures(line) {
            let indent_str = caps.get(1).map_or("", |m| m.as_str());
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let name = caps.get(3).map_or("", |m| m.as_str()).to_string();
            let end_line = track_python_indent_end(&lines, idx, indent_str.len());
            let sig = line.trim().to_string();
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line,
                signature: sig,
            });
        }
    }
    symbols
}

fn extract_go_symbols(content: &str) -> Vec<SymbolSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let re_func = Regex::new(r"^[ \t]*func\b(?:[ \t]+\([^)]+\))?[ \t]+([A-Za-z0-9_]+)").unwrap();
    let re_type = Regex::new(r"^[ \t]*type[ \t]+([A-Za-z0-9_]+)[ \t]+(struct|interface)").unwrap();

    let mut symbols = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = re_func.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let end_line = track_brace_end(&lines, idx);
            symbols.push(SymbolSpan {
                name,
                kind: "func".to_string(),
                start_line: idx + 1,
                end_line,
                signature: line.trim().to_string(),
            });
        } else if let Some(caps) = re_type.captures(line) {
            let name = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let kind = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let end_line = track_brace_end(&lines, idx);
            symbols.push(SymbolSpan {
                name,
                kind,
                start_line: idx + 1,
                end_line,
                signature: line.trim().to_string(),
            });
        }
    }
    symbols
}

fn extract_symbols_for_file(path: &Path, content: &str) -> Vec<SymbolSpan> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => extract_rust_symbols(content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => extract_ts_symbols(content),
        "py" => extract_py_symbols(content),
        "go" => extract_go_symbols(content),
        _ => Vec::new(),
    }
}

fn analyze_structure(dir_path: &Path, max_depth: usize) -> Result<String, String> {
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

    let walker = WalkBuilder::new(dir_path)
        .max_depth(Some(max_depth))
        .standard_filters(true)
        .build();

    let root_canon = dir_path.canonicalize().unwrap_or_else(|_| dir_path.to_path_buf());

    for result in walker {
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
            Err(_) => match p.strip_prefix(dir_path) {
                Ok(r) => r,
                Err(_) => p,
            },
        };

        let depth = rel.components().count();
        let indent = "  ".repeat(depth.saturating_sub(1));
        let file_name = p.file_name().and_then(|f| f.to_str()).unwrap_or("");

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

    let mut out = format!(
        "Symbols in {} (total: {}):\n",
        path.display(),
        symbols.len()
    );
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
    let matched = symbols.iter().find(|s| s.name == symbol || s.signature.contains(symbol));

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
        // Fallback: search lines for symbol string
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
                    "description": "Analysis mode: 'symbols' for high-level declarations in a file, 'structure' for directory hierarchy overview, 'focus' for extracting the implementation of a specific symbol"
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
        _cancellation_token: CancellationToken,
        _update_callback: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let path_str = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'path'".to_string())?;

        let resolved_path = self.resolve_path(path_str);
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

        let mut output = match mode {
            "structure" => {
                let depth = arguments
                    .get("depth")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2)
                    .clamp(1, 5) as usize;
                analyze_structure(&resolved_path, depth)?
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
                let bytes = tokio::fs::read(&resolved_path)
                    .await
                    .map_err(|e| format!("Failed to read file {}: {}", resolved_path.display(), e))?;
                let content = String::from_utf8_lossy(&bytes);
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
                let bytes = tokio::fs::read(&resolved_path)
                    .await
                    .map_err(|e| format!("Failed to read file {}: {}", resolved_path.display(), e))?;
                let content = String::from_utf8_lossy(&bytes);
                analyze_focus(&resolved_path, &content, symbol)?
            }
            unknown => {
                return Err(format!(
                    "Unknown analysis mode '{}'. Supported modes: 'symbols', 'structure', 'focus'",
                    unknown
                ));
            }
        };

        if output.len() > MAX_ANALYZE_CHARS {
            output.truncate(MAX_ANALYZE_CHARS);
            output.push_str("\n... [Output truncated to 24000 characters. Use focus mode or specific file paths]");
        }

        Ok(AgentToolResult::text(output))
    }
}
