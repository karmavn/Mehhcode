use std::path::PathBuf;

use async_trait::async_trait;
use base64::prelude::*;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, ToolResultContentBlock};
use serde_json::json;

use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};

const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TEXT_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_READ_LINES: usize = 2000;
const MAX_READ_OUTPUT_BYTES: usize = 100 * 1024;
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct ReadTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl ReadTool {
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

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read file contents. Supports text files and images (png, jpg, jpeg, gif, webp). Text output is bounded by line count and total bytes; binary files are rejected."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (max 2000)"
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

        let offset = params
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(MAX_READ_LINES as i64)
            .clamp(1, MAX_READ_LINES as i64) as usize;

        let full_path = self.resolve_path(path_str)?;

        let meta = match tokio::fs::metadata(&full_path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("File not found: {}", path_str));
            }
            Err(e) => return Err(format!("Failed to stat file {}: {}", path_str, e)),
        };
        if meta.is_dir() {
            return Err(format!("Path is a directory: {}", path_str));
        }

        let ext = full_path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let image_mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };

        if let Some(mime_type) = image_mime {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                return Err(format!(
                    "Image file {} is too large ({} bytes exceeds {} byte limit)",
                    path_str,
                    meta.len(),
                    MAX_IMAGE_FILE_BYTES
                ));
            }
            let bytes = read_file_bounded(&full_path, MAX_IMAGE_FILE_BYTES, path_str, true).await?;
            if bytes.is_empty() {
                return Ok(AgentToolResult::text(""));
            }
            return Ok(AgentToolResult {
                content: vec![ToolResultContentBlock::Image(ImageContent {
                    data: BASE64_STANDARD.encode(&bytes),
                    mime_type: mime_type.to_string(),
                })],
                details: None,
                ..Default::default()
            });
        }

        if meta.len() > MAX_TEXT_FILE_BYTES {
            return Err(format!(
                "File {} is too large ({} bytes exceeds {} byte limit)",
                path_str,
                meta.len(),
                MAX_TEXT_FILE_BYTES
            ));
        }

        // Stream in 64 KiB chunks so large files with small offset/limit never
        // allocate 50 MiB in RAM and can cancel immediately mid-read.
        let text_res = read_text_streaming(
            &full_path,
            path_str,
            offset,
            limit,
            &cancel,
        )
        .await?;
        let Some(output) = text_res else {
            return Ok(AgentToolResult::text(""));
        };
        Ok(AgentToolResult::text(output))
    }
}

struct LineAccumulator {
    start_line: usize,
    stop_line: usize,
    line_no: usize,
    total_lines: usize,
    shown_lines: usize,
    output_bytes: usize,
    output_capped: bool,
    output: String,
    cur_line: String,
    cur_chars: usize,
    cur_truncated: bool,
    cur_cr_pending: bool,
    cur_has_data: bool,
    at_start: bool,
}

impl LineAccumulator {
    fn new(offset: usize, limit: usize) -> Self {
        let start_line = offset - 1;
        let stop_line = start_line.saturating_add(limit);
        Self {
            start_line,
            stop_line,
            line_no: 0,
            total_lines: 0,
            shown_lines: 0,
            output_bytes: 0,
            output_capped: false,
            output: String::new(),
            cur_line: String::new(),
            cur_chars: 0,
            cur_truncated: false,
            cur_cr_pending: false,
            cur_has_data: false,
            at_start: true,
        }
    }

    fn in_window(&self) -> bool {
        !self.output_capped && self.line_no >= self.start_line && self.line_no < self.stop_line
    }

    fn push_text_fragment(&mut self, frag: &str) {
        if frag.is_empty() || !self.in_window() || self.cur_truncated {
            return;
        }
        let rem = MAX_LINE_CHARS.saturating_sub(self.cur_chars);
        if rem == 0 {
            self.cur_line.push_str("... [truncated]");
            self.cur_truncated = true;
            return;
        }
        match frag.char_indices().nth(rem) {
            Some((cut, _)) => {
                self.cur_line.push_str(&frag[..cut]);
                self.cur_line.push_str("... [truncated]");
                self.cur_chars = MAX_LINE_CHARS;
                self.cur_truncated = true;
            }
            None => {
                self.cur_line.push_str(frag);
                self.cur_chars += frag.chars().count();
            }
        }
    }

    fn finish_line(&mut self) {
        self.total_lines += 1;
        if self.in_window() {
            let line_len = self.cur_line.len();
            if self.output_bytes + line_len + 1 > MAX_READ_OUTPUT_BYTES {
                self.output_capped = true;
            } else {
                if self.shown_lines > 0 {
                    self.output.push('\n');
                }
                self.output.push_str(&self.cur_line);
                self.output_bytes += line_len + 1;
                self.shown_lines += 1;
            }
        }
        self.line_no += 1;
        self.cur_line.clear();
        self.cur_chars = 0;
        self.cur_truncated = false;
        self.cur_cr_pending = false;
        self.cur_has_data = false;
    }

    fn feed_str(&mut self, mut s: &str) {
        if self.at_start && !s.is_empty() {
            self.at_start = false;
            if let Some(stripped) = s.strip_prefix('\u{feff}') {
                self.cur_has_data = true;
                s = stripped;
            }
        }
        if s.is_empty() {
            return;
        }
        if self.output_capped || self.line_no >= self.stop_line {
            let newlines = s.as_bytes().iter().filter(|&&b| b == b'\n').count();
            self.total_lines += newlines;
            self.line_no += newlines;
            self.cur_has_data = !s.ends_with('\n');
            return;
        }
        if self.cur_cr_pending {
            self.cur_cr_pending = false;
            if !s.starts_with('\n') {
                self.push_text_fragment("\r");
            }
        }
        for piece in s.split_inclusive('\n') {
            self.cur_has_data = true;
            if let Some(without_nl) = piece.strip_suffix('\n') {
                let clean = without_nl.strip_suffix('\r').unwrap_or(without_nl);
                self.push_text_fragment(clean);
                self.finish_line();
            } else if let Some(without_cr) = piece.strip_suffix('\r') {
                self.push_text_fragment(without_cr);
                self.cur_cr_pending = true;
            } else {
                self.push_text_fragment(piece);
            }
        }
    }

    fn finalize(mut self, offset: usize) -> Result<String, String> {
        if self.cur_cr_pending {
            self.cur_cr_pending = false;
            self.push_text_fragment("\r");
        }
        if self.cur_has_data {
            self.finish_line();
        }
        if self.start_line >= self.total_lines && self.total_lines > 0 {
            return Err(format!(
                "Offset {} is beyond end of file ({} lines total)",
                offset, self.total_lines
            ));
        }

        let end_line = self.start_line + self.shown_lines;
        if end_line < self.total_lines {
            use std::fmt::Write as _;
            let _ = write!(
                self.output,
                "\n\n[Showing lines {}-{} of {}. Use offset={} to read more.]",
                self.start_line + 1,
                end_line,
                self.total_lines,
                end_line + 1
            );
        } else if self.start_line > 0 {
            use std::fmt::Write as _;
            let _ = write!(
                self.output,
                "\n\n[Showing lines {}-{} of {}.]",
                self.start_line + 1,
                end_line,
                self.total_lines
            );
        }
        if self.output_capped {
            use std::fmt::Write as _;
            let _ = write!(
                self.output,
                "\n\n[Output truncated at {} byte limit]",
                MAX_READ_OUTPUT_BYTES
            );
        }
        Ok(self.output)
    }
}

async fn read_text_streaming(
    path: &std::path::Path,
    path_str: &str,
    offset: usize,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Option<String>, String> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("Failed to read {}: {}", path_str, e))?;
    let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total_bytes: u64 = 0;
    let mut carry = [0u8; 4];
    let mut carry_len = 0usize;
    let mut acc = LineAccumulator::new(offset, limit);

    loop {
        let n = tokio::select! {
            _ = cancel.cancelled() => return Err("Tool execution cancelled".to_string()),
            res = reader.read(&mut buf) => {
                res.map_err(|e| format!("Failed to read {}: {}", path_str, e))?
            }
        };
        if n == 0 {
            break;
        }
        total_bytes += n as u64;
        if total_bytes > MAX_TEXT_FILE_BYTES {
            return Err(format!(
                "File {} is too large (exceeds {} byte limit)",
                path_str, MAX_TEXT_FILE_BYTES
            ));
        }
        let chunk = &buf[..n];
        if chunk.contains(&0) {
            return Err(format!("File {} appears to be binary", path_str));
        }

        let mut start = 0usize;
        if carry_len > 0 {
            let need = (4 - carry_len).min(chunk.len());
            carry[carry_len..carry_len + need].copy_from_slice(&chunk[..need]);
            let combined_len = carry_len + need;
            match std::str::from_utf8(&carry[..combined_len]) {
                Ok(s) => {
                    acc.feed_str(s);
                    carry_len = 0;
                    start = need;
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let valid = e.valid_up_to();
                    let s = std::str::from_utf8(&carry[..valid]).map_err(|_| {
                        format!("File {} is not valid UTF-8 and appears to be binary", path_str)
                    })?;
                    acc.feed_str(s);
                    let consumed_from_chunk = valid.saturating_sub(carry_len);
                    carry_len = 0;
                    start = consumed_from_chunk;
                }
                Err(e) => {
                    if e.error_len().is_some() {
                        return Err(format!(
                            "File {} is not valid UTF-8 and appears to be binary",
                            path_str
                        ));
                    }
                    carry_len = combined_len;
                    continue;
                }
            }
        }

        let rest = &chunk[start..];
        if !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(s) => acc.feed_str(s),
                Err(e) => {
                    if e.error_len().is_some() {
                        return Err(format!(
                            "File {} is not valid UTF-8 and appears to be binary",
                            path_str
                        ));
                    }
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s = std::str::from_utf8(&rest[..valid]).map_err(|_| {
                            format!(
                                "File {} is not valid UTF-8 and appears to be binary",
                                path_str
                            )
                        })?;
                        acc.feed_str(s);
                    }
                    let rem = &rest[valid..];
                    carry[..rem.len()].copy_from_slice(rem);
                    carry_len = rem.len();
                }
            }
        }
    }

    if carry_len > 0 {
        return Err(format!(
            "File {} is not valid UTF-8 and appears to be binary",
            path_str
        ));
    }
    if total_bytes == 0 {
        return Ok(None);
    }
    acc.finalize(offset).map(Some)
}

async fn read_reader_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    max_bytes: u64,
    path_str: &str,
    is_image: bool,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .await
        .map_err(|e| {
            if is_image {
                format!("Failed to read image file {}: {}", path_str, e)
            } else {
                format!("Failed to read {}: {}", path_str, e)
            }
        })?;
    if buf.len() as u64 > max_bytes {
        return Err(if is_image {
            format!(
                "Image file {} is too large (exceeds {} byte limit)",
                path_str, max_bytes
            )
        } else {
            format!(
                "File {} is too large (exceeds {} byte limit)",
                path_str, max_bytes
            )
        });
    }
    Ok(buf)
}

async fn read_file_bounded(
    path: &std::path::Path,
    max_bytes: u64,
    path_str: &str,
    is_image: bool,
) -> Result<Vec<u8>, String> {
    let file = tokio::fs::File::open(path).await.map_err(|e| {
        if is_image {
            format!("Failed to read image file {}: {}", path_str, e)
        } else {
            format!("Failed to read {}: {}", path_str, e)
        }
    })?;
    read_reader_bounded(file, max_bytes, path_str, is_image).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_rejects_nul_beyond_first_kb() {
        let dir = crate::TempTestDir::new("read_binary_nul");
        let path = dir.path().join("data.txt");
        let mut data = vec![b' '; 2000];
        data.push(0);
        data.extend_from_slice(b"text after nul");
        std::fs::write(&path, &data).unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let params = json!({ "path": "data.txt" }).as_object().unwrap().clone();
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
    async fn test_read_offset_to_eof_does_not_suggest_beyond_eof_offset() {
        let dir = crate::TempTestDir::new("read_offset_eof");
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, "l1\nl2\nl3\nl4\nl5\n").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let eof_params = json!({ "path": "lines.txt", "offset": 3, "limit": 10 })
            .as_object()
            .unwrap()
            .clone();
        let res = tool
            .execute("tc1", eof_params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("[Showing lines 3-5 of 5.]"), "got: {txt}");
        assert!(!txt.contains("Use offset="), "got: {txt}");

        let mid_params = json!({ "path": "lines.txt", "offset": 2, "limit": 2 })
            .as_object()
            .unwrap()
            .clone();
        let res_mid = tool
            .execute("tc2", mid_params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt_mid = match &res_mid.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            txt_mid.contains("[Showing lines 2-3 of 5. Use offset=4 to read more.]"),
            "got: {txt_mid}"
        );
    }

    #[tokio::test]
    async fn test_read_bounded_and_empty_file() {
        let dir = crate::TempTestDir::new("read_bounded_empty");
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, b"").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let params = json!({ "path": "empty.txt" }).as_object().unwrap().clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(txt, "");

        let data = b"virtual-proc-content";
        let ok = read_reader_bounded(&data[..], 64, "/proc/mock", false)
            .await
            .unwrap();
        assert_eq!(ok, data);

        let err = read_reader_bounded(&data[..], 5, "/proc/mock", false)
            .await
            .unwrap_err();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[tokio::test]
    async fn test_read_streaming_across_64kb_chunk_boundaries() {
        let dir = crate::TempTestDir::new("read_stream_64k");
        let path = dir.path().join("chunked.txt");
        // Place multi-byte UTF-8 and CRLF spanning the 65536-byte chunk boundary.
        let mut content = String::from("\u{feff}");
        content.push_str(&"a".repeat(65532));
        content.push_str("é\r\nline2\r\n");
        std::fs::write(&path, content.as_bytes()).unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let params = json!({ "path": "chunked.txt", "offset": 1, "limit": 2 })
            .as_object()
            .unwrap()
            .clone();
        let res = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(
            txt,
            format!("{}... [truncated]\nline2", "a".repeat(MAX_LINE_CHARS)),
            "got: {txt}"
        );
    }
}
