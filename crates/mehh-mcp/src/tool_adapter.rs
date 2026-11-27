//! MCP tool adapter exposing MCP tools as `mehh_agent::AgentTool` implementations.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use async_trait::async_trait;
use anyhow::Context;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, TextContent, ToolResultContentBlock};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::client::{
    McpCallToolResult, McpClient, McpContentBlock, McpRequestOptions, McpToolInfo,
};

pub type McpOutputSaver = Arc<
    dyn Fn(Vec<u8>, String) -> futures::future::BoxFuture<'static, anyhow::Result<PathBuf>>
        + Send
        + Sync,
>;

/// Persists raw MCP output or binary blob to a private temporary file.
fn sanitize_output_ext(raw: &str) -> String {
    let cleaned: String = raw
        .trim_start_matches('.')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        ".bin".to_string()
    } else {
        format!(".{cleaned}")
    }
}

fn cleanup_old_mcp_outputs_with_cutoff(dir: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("mcp-output-") {
            continue;
        }
        let path = entry.path();
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_file() && meta.modified().is_ok_and(|m| m < cutoff) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn cleanup_old_mcp_outputs(dir: &Path) {
    if let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(24 * 3600)) {
        cleanup_old_mcp_outputs_with_cutoff(dir, cutoff);
    }
}

fn mcp_output_base_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").filter(|v| !v.to_string_lossy().trim().is_empty())
        });
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .or_else(|| {
            std::env::var_os("USERPROFILE").filter(|v| !v.to_string_lossy().trim().is_empty())
        });
    if let Some(h) = home {
        PathBuf::from(h).join(".mehh").join("tmp").join("mcp")
    } else {
        std::env::temp_dir().join(format!("mehh-mcp-{}", std::process::id()))
    }
}

fn save_mcp_output_file_in_dir(
    base_dir: &Path,
    data: &[u8],
    ext: &str,
) -> anyhow::Result<PathBuf> {
    if data.len() > MCP_DISK_WRITE_MAX_BYTES {
        anyhow::bail!(
            "MCP output size ({} bytes) exceeds {} byte disk write limit",
            data.len(),
            MCP_DISK_WRITE_MAX_BYTES
        );
    }

    if let Ok(meta) = std::fs::symlink_metadata(base_dir) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "Refusing to write MCP output into symlinked directory '{}'",
                base_dir.display()
            );
        }
    }
    std::fs::create_dir_all(base_dir)?;
    let dir_meta = std::fs::symlink_metadata(base_dir)?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        anyhow::bail!(
            "MCP output target directory '{}' is not a real directory",
            base_dir.display()
        );
    }

    let clean_ext = sanitize_output_ext(ext);
    let filename = format!("mcp-output-{}{clean_ext}", uuid::Uuid::new_v4());
    let file_path = base_dir.join(filename);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&file_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let file_meta = file.metadata()?;
        if dir_meta.uid() != file_meta.uid() {
            drop(file);
            let _ = std::fs::remove_file(&file_path);
            anyhow::bail!(
                "Refusing MCP output directory '{}' owned by another user",
                base_dir.display()
            );
        }
        std::fs::set_permissions(base_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    use std::io::Write;
    if let Err(e) = file.write_all(data).and_then(|()| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(&file_path);
        return Err(e.into());
    }
    drop(file);

    cleanup_old_mcp_outputs(base_dir);

    Ok(file_path)
}

pub fn save_mcp_output_file(data: &[u8], ext: &str) -> anyhow::Result<PathBuf> {
    save_mcp_output_file_in_dir(&mcp_output_base_dir(), data, ext)
}

async fn decode_base64_maybe_blocking(blob: &str) -> anyhow::Result<Vec<u8>> {
    const BLOCKING_THRESHOLD_BYTES: usize = 64 * 1024;
    if blob.len() > BLOCKING_THRESHOLD_BYTES {
        let owned = blob.to_string();
        tokio::task::spawn_blocking(move || {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, owned)
        })
        .await
        .map_err(|e| anyhow::anyhow!("base64 decode task failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("{e}"))
    } else {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

fn is_text_mime_type(mime: &str) -> bool {
    let clean = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    clean.starts_with("text/")
        || clean == "application/json"
        || clean.ends_with("+json")
        || clean.ends_with("+xml")
}

pub const MAX_TOOL_NAME_LENGTH: usize = 64;
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4096;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_PROGRESS_MESSAGE_BYTES: usize = 512;
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024; // 20KB model-facing limit
pub const MCP_RESOURCE_MAX_BYTES: usize = 100 * 1024; // 100KB cap for resources/images
pub const MCP_DISK_WRITE_MAX_BYTES: usize = 10 * 1024 * 1024; // 10MB cap for binary/output disk writes

/// Async wrapper so filesystem work never blocks a Tokio worker thread.
pub async fn save_mcp_output_file_async(data: Vec<u8>, ext: String) -> anyhow::Result<PathBuf> {
    tokio::task::spawn_blocking(move || {
        let base_dir = mcp_output_base_dir();
        save_mcp_output_file_in_dir(&base_dir, &data, &ext)
    })
    .await
    .context("MCP output save task panicked")?
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn strip_ansi_control_and_bidi(raw: &str, preserve_newlines_tabs: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                // CSI sequence: ESC [ <params/intermediates> <final byte 0x40..=0x7E>
                Some('[') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&nc) {
                            break;
                        }
                    }
                }
                // OSC / DCS / SOS / PM / APC: terminated by BEL, ST (ESC \), or C1 ST (0x9C)
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    while let Some(nc) = chars.next() {
                        if nc == '\x07' || nc == '\u{009C}' {
                            break;
                        }
                        if nc == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Other 2-byte ESC sequence
                Some(nc) if (' '..='~').contains(&nc) => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        // C1 CSI (0x9B)
        if c == '\u{009B}' {
            while let Some(&nc) = chars.peek() {
                chars.next();
                if ('@'..='~').contains(&nc) {
                    break;
                }
            }
            continue;
        }
        // C1 OSC (0x9D)
        if c == '\u{009D}' {
            while let Some(nc) = chars.next() {
                if nc == '\x07' || nc == '\u{009C}' {
                    break;
                }
                if nc == '\x1b' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        if is_bidi_control(c) {
            continue;
        }
        let code = c as u32;
        if (preserve_newlines_tabs && (c == '\n' || c == '\t'))
            || (code >= 0x20 && code != 0x7f && !(0x80..=0x9f).contains(&code))
        {
            out.push(c);
        }
    }
    out
}

fn truncate_utf8_with_ellipsis(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Sanitizes untrusted ANSI/CSI/OSC sequences, bidi overrides, and control characters,
/// capping length on a UTF-8 char boundary.
pub fn sanitize_mcp_text(raw: &str, max_bytes: usize) -> String {
    let cleaned = strip_ansi_control_and_bidi(raw, true);
    truncate_utf8_with_ellipsis(cleaned, max_bytes)
}

/// Sanitizes single-line progress notification messages from untrusted MCP servers.
pub fn sanitize_progress_message(raw: &str) -> String {
    let cleaned = strip_ansi_control_and_bidi(raw, false);
    truncate_utf8_with_ellipsis(cleaned, MAX_PROGRESS_MESSAGE_BYTES)
}

fn sanitize_schema_value(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::String(s) => {
            *s = sanitize_mcp_text(s, MAX_TOOL_DESCRIPTION_BYTES);
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                sanitize_schema_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                sanitize_schema_value(v);
            }
        }
        _ => {}
    }
}

fn strip_schema_descriptions(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Array(items) => {
            for item in items {
                strip_schema_descriptions(item);
            }
        }
        serde_json::Value::Object(map) => {
            map.remove("description");
            for value in map.values_mut() {
                strip_schema_descriptions(value);
            }
        }
        _ => {}
    }
}

struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_byte_len(value: &serde_json::Value) -> Option<usize> {
    let mut writer = CountingWriter(0);
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.0)
}

fn schema_too_big(schema: &serde_json::Value) -> bool {
    json_byte_len(schema)
        .map(|len| len > MAX_TOOL_SCHEMA_BYTES)
        .unwrap_or(true)
}

fn sanitize_input_schema(mut schema: serde_json::Value) -> serde_json::Value {
    sanitize_schema_value(&mut schema);
    if !schema_too_big(&schema) {
        return schema;
    }
    // Descriptions are usually most of the bulk. Drop them before losing the
    // actual parameter shape.
    strip_schema_descriptions(&mut schema);
    if !schema_too_big(&schema) {
        return schema;
    }
    // Keep type, required and property names so the model can still call the
    // tool instead of guessing against an empty `{}` schema, while strictly
    // enforcing `MAX_TOOL_SCHEMA_BYTES`.
    let mut minimal = serde_json::Map::new();
    if let Some(kind) = schema.get("type").filter(|v| v.is_string()) {
        minimal.insert("type".to_string(), kind.clone());
    }
    minimal.insert(
        "description".to_string(),
        serde_json::json!("[schema truncated: original exceeded size limit]"),
    );
    if let Some(serde_json::Value::Object(props)) = schema.get("properties") {
        let mut kept = serde_json::Map::new();
        const MAX_FALLBACK_PROPERTIES: usize = 256;
        let mut approx_bytes = 256usize;
        for (name, prop) in props.iter().take(MAX_FALLBACK_PROPERTIES) {
            let mut slim = serde_json::Map::new();
            if let Some(kind) = prop.get("type").filter(|v| v.is_string()) {
                slim.insert("type".to_string(), kind.clone());
            }
            let entry_cost = name.len().saturating_add(32);
            if approx_bytes.saturating_add(entry_cost) > MAX_TOOL_SCHEMA_BYTES / 2 {
                break;
            }
            approx_bytes = approx_bytes.saturating_add(entry_cost);
            kept.insert(name.clone(), serde_json::Value::Object(slim));
        }
        minimal.insert("properties".to_string(), serde_json::Value::Object(kept));
    }
    if let Some(serde_json::Value::Array(req)) = schema.get("required") {
        let kept_props = minimal.get("properties").and_then(|p| p.as_object());
        let filtered_req: Vec<serde_json::Value> = req
            .iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|s| kept_props.is_some_and(|kp| kp.contains_key(s)))
            })
            .cloned()
            .collect();
        if !filtered_req.is_empty() {
            minimal.insert("required".to_string(), serde_json::Value::Array(filtered_req));
        }
    }
    let fallback = serde_json::Value::Object(minimal);
    if !schema_too_big(&fallback) {
        return fallback;
    }
    serde_json::json!({
        "type": "object",
        "description": "[schema truncated: original exceeded size limit]"
    })
}

/// Sanitizes and formats an MCP tool into `mcp__<server>__<tool>`.
/// When length exceeds 64 characters, shortens to 55 characters + `_` + 8-char SHA-256 suffix.
pub fn create_mcp_tool_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();

    if sanitized.len() <= MAX_TOOL_NAME_LENGTH {
        sanitized
    } else {
        let mut hasher = Sha256::new();
        hasher.update(format!("{server}\0{tool}").as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let hash_prefix = &hash[..8];
        let prefix = &sanitized[..55];
        format!("{prefix}_{hash_prefix}")
    }
}

/// Generates a sanitized `mcp__<server>__<tool>` name guaranteed unique against `used_names`.
pub fn resolve_unique_mcp_tool_name(
    server: &str,
    tool: &str,
    used_names: &mut HashSet<String>,
) -> String {
    let base = create_mcp_tool_name(server, tool);
    if used_names.insert(base.clone()) {
        return base;
    }

    let mut suffix_num = 2usize;
    loop {
        let suffix = format!("_{suffix_num}");
        let max_prefix_len = MAX_TOOL_NAME_LENGTH.saturating_sub(suffix.len());
        let mut prefix_len = max_prefix_len.min(base.len());
        while prefix_len > 0 && !base.is_char_boundary(prefix_len) {
            prefix_len -= 1;
        }
        let candidate = format!("{}{}", &base[..prefix_len], suffix);
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
        suffix_num += 1;
    }
}

/// Builds collision-free `McpAgentTool` adapters, sharing `used_names` across
/// all servers registered through this function so sanitized names cannot shadow
/// one another (for example `svc-a` and `svc_a` both map to `mcp__svc_a__*`).
pub fn create_mcp_agent_tools_with_shared_names(
    server_name: &str,
    tools: Vec<McpToolInfo>,
    client: Arc<McpClient>,
    used_names: &mut HashSet<String>,
) -> Vec<Arc<dyn AgentTool>> {
    create_mcp_agent_tools_with_shared_names_and_timeout(
        server_name,
        tools,
        client,
        used_names,
        None,
    )
}

/// Builds collision-free `McpAgentTool` adapters with an explicit tool invocation timeout.
pub fn create_mcp_agent_tools_with_shared_names_and_timeout(
    server_name: &str,
    tools: Vec<McpToolInfo>,
    client: Arc<McpClient>,
    used_names: &mut HashSet<String>,
    timeout: Option<Duration>,
) -> Vec<Arc<dyn AgentTool>> {
    tools
        .into_iter()
        .map(|info| {
            let unique_name = resolve_unique_mcp_tool_name(server_name, &info.name, used_names);
            Arc::new(
                McpAgentTool::with_resolved_name(server_name, info, client.clone(), unique_name)
                    .with_timeout(timeout),
            ) as Arc<dyn AgentTool>
        })
        .collect()
}

/// Builds collision-free `McpAgentTool` adapters for a list of tools from one server.
pub fn create_mcp_agent_tools(
    server_name: &str,
    tools: Vec<McpToolInfo>,
    client: Arc<McpClient>,
) -> Vec<Arc<dyn AgentTool>> {
    let mut used = HashSet::new();
    create_mcp_agent_tools_with_shared_names(server_name, tools, client, &mut used)
}

/// Computes the server namespace `mcp__<server>` with hyphens replaced by underscores.
pub fn mcp_namespace(server: &str) -> String {
    let sanitized: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    format!("mcp__{sanitized}")
}

/// Truncates large text in the middle, preserving the head and tail with a marker and token counts.
pub fn truncate_middle(text: &str, max_bytes: usize) -> Option<String> {
    if text.len() <= max_bytes {
        return None;
    }
    let total_bytes = text.len();
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = total_bytes.saturating_sub(max_bytes - max_bytes / 2);
    while tail_start < total_bytes && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    let removed_chars = text[head_end..tail_start].chars().count();
    let lines = text.split('\n').count() - if text.ends_with('\n') { 1 } else { 0 };
    let tokens = total_bytes.div_ceil(4);

    Some(format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {lines}\n\n{head}…{removed_chars} chars truncated…{tail}"
    ))
}

/// Caps oversized strings and total serialized size inside a `details` payload before it reaches the model.
fn truncate_details_inner(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(truncated) = truncate_middle(s, MCP_OUTPUT_MAX_BYTES) {
                *s = truncated;
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                truncate_details_inner(item);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                truncate_details_inner(item);
            }
        }
        _ => {}
    }
}

fn truncate_details(value: &mut serde_json::Value) {
    truncate_details_inner(value);
    if let Some(len) = json_byte_len(value) {
        if len > MCP_OUTPUT_MAX_BYTES * 2 {
            *value = serde_json::json!({
                "truncated": true,
                "originalBytes": len
            });
        }
    }
}

fn push_capped_image(
    data: String,
    mime_type: String,
    images: &mut Vec<ToolResultContentBlock>,
    text_parts: &mut Vec<String>,
    total_image_bytes: &mut usize,
) {
    let new_total = total_image_bytes.saturating_add(data.len());
    if new_total > MCP_RESOURCE_MAX_BYTES {
        text_parts.push(format!(
            "[image {mime_type} omitted: {} bytes exceeds {} byte cap]",
            data.len(),
            MCP_RESOURCE_MAX_BYTES
        ));
    } else {
        *total_image_bytes = new_total;
        images.push(ToolResultContentBlock::Image(ImageContent {
            data,
            mime_type,
        }));
    }
}

/// Converts an MCP tool call result into an `AgentToolResult` conforming to `mehh_agent`,
/// persisting oversized text (>20KB) and non-image binary resource blobs to disk.
pub async fn convert_mcp_result_with_saver(
    call_res: McpCallToolResult,
    saver: Option<McpOutputSaver>,
) -> AgentToolResult {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut total_image_bytes = 0usize;

    for block in call_res.content {
        match block {
            McpContentBlock::Text { text } => {
                text_parts.push(text);
            }
            McpContentBlock::Image { data, mime_type } => {
                push_capped_image(data, mime_type, &mut images, &mut text_parts, &mut total_image_bytes);
            }
            McpContentBlock::Audio { mime_type, .. } => {
                text_parts.push(format!("[audio {mime_type} omitted]"));
            }
            McpContentBlock::ResourceLink { name, uri, .. } => {
                text_parts.push(format!("{name}: {uri}"));
            }
            McpContentBlock::Resource { resource } => {
                if let Some(text) = resource.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                } else if let Some(blob) = resource.get("blob").and_then(|b| b.as_str()) {
                    let mime = resource
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        push_capped_image(
                            blob.to_string(),
                            mime.to_string(),
                            &mut images,
                            &mut text_parts,
                            &mut total_image_bytes,
                        );
                    } else {
                        let raw_uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                        let safe_uri = sanitize_mcp_text(raw_uri, 512);
                        let max_b64_len = MCP_DISK_WRITE_MAX_BYTES.div_ceil(3).saturating_mul(4);
                        if blob.len() > max_b64_len {
                            text_parts.push(format!(
                                "[Binary resource {safe_uri} omitted: encoded size {} bytes exceeds {} byte cap]",
                                blob.len(),
                                MCP_DISK_WRITE_MAX_BYTES
                            ));
                        } else {
                            match decode_base64_maybe_blocking(blob).await {
                                Ok(data) => {
                                    if data.len() > MCP_DISK_WRITE_MAX_BYTES {
                                        text_parts.push(format!(
                                            "[Binary resource {safe_uri} omitted: {} bytes exceeds {} byte cap]",
                                            data.len(),
                                            MCP_DISK_WRITE_MAX_BYTES
                                        ));
                                    } else if is_text_mime_type(mime) {
                                        if let Ok(text) = String::from_utf8(data) {
                                            text_parts.push(text);
                                        } else {
                                            text_parts.push("[resource omitted]".to_string());
                                        }
                                    } else {
                                        let clean = raw_uri.split(['?', '#']).next().unwrap_or(raw_uri);
                                        let raw_ext = std::path::Path::new(clean)
                                            .extension()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("");
                                        let ext = sanitize_output_ext(raw_ext);
                                        let save_res = if let Some(ref s) = saver {
                                            s(data, ext).await
                                        } else {
                                            save_mcp_output_file_async(data, ext).await
                                        };
                                        match save_res {
                                            Ok(p) => {
                                                text_parts.push(format!(
                                                    "[Binary resource {safe_uri} saved to {}]",
                                                    p.display()
                                                ));
                                            }
                                            Err(e) => {
                                                text_parts.push(format!(
                                                    "[Binary resource {safe_uri} could not be saved: {e}]"
                                                ));
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    text_parts.push(format!(
                                        "[Binary resource {safe_uri} failed to decode base64: {e}]"
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    text_parts.push("[resource omitted]".to_string());
                }
            }
            McpContentBlock::Unknown => {
                text_parts.push("[unknown content block omitted]".to_string());
            }
        }
    }

    if text_parts.is_empty() && images.is_empty() {
        if let Some(ref structured) = call_res.structured_content {
            let formatted = serde_json::to_string_pretty(structured).unwrap_or_default();
            text_parts.push(formatted);
        }
    }

    let combined_text = text_parts.join("\n");
    let mut blocks = Vec::new();

    if let Some(truncated) = truncate_middle(&combined_text, MCP_OUTPUT_MAX_BYTES) {
        let save_res = if combined_text.len() > MCP_DISK_WRITE_MAX_BYTES {
            None
        } else if let Some(ref s) = saver {
            s(combined_text.into_bytes(), ".txt".to_string()).await.ok()
        } else {
            save_mcp_output_file_async(combined_text.into_bytes(), ".txt".to_string())
                .await
                .ok()
        };
        let suffix = if let Some(p) = save_res {
            format!("\n\n[Full output: {} (read it with offset/limit)]", p.display())
        } else {
            String::new()
        };
        blocks.push(ToolResultContentBlock::Text(TextContent {
            text: format!("{truncated}{suffix}"),
            text_signature: None,
        }));
    } else if !combined_text.is_empty() {
        blocks.push(ToolResultContentBlock::Text(TextContent {
            text: combined_text,
            text_signature: None,
        }));
    }

    blocks.extend(images);

    let mut structured_content = call_res.structured_content;
    if let Some(ref mut value) = structured_content {
        truncate_details(value);
    }

    AgentToolResult {
        content: blocks,
        structured_content,
        ..Default::default()
    }
}

/// Converts an MCP tool call result into an `AgentToolResult` conforming to `mehh_agent`.
pub async fn convert_mcp_result(call_res: McpCallToolResult) -> AgentToolResult {
    convert_mcp_result_with_saver(call_res, None).await
}

/// Agent tool adapter wrapping an MCP tool registered on an MCP client.
pub struct McpAgentTool {
    server_name: String,
    tool_name: String,
    adapter_name: String,
    sanitized_description: String,
    sanitized_schema: serde_json::Value,
    tool_info: McpToolInfo,
    client: Arc<McpClient>,
    timeout: Option<Duration>,
}

impl McpAgentTool {
    /// Creates a new MCP agent tool adapter.
    pub fn new(server_name: impl Into<String>, tool_info: McpToolInfo, client: Arc<McpClient>) -> Self {
        let server = server_name.into();
        let adapter_name = create_mcp_tool_name(&server, &tool_info.name);
        Self::with_resolved_name(server, tool_info, client, adapter_name)
    }

    /// Creates a new MCP agent tool adapter with an explicit pre-deduplicated adapter name.
    pub fn with_resolved_name(
        server_name: impl Into<String>,
        tool_info: McpToolInfo,
        client: Arc<McpClient>,
        adapter_name: impl Into<String>,
    ) -> Self {
        let server = server_name.into();
        let tool_name = tool_info.name.clone();
        let sanitized_description =
            sanitize_mcp_text(tool_info.description.as_deref().unwrap_or(""), MAX_TOOL_DESCRIPTION_BYTES);
        let sanitized_schema = sanitize_input_schema(tool_info.input_schema.clone());
        Self {
            server_name: server,
            tool_name,
            adapter_name: adapter_name.into(),
            sanitized_description,
            sanitized_schema,
            tool_info,
            client,
            timeout: None,
        }
    }

    /// Configures an optional per-call timeout forwarded to `McpRequestOptions`.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn original_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_info(&self) -> &McpToolInfo {
        &self.tool_info
    }
}

#[async_trait]
impl AgentTool for McpAgentTool {
    fn name(&self) -> &str {
        &self.adapter_name
    }

    fn label(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.sanitized_description
    }

    fn parameters(&self) -> serde_json::Value {
        self.sanitized_schema.clone()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        if self.is_read_only() == Some(true) {
            ToolExecutionMode::Parallel
        } else {
            ToolExecutionMode::Sequential
        }
    }

    fn is_read_only(&self) -> Option<bool> {
        let annotations = self.tool_info.annotations.as_ref()?;
        if annotations.destructive_hint == Some(true) {
            return Some(false);
        }
        annotations.read_only_hint
    }

    fn is_safe_for_auto_approve(
        &self,
        _args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<bool> {
        // Untrusted external MCP servers must never auto-approve purely on read_only_hint.
        Some(false)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        mut params: serde_json::Map<String, serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        // Strip internal security-inspector metadata before forwarding to untrusted MCP servers.
        params.remove(mehh_agent::types::INSPECTED_TARGET_ARG);
        let arguments = serde_json::Value::Object(params);
        let progress_cb = on_update.map(|cb| {
            Arc::new(move |prog: crate::client::McpProgressNotification| {
                // MCP `progress` is an absolute value; `total` may be absent.
                let mut msg = match prog.total {
                    Some(total) if total > 0.0 => {
                        format!("Progress: {:.0}%", (prog.progress / total) * 100.0)
                    }
                    _ => format!("Progress: {}", prog.progress),
                };
                if let Some(m) = &prog.message {
                    let clean_m = sanitize_progress_message(m);
                    if !clean_m.is_empty() {
                        msg.push_str(" - ");
                        msg.push_str(&clean_m);
                    }
                }
                cb(AgentToolResult {
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: msg,
                        text_signature: None,
                    })],
                    ..Default::default()
                });
            }) as crate::client::ProgressCallback
        });

        let options = crate::client::McpRequestOptions {
            timeout: self.timeout,
            cancel: Some(cancel.clone()),
            on_progress: progress_cb,
            ..Default::default()
        };

        let res = self
            .client
            .call_tool_with_options(&self.tool_name, arguments, options)
            .await
            .map_err(|e| e.to_string())?;

        let is_error = res.is_error;
        let tool_res = convert_mcp_result_with_saver(res, None).await;

        if is_error {
            let mut details: Vec<String> = Vec::new();
            for block in &tool_res.content {
                match block {
                    ToolResultContentBlock::Text(t) => {
                        if !t.text.is_empty() {
                            details.push(t.text.clone());
                        }
                    }
                    ToolResultContentBlock::Image(img) => {
                        details.push(format!("[image {}]", img.mime_type));
                    }
                }
            }
            if let Some(ref structured) = tool_res.structured_content {
                details.push(format!("structuredContent: {structured}"));
            }
            Err(if details.is_empty() {
                "Tool execution failed".to_string()
            } else {
                details.join("\n")
            })
        } else {
            Ok(tool_res)
        }
    }
}

/// Thread-safe registry holding active MCP server connections.
pub type McpServerRegistry = Arc<RwLock<Vec<(String, Arc<McpClient>)>>>;

/// Meta-tool for listing resources provided by MCP servers.
pub struct ListMcpResourcesTool {
    registry: McpServerRegistry,
}

impl ListMcpResourcesTool {
    pub fn new(registry: McpServerRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl AgentTool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        "list_mcp_resources"
    }

    fn label(&self) -> &str {
        "list_mcp_resources"
    }

    fn description(&self) -> &str {
        "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models. Prefer resources over web search when possible. Without `server`, the same cursor is sent to every server and the result contains a per-server `nextCursors` map; use a specific `server` for precise pagination."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Optional name of a specific MCP server to query resources from."
                },
                "cursor": {
                    "type": "string",
                    "description": "Optional pagination cursor."
                }
            }
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
        cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let server_name = params.get("server").and_then(|v| v.as_str());
        let cursor = params.get("cursor").and_then(|v| v.as_str());

        let result_json = if let Some(srv) = server_name {
            let reg = self.registry.read().await;
            let client = reg
                .iter()
                .find(|(n, _)| n == srv)
                .map(|(_, c)| c.clone())
                .ok_or_else(|| format!("MCP server '{srv}' not found in registry"))?;
            drop(reg);

            let req_params = match cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let res = client
                .request_with_options(
                    "resources/list",
                    Some(req_params),
                    McpRequestOptions::new().with_cancel(cancel.clone()),
                )
                .await
                .map_err(|e| e.to_string())?;

            let mut resources = Vec::new();
            if let Some(res_arr) = res.get("resources").and_then(|v| v.as_array()) {
                for r in res_arr {
                    let uri = r.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                    if uri.starts_with("ui://") || uri.contains("profile=mcp-app") {
                        continue;
                    }
                    let mut obj = r.clone();
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("server".to_string(), serde_json::json!(srv));
                    }
                    resources.push(obj);
                }
            }

            serde_json::json!({
                "server": srv,
                "resources": resources,
                "nextCursor": res.get("nextCursor")
            })
        } else {
            let reg = self.registry.read().await.clone();
            let mut all_resources = Vec::new();
            let mut errors = Vec::new();

            let requests = reg.into_iter().map(|(srv, client)| {
                let cancel = cancel.clone();
                let req_params = match cursor {
                    Some(c) => serde_json::json!({ "cursor": c }),
                    None => serde_json::json!({}),
                };
                async move {
                    let res = client
                        .request_with_options(
                            "resources/list",
                            Some(req_params),
                            McpRequestOptions::new().with_cancel(cancel),
                        )
                        .await;
                    (srv, res)
                }
            });
            let mut next_cursors = serde_json::Map::new();
            let mut stream =
                futures::stream::StreamExt::buffered(futures::stream::iter(requests), 8);
            while let Some((srv, outcome)) = futures::StreamExt::next(&mut stream).await {
                match outcome {
                    Ok(res) => {
                        if let Some(next) = res.get("nextCursor").filter(|v| !v.is_null()) {
                            next_cursors.insert(srv.clone(), next.clone());
                        }
                        if let Some(arr) = res.get("resources").and_then(|v| v.as_array()) {
                            for r in arr {
                                let uri = r.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                                if uri.starts_with("ui://") || uri.contains("profile=mcp-app") {
                                    continue;
                                }
                                all_resources.push(serde_json::json!({
                                    "server": srv,
                                    "uri": uri,
                                    "name": r.get("name").cloned().unwrap_or(serde_json::Value::Null),
                                    "description": r.get("description").cloned(),
                                    "mimeType": r.get("mimeType").cloned(),
                                }));
                            }
                        }
                    }
                    Err(e) => {
                        errors.push(serde_json::json!({ "server": srv, "error": e.to_string() }));
                    }
                }
            }

            serde_json::json!({
                "resources": all_resources,
                "errors": errors,
                "nextCursors": next_cursors
            })
        };

        let formatted = serde_json::to_string_pretty(&result_json).unwrap_or_default();
        let output = truncate_middle(&formatted, MCP_OUTPUT_MAX_BYTES).unwrap_or(formatted);
        Ok(AgentToolResult {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: output,
                text_signature: None,
            })],
            details: Some({
                let mut details = result_json;
                truncate_details(&mut details);
                details
            }),
            ..Default::default()
        })
    }
}

/// Meta-tool for listing resource templates provided by MCP servers.
pub struct ListMcpResourceTemplatesTool {
    registry: McpServerRegistry,
}

impl ListMcpResourceTemplatesTool {
    pub fn new(registry: McpServerRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl AgentTool for ListMcpResourceTemplatesTool {
    fn name(&self) -> &str {
        "list_mcp_resource_templates"
    }

    fn label(&self) -> &str {
        "list_mcp_resource_templates"
    }

    fn description(&self) -> &str {
        "Lists parameterized RFC 6570 resource templates provided by MCP servers. Without `server`, the same cursor is sent to every server and the result contains a per-server `nextCursors` map; use a specific `server` for precise pagination."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Optional name of a specific MCP server to query templates from."
                },
                "cursor": {
                    "type": "string",
                    "description": "Optional pagination cursor."
                }
            }
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
        cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let server_name = params.get("server").and_then(|v| v.as_str());
        let cursor = params.get("cursor").and_then(|v| v.as_str());

        let result_json = if let Some(srv) = server_name {
            let reg = self.registry.read().await;
            let client = reg
                .iter()
                .find(|(n, _)| n == srv)
                .map(|(_, c)| c.clone())
                .ok_or_else(|| format!("MCP server '{srv}' not found in registry"))?;
            drop(reg);

            let req_params = match cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let res = client
                .request_with_options(
                    "resources/templates/list",
                    Some(req_params),
                    McpRequestOptions::new().with_cancel(cancel.clone()),
                )
                .await
                .map_err(|e| e.to_string())?;

            serde_json::json!({
                "server": srv,
                "resourceTemplates": res.get("resourceTemplates").unwrap_or(&serde_json::json!([])),
                "nextCursor": res.get("nextCursor")
            })
        } else {
            let reg = self.registry.read().await.clone();
            let mut all_templates = Vec::new();
            let mut errors = Vec::new();

            let requests = reg.into_iter().map(|(srv, client)| {
                let cancel = cancel.clone();
                let req_params = match cursor {
                    Some(c) => serde_json::json!({ "cursor": c }),
                    None => serde_json::json!({}),
                };
                async move {
                    let res = client
                        .request_with_options(
                            "resources/templates/list",
                            Some(req_params),
                            McpRequestOptions::new().with_cancel(cancel),
                        )
                        .await;
                    (srv, res)
                }
            });
            let mut next_cursors = serde_json::Map::new();
            let mut stream =
                futures::stream::StreamExt::buffered(futures::stream::iter(requests), 8);
            while let Some((srv, outcome)) = futures::StreamExt::next(&mut stream).await {
                match outcome {
                    Ok(res) => {
                        if let Some(next) = res.get("nextCursor").filter(|v| !v.is_null()) {
                            next_cursors.insert(srv.clone(), next.clone());
                        }
                        if let Some(arr) = res.get("resourceTemplates").and_then(|v| v.as_array()) {
                            for t in arr {
                                let mut t = t.clone();
                                if let Some(map) = t.as_object_mut() {
                                    map.insert("server".to_string(), serde_json::json!(srv.clone()));
                                }
                                all_templates.push(t);
                            }
                        }
                    }
                    Err(e) => {
                        errors.push(serde_json::json!({ "server": srv, "error": e.to_string() }));
                    }
                }
            }

            serde_json::json!({
                "resourceTemplates": all_templates,
                "errors": errors,
                "nextCursors": next_cursors
            })
        };

        let formatted = serde_json::to_string_pretty(&result_json).unwrap_or_default();
        let output = truncate_middle(&formatted, MCP_OUTPUT_MAX_BYTES).unwrap_or(formatted);
        Ok(AgentToolResult {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: output,
                text_signature: None,
            })],
            details: Some({
                let mut details = result_json;
                truncate_details(&mut details);
                details
            }),
            ..Default::default()
        })
    }
}

/// Meta-tool for reading a specific resource from an MCP server.
pub struct ReadMcpResourceTool {
    registry: McpServerRegistry,
}

impl ReadMcpResourceTool {
    pub fn new(registry: McpServerRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl AgentTool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        "read_mcp_resource"
    }

    fn label(&self) -> &str {
        "read_mcp_resource"
    }

    fn description(&self) -> &str {
        "Read a specific resource from an MCP server given the server name and resource URI."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Name of the MCP server hosting the resource."
                },
                "uri": {
                    "type": "string",
                    "description": "URI of the resource to read."
                }
            },
            "required": ["server", "uri"]
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
        cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let server = params
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: server".to_string())?;
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: uri".to_string())?;

        let reg = self.registry.read().await;
        let client = reg
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| format!("MCP server '{server}' not found in registry"))?;
        drop(reg);

        let res = client
            .request_with_options(
                "resources/read",
                Some(serde_json::json!({ "uri": uri })),
                McpRequestOptions::new().with_cancel(cancel.clone()),
            )
            .await
            .map_err(|e| e.to_string())?;

        let mut blocks = Vec::new();
        let mut text_parts = Vec::new();
        let mut total_image_bytes = 0usize;

        if let Some(contents_arr) = res.get("contents").and_then(|v| v.as_array()) {
            for item in contents_arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                } else if let Some(blob) = item.get("blob").and_then(|b| b.as_str()) {
                    let mime = item
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        push_capped_image(
                            blob.to_string(),
                            mime.to_string(),
                            &mut blocks,
                            &mut text_parts,
                            &mut total_image_bytes,
                        );
                    } else if is_text_mime_type(mime) {
                        let max_b64_len = MCP_DISK_WRITE_MAX_BYTES.div_ceil(3).saturating_mul(4);
                        if blob.len() > max_b64_len {
                            text_parts.push(format!(
                                "[text resource {mime} omitted: encoded size {} bytes exceeds {} byte cap]",
                                blob.len(),
                                MCP_DISK_WRITE_MAX_BYTES
                            ));
                        } else {
                            match decode_base64_maybe_blocking(blob).await {
                                Ok(data) => match String::from_utf8(data) {
                                    Ok(text) => text_parts.push(text),
                                    Err(_) => {
                                        text_parts.push(format!("[binary resource {mime} omitted]"))
                                    }
                                },
                                Err(e) => {
                                    text_parts.push(format!(
                                        "[resource {mime} failed to decode base64: {e}]"
                                    ));
                                }
                            }
                        }
                    } else {
                        text_parts.push(format!("[binary resource {mime} omitted]"));
                    }
                }
            }
        }

        if text_parts.is_empty() && blocks.is_empty() {
            text_parts.push(serde_json::to_string_pretty(&res).unwrap_or_default());
        }

        let combined = text_parts.join("\n");
        if !combined.is_empty() {
            let final_text = truncate_middle(&combined, MCP_RESOURCE_MAX_BYTES).unwrap_or(combined);
            blocks.insert(
                0,
                ToolResultContentBlock::Text(TextContent {
                    text: final_text,
                    text_signature: None,
                }),
            );
        }

        Ok(AgentToolResult {
            content: blocks,
            details: Some({
                let mut details = res;
                truncate_details(&mut details);
                details
            }),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::McpTransport;

    #[test]
    fn test_create_mcp_tool_name_normal() {
        let name = create_mcp_tool_name("filesystem", "read_file");
        assert_eq!(name, "mcp__filesystem__read_file");
    }

    #[test]
    fn test_create_mcp_tool_name_sanitization() {
        let name = create_mcp_tool_name("my-server", "tool.action");
        assert_eq!(name, "mcp__my_server__tool_action");
    }

    #[test]
    fn test_create_mcp_tool_name_truncation_over_64_chars() {
        let server = "a".repeat(40);
        let tool = "b".repeat(40);
        let name = create_mcp_tool_name(&server, &tool);
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("mcp__aaaaaaaaaa"));
        assert_eq!(name.chars().nth(55), Some('_'));
    }

    #[test]
    fn test_resolve_unique_mcp_tool_name_deduplicates_collisions() {
        let mut used = HashSet::new();
        let n1 = resolve_unique_mcp_tool_name("srv", "foo-bar", &mut used);
        let n2 = resolve_unique_mcp_tool_name("srv", "foo.bar", &mut used);
        assert_eq!(n1, "mcp__srv__foo_bar");
        assert_eq!(n2, "mcp__srv__foo_bar_2");

        let long_a = format!("{}a", "x".repeat(60));
        let long_b = format!("{}b", "x".repeat(60));
        let l1 = resolve_unique_mcp_tool_name("srv", &long_a, &mut used);
        let l2 = resolve_unique_mcp_tool_name("srv", &long_b, &mut used);
        assert!(l1.len() <= MAX_TOOL_NAME_LENGTH);
        assert!(l2.len() <= MAX_TOOL_NAME_LENGTH);
        assert_ne!(l1, l2);
    }

    #[test]
    fn test_sanitize_mcp_text_strips_control_and_truncates_utf8_safely() {
        let dirty = format!("hello\x1b[31m world\t\n{}", "界".repeat(2000));
        let clean = sanitize_mcp_text(&dirty, MAX_TOOL_DESCRIPTION_BYTES);
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains('\t'));
        assert!(clean.contains('\n'));
        assert!(clean.len() <= MAX_TOOL_DESCRIPTION_BYTES);
    }

    #[tokio::test]
    async fn test_convert_mcp_result_caps_oversized_images() {
        let big_img = "A".repeat(MCP_RESOURCE_MAX_BYTES + 10);
        let res = McpCallToolResult {
            content: vec![McpContentBlock::Image {
                data: big_img,
                mime_type: "image/png".to_string(),
            }],
            is_error: false,
            structured_content: None,
        };
        let converted = convert_mcp_result(res).await;
        assert_eq!(converted.content.len(), 1);
        match &converted.content[0] {
            ToolResultContentBlock::Text(t) => assert!(t.text.contains("exceeds")),
            _ => panic!("expected text omission warning"),
        }
    }

    #[test]
    fn test_truncate_middle_no_truncation_when_under_limit() {
        let text = "short string";
        assert!(truncate_middle(text, 100).is_none());
    }

    #[test]
    fn test_truncate_middle_cuts_middle_and_adds_warning() {
        let text = "head".to_string() + &"x".repeat(100) + "tail";
        let res = truncate_middle(&text, 20).unwrap();
        assert!(res.contains("Warning: truncated output"));
        assert!(res.contains("head"));
        assert!(res.contains("tail"));
        assert!(res.contains("chars truncated"));
    }

    #[tokio::test]
    async fn convert_mcp_result_saves_binary_blob_and_oversized_text() {
        let big_text = "line 1\n".to_string() + &"x".repeat(30_000) + "\nlast line";
        let pdf_bytes = b"%PDF-1.4 mock binary data";
        let b64_blob = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pdf_bytes);

        let res = McpCallToolResult {
            content: vec![
                McpContentBlock::Text { text: big_text.clone() },
                McpContentBlock::Resource {
                    resource: serde_json::json!({
                        "uri": "file:///tmp/sample.pdf",
                        "mimeType": "application/pdf",
                        "blob": b64_blob,
                    }),
                },
            ],
            is_error: false,
            structured_content: None,
        };

        let converted = convert_mcp_result_with_saver(res, None).await;
        let mut full_output_path = None;
        let mut binary_saved_path = None;

        for block in &converted.content {
            if let ToolResultContentBlock::Text(t) = block {
                if t.text.contains("[Full output: ") {
                    let start = t.text.find("[Full output: ").unwrap() + 14;
                    let end = t.text[start..].find(" (read it with offset/limit)]").unwrap() + start;
                    full_output_path = Some(std::path::PathBuf::from(&t.text[start..end]));
                }
                if t.text.contains("[Binary resource file:///tmp/sample.pdf") && t.text.contains(" saved to ") {
                    let marker = " saved to ";
                    let start = t.text.find(marker).unwrap() + marker.len();
                    let end = t.text[start..].find(']').unwrap() + start;
                    binary_saved_path = Some(std::path::PathBuf::from(&t.text[start..end]));
                }
            }
        }

        let full_p = full_output_path.expect("must have saved oversized text to disk");
        assert!(full_p.exists());
        let saved_text = std::fs::read_to_string(&full_p).unwrap();
        assert!(saved_text.contains("line 1"));
        assert!(saved_text.contains("last line"));
        let _ = std::fs::remove_file(full_p);

        let bin_p = binary_saved_path.expect("must have saved binary blob to disk");
        assert!(bin_p.exists());
        let saved_bytes = std::fs::read(&bin_p).unwrap();
        assert_eq!(saved_bytes, pdf_bytes);
        let _ = std::fs::remove_file(bin_p);
    }

    #[tokio::test]
    async fn test_binary_resource_invalid_base64_returns_error_string() {
        let raw_res = serde_json::json!({
            "content": [
                {
                    "type": "resource",
                    "resource": {
                        "uri": "file:///tmp/corrupt.bin",
                        "mimeType": "application/octet-stream",
                        "blob": "not-valid-base64-!!!"
                    }
                }
            ]
        });

        let call_res: McpCallToolResult = serde_json::from_value(raw_res).unwrap();
        let agent_res = convert_mcp_result_with_saver(call_res, None).await;
        let text = match &agent_res.content[0] {
            ToolResultContentBlock::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("[Binary resource file:///tmp/corrupt.bin failed to decode base64:"));
    }

    #[tokio::test]
    async fn mcp_agent_tool_forwards_progress_to_on_update_and_cancels_server() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcNotification, JsonRpcResponse};

        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        tokio::spawn({
            let server_transport = server_transport.clone();
            async move {
                if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                    let resp = JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": { "name": "test-server", "version": "1.0.0" }
                        }),
                    );
                    server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
                }
                let _ = server_transport.recv().await;

                // Tool call
                if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                    let token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .cloned()
                        .expect("progressToken present");

                    // Send progress frame
                    let p = JsonRpcNotification::new(
                        "notifications/progress",
                        Some(serde_json::json!({
                            "progressToken": token,
                            "progress": 0.5,
                            "total": 1.0,
                            "message": "working"
                        })),
                    );
                    server_transport.send(JsonRpcMessage::Notification(p)).await.unwrap();

                    // Wait for cancel or complete
                    let mut cancelled = false;
                    while let Some(Ok(msg)) = server_transport.recv().await {
                        if let JsonRpcMessage::Notification(notif) = msg {
                            if notif.method == "notifications/cancelled" {
                                cancelled = true;
                                break;
                            }
                        }
                    }
                    assert!(cancelled);
                }
            }
        });

        let client = Arc::new(McpClient::connect(client_transport).await.unwrap());
        let tool_info = McpToolInfo {
            name: "test_tool".to_string(),
            description: Some("desc".to_string()),
            input_schema: serde_json::json!({ "type": "object" }),
            annotations: None,
        };
        let agent_tool = McpAgentTool::new("mock_server", tool_info, client);

        let update_received = Arc::new(AtomicBool::new(false));
        let update_clone = update_received.clone();
        let on_update: mehh_agent::AgentToolUpdateCallback = Arc::new(move |res| {
            update_clone.store(true, Ordering::SeqCst);
            let text = match &res.content[0] {
                ToolResultContentBlock::Text(t) => t.text.as_str(),
                _ => panic!("expected text progress update"),
            };
            assert!(text.contains("Progress: 50%"), "unexpected progress message: {text}");
        });

        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel_clone.cancel();
        });

        let res = agent_tool
            .execute(
                "call-1",
                serde_json::Map::new(),
                cancel,
                Some(on_update),
            )
            .await;

        assert!(res.is_err());
        assert!(update_received.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_mcp_agent_tool_error_includes_structured_content_and_block_descriptors() {
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcResponse};

        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let server = tokio::spawn(async move {
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                let _ = server_transport.send(JsonRpcMessage::Response(resp)).await;
            }
            let _ = server_transport.recv().await; // notifications/initialized
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                assert_eq!(req.method, "tools/call");
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "content": [
                            { "type": "image", "data": "AAAA", "mimeType": "image/png" }
                        ],
                        "isError": true,
                        "structuredContent": { "error_code": "E42", "detail": "bad input" }
                    }),
                );
                let _ = server_transport.send(JsonRpcMessage::Response(resp)).await;
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_transport)).await.unwrap());
        let tool_info = McpToolInfo {
            name: "failing_tool".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
            annotations: None,
        };
        let agent_tool = McpAgentTool::new("mock_server", tool_info, client.clone());

        let err = agent_tool
            .execute(
                "call-err",
                serde_json::Map::new(),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("error_code") && err.contains("E42") && err.contains("bad input"),
            "error must surface structuredContent, got: {err}"
        );
        assert!(
            err.contains("image/png"),
            "error must describe non-text blocks, got: {err}"
        );

        client.close().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn convert_mcp_result_truncates_oversized_structured_content() {
        let oversized = "x".repeat(MCP_OUTPUT_MAX_BYTES + 2_000);
        let res = McpCallToolResult {
            content: vec![McpContentBlock::Text { text: "ok".to_string() }],
            is_error: false,
            structured_content: Some(serde_json::json!({ "output": oversized })),
        };

        let converted = convert_mcp_result(res).await;
        let structured = converted
            .structured_content
            .expect("structured content must be preserved");
        let text = structured
            .get("output")
            .and_then(|v| v.as_str())
            .expect("output must remain a string");

        assert!(
            text.contains("Warning: truncated output"),
            "oversized structuredContent must be truncated, got {} bytes",
            text.len()
        );
        assert!(
            text.len() < MCP_OUTPUT_MAX_BYTES + 256,
            "structuredContent still exceeds cap: {} bytes",
            text.len()
        );
    }

    #[tokio::test]
    async fn create_mcp_agent_tools_with_shared_names_dedupes_across_servers() {
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcResponse};

        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let server = tokio::spawn(async move {
            while let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                let _ = server_transport.send(JsonRpcMessage::Response(resp)).await;
            }
        });
        let client = Arc::new(McpClient::connect(Arc::new(client_transport)).await.unwrap());

        let tool = || McpToolInfo {
            name: "delete".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
            annotations: None,
        };
        let mut used = HashSet::new();
        let from_dash = create_mcp_agent_tools_with_shared_names(
            "svc-a",
            vec![tool()],
            client.clone(),
            &mut used,
        );
        let from_underscore = create_mcp_agent_tools_with_shared_names(
            "svc_a",
            vec![tool()],
            client.clone(),
            &mut used,
        );

        let name_a = from_dash[0].name().to_string();
        let name_b = from_underscore[0].name().to_string();
        assert_eq!(name_a, "mcp__svc_a__delete");
        assert_eq!(name_b, "mcp__svc_a__delete_2");
        assert_ne!(name_a, name_b);

        client.close().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn execute_strips_inspected_target_arg_and_sanitizes_progress_message() {
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcNotification, JsonRpcResponse};

        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let captured_args = Arc::new(tokio::sync::Mutex::new(None));
        let captured_clone = captured_args.clone();

        let server = tokio::spawn(async move {
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            let _ = server_transport.recv().await; // notifications/initialized

            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let args = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("arguments"))
                    .cloned();
                *captured_clone.lock().await = args;

                let token = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("_meta"))
                    .and_then(|m| m.get("progressToken"))
                    .cloned()
                    .expect("progressToken");

                let dirty_progress = JsonRpcNotification::new(
                    "notifications/progress",
                    Some(serde_json::json!({
                        "progressToken": token,
                        "progress": 1.0,
                        "total": 2.0,
                        "message": "\x1b[31mstep\x1b[0m \x1b]8;;https://evil.test\x07link\x1b]8;;\x07\r\n\u{202E}spoof\u{2066}!"
                    })),
                );
                server_transport
                    .send(JsonRpcMessage::Notification(dirty_progress))
                    .await
                    .unwrap();

                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": "done" }]
                    }),
                );
                server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_transport)).await.unwrap());
        let tool_info = McpToolInfo {
            name: "echo".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
            annotations: None,
        };
        let tool = McpAgentTool::new("srv", tool_info, client.clone());

        let received_progress = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let progress_seen = Arc::new(tokio::sync::Notify::new());
        let progress_clone = received_progress.clone();
        let progress_seen_cb = progress_seen.clone();
        let on_update: AgentToolUpdateCallback = Arc::new(move |res| {
            if let Some(ToolResultContentBlock::Text(t)) = res.content.first() {
                progress_clone.lock().unwrap().push(t.text.clone());
                progress_seen_cb.notify_one();
            }
        });

        let mut params = serde_json::Map::new();
        params.insert("query".to_string(), serde_json::json!("hello"));
        params.insert(
            mehh_agent::types::INSPECTED_TARGET_ARG.to_string(),
            serde_json::json!("/secret/internal/path"),
        );

        let result = tool
            .execute(
                "call-1",
                params,
                tokio_util::sync::CancellationToken::new(),
                Some(on_update),
            )
            .await
            .unwrap();
        assert_eq!(result.content.len(), 1);

        let forwarded = captured_args.lock().await.clone().unwrap();
        assert_eq!(forwarded.get("query").and_then(|v| v.as_str()), Some("hello"));
        assert!(
            forwarded.get(mehh_agent::types::INSPECTED_TARGET_ARG).is_none(),
            "INSPECTED_TARGET_ARG must be stripped before forwarding to MCP server: {forwarded}"
        );

        if received_progress.lock().unwrap().is_empty() {
            tokio::time::timeout(std::time::Duration::from_secs(2), progress_seen.notified())
                .await
                .expect("expected sanitized progress update");
        }
        {
            let updates = received_progress.lock().unwrap();
            assert_eq!(updates.len(), 1);
            let msg = &updates[0];
            assert!(!msg.contains('\x1b'), "ANSI escape leaked: {msg:?}");
            assert!(!msg.contains('\x07'), "BEL leaked: {msg:?}");
            assert!(!msg.contains('\r'), "CR leaked: {msg:?}");
            assert!(!msg.contains('\n'), "LF leaked: {msg:?}");
            assert!(!msg.contains('\u{202E}'), "bidi RLO leaked: {msg:?}");
            assert!(!msg.contains('\u{2066}'), "bidi LRI leaked: {msg:?}");
            assert!(msg.contains("step"), "expected clean progress text in {msg:?}");
            assert!(msg.contains("spoof!"), "expected clean progress text in {msg:?}");
        }

        client.close().await.unwrap();
        server.abort();
    }

    #[test]
    fn sanitize_input_schema_bounds_fallback_size() {
        let mut props = serde_json::Map::new();
        let mut required = Vec::new();
        for i in 0..3000 {
            let name = format!("field_{i:04}_{}", "x".repeat(40));
            props.insert(
                name.clone(),
                serde_json::json!({
                    "type": "string",
                    "description": "d".repeat(64)
                }),
            );
            required.push(serde_json::Value::String(name));
        }
        let giant = serde_json::json!({
            "type": "object",
            "properties": props,
            "required": required
        });
        let sanitized = sanitize_input_schema(giant);
        let serialized_len = serde_json::to_vec(&sanitized).unwrap().len();
        assert!(
            serialized_len <= MAX_TOOL_SCHEMA_BYTES,
            "fallback schema exceeded MAX_TOOL_SCHEMA_BYTES: {serialized_len}"
        );
    }

    #[tokio::test]
    async fn convert_mcp_result_bounds_binary_resource_writes_and_sanitizes_ext() {
        let oversized_b64 = "A".repeat(MCP_DISK_WRITE_MAX_BYTES.div_ceil(3) * 4 + 16);
        let res = McpCallToolResult {
            content: vec![McpContentBlock::Resource {
                resource: serde_json::json!({
                    "uri": "file:///tmp/huge.bin",
                    "mimeType": "application/octet-stream",
                    "blob": oversized_b64,
                }),
            }],
            is_error: false,
            structured_content: None,
        };
        let converted = convert_mcp_result_with_saver(res, None).await;
        match &converted.content[0] {
            ToolResultContentBlock::Text(t) => {
                assert!(t.text.contains("exceeds"), "expected size cap omission: {}", t.text);
            }
            _ => panic!("expected text omission"),
        }

        // Verify extension passed to custom saver is sanitized and strips #fragment/?query.
        let captured_ext = Arc::new(std::sync::Mutex::new(String::new()));
        let ext_clone = captured_ext.clone();
        let saver: McpOutputSaver = Arc::new(move |_data, ext| {
            let ext_clone = ext_clone.clone();
            Box::pin(async move {
                *ext_clone.lock().unwrap() = ext;
                Ok(PathBuf::from("/tmp/fake.out"))
            })
        });
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hello");
        let res2 = McpCallToolResult {
            content: vec![McpContentBlock::Resource {
                resource: serde_json::json!({
                    "uri": "file:///tmp/doc.PDF?dl=1#../evil",
                    "mimeType": "application/pdf",
                    "blob": b64,
                }),
            }],
            is_error: false,
            structured_content: None,
        };
        let _ = convert_mcp_result_with_saver(res2, Some(saver)).await;
        assert_eq!(captured_ext.lock().unwrap().as_str(), ".pdf");

        // Verify direct disk write rejects oversized payload.
        let dir = crate::TempTestDir::new("tool_adapter_disk_cap");
        let err = save_mcp_output_file_in_dir(
            dir.path(),
            &vec![0u8; MCP_DISK_WRITE_MAX_BYTES + 1],
            ".bin",
        )
        .unwrap_err();
        assert!(err.to_string().contains("disk write limit"));
    }

    #[test]
    fn save_mcp_output_refuses_symlink_dir_and_cleanup_scopes_to_regular_prefixed_files() {
        let dir = crate::TempTestDir::new("tool_adapter_cleanup");
        let target_dir = dir.path().join("real_target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let victim_file = target_dir.join("mcp-output-old.txt");
        let unrelated_file = target_dir.join("unrelated.txt");
        std::fs::write(&victim_file, b"keep-me").unwrap();
        std::fs::write(&unrelated_file, b"keep-unrelated").unwrap();

        let symlink_dir = dir.path().join("link_dir");
        #[cfg(unix)]
        let linked_dir = std::os::unix::fs::symlink(&target_dir, &symlink_dir).is_ok();
        #[cfg(windows)]
        let linked_dir = std::os::windows::fs::symlink_dir(&target_dir, &symlink_dir).is_ok();

        if linked_dir {
            let err = save_mcp_output_file_in_dir(&symlink_dir, b"payload", ".txt").unwrap_err();
            assert!(err.to_string().contains("symlinked directory"), "got: {err}");
            assert!(victim_file.exists(), "symlinked dir contents must not be deleted");
            assert!(unrelated_file.exists());
        }

        // Cleanup with future cutoff must delete only regular `mcp-output-*` files,
        // never unrelated files or symlinks named `mcp-output-*`.
        let external_victim = dir.path().join("external.txt");
        std::fs::write(&external_victim, b"secret").unwrap();
        let symlink_entry = target_dir.join("mcp-output-link.txt");
        #[cfg(unix)]
        let linked_file = std::os::unix::fs::symlink(&external_victim, &symlink_entry).is_ok();
        #[cfg(windows)]
        let linked_file =
            std::os::windows::fs::symlink_file(&external_victim, &symlink_entry).is_ok();

        let future_cutoff = SystemTime::now() + Duration::from_secs(60);
        cleanup_old_mcp_outputs_with_cutoff(&target_dir, future_cutoff);

        assert!(!victim_file.exists(), "expired regular mcp-output-* file should be cleaned up");
        assert!(unrelated_file.exists(), "unrelated file must not be deleted");
        assert!(external_victim.exists(), "symlink target must not be deleted");
        if linked_file {
            assert!(
                std::fs::symlink_metadata(&symlink_entry).is_ok(),
                "symlink named mcp-output-* must not be deleted"
            );
        }
    }

    #[tokio::test]
    async fn i23_convert_mcp_result_handles_blob_without_mime_type_and_saves_untruncated_resource_text() {
        // 1. Blob missing mimeType must be saved as binary (application/octet-stream), not omitted.
        let payload = b"raw-binary-bytes";
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload);
        type SavedBlobs = Arc<std::sync::Mutex<Vec<(Vec<u8>, String)>>>;
        let saved_blobs: SavedBlobs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved_blobs_cb = saved_blobs.clone();
        let saver: McpOutputSaver = Arc::new(move |data, ext| {
            let saved_blobs_cb = saved_blobs_cb.clone();
            Box::pin(async move {
                saved_blobs_cb.lock().unwrap().push((data, ext));
                Ok(PathBuf::from("/tmp/saved.out"))
            })
        });

        let res_blob = McpCallToolResult {
            content: vec![McpContentBlock::Resource {
                resource: serde_json::json!({
                    "uri": "file:///tmp/mystery.dat",
                    "blob": b64,
                }),
            }],
            is_error: false,
            structured_content: None,
        };
        let converted_blob = convert_mcp_result_with_saver(res_blob, Some(saver.clone())).await;
        let blob_text = match &converted_blob.content[0] {
            ToolResultContentBlock::Text(t) => &t.text,
            _ => panic!("expected text block"),
        };
        assert!(
            blob_text.contains("saved to"),
            "blob without mimeType must be saved, got: {blob_text}"
        );
        assert_eq!(saved_blobs.lock().unwrap()[0].0, payload);

        // 2. Resource text larger than MCP_RESOURCE_MAX_BYTES must NOT be pre-truncated before saving.
        let oversized_resource_text =
            format!("HEAD_MARKER_{}_TAIL_MARKER", "Z".repeat(MCP_RESOURCE_MAX_BYTES + 10_000));
        let res_text = McpCallToolResult {
            content: vec![McpContentBlock::Resource {
                resource: serde_json::json!({
                    "uri": "file:///tmp/big.txt",
                    "mimeType": "text/plain",
                    "text": oversized_resource_text,
                }),
            }],
            is_error: false,
            structured_content: None,
        };
        let _ = convert_mcp_result_with_saver(res_text, Some(saver)).await;
        let saved_guard = saved_blobs.lock().unwrap();
        let full_saved = String::from_utf8(saved_guard[1].0.clone()).unwrap();
        assert_eq!(
            full_saved.len(),
            oversized_resource_text.len(),
            "saved resource text must not be pre-truncated before disk write"
        );
        assert!(!full_saved.contains("chars truncated"));
    }

    #[tokio::test]
    async fn i23_read_mcp_resource_tool_decodes_text_blobs_and_omits_unknown_mime_cleanly() {
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcResponse};

        let (client_tr, server_tr) = crate::transport::InMemoryTransport::pair();
        let text_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"hello decoded text",
        );
        let bin_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [0xff, 0x00, 0xfe, 0x01],
        );

        let bin_b64_srv = bin_b64.clone();
        let srv = tokio::spawn(async move {
            while let Some(Ok(msg)) = server_tr.recv().await {
                let JsonRpcMessage::Request(req) = msg else {
                    continue;
                };
                let resp = if req.method == "initialize" {
                    JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "resources": {} },
                            "serverInfo": { "name": "res-srv", "version": "1.0" }
                        }),
                    )
                } else if req.method == "resources/read" {
                    let uri = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    if uri == "mem://text" {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "contents": [{
                                    "uri": uri,
                                    "mimeType": "text/plain",
                                    "blob": text_b64,
                                }]
                            }),
                        )
                    } else {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "contents": [{
                                    "uri": uri,
                                    "blob": bin_b64_srv,
                                }]
                            }),
                        )
                    }
                } else {
                    JsonRpcResponse::success(req.id, serde_json::json!({}))
                };
                let _ = server_tr.send(JsonRpcMessage::Response(resp)).await;
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_tr)).await.unwrap());
        let reg: McpServerRegistry = Arc::new(RwLock::new(vec![("srv".to_string(), client.clone())]));
        let tool = ReadMcpResourceTool::new(reg);

        let mut p1 = serde_json::Map::new();
        p1.insert("server".to_string(), serde_json::json!("srv"));
        p1.insert("uri".to_string(), serde_json::json!("mem://text"));
        let r1 = tool
            .execute("c1", p1, tokio_util::sync::CancellationToken::new(), None)
            .await
            .unwrap();
        let t1 = match &r1.content[0] {
            ToolResultContentBlock::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert_eq!(t1, "hello decoded text");

        let mut p2 = serde_json::Map::new();
        p2.insert("server".to_string(), serde_json::json!("srv"));
        p2.insert("uri".to_string(), serde_json::json!("mem://unknown"));
        let r2 = tool
            .execute("c2", p2, tokio_util::sync::CancellationToken::new(), None)
            .await
            .unwrap();
        let t2 = match &r2.content[0] {
            ToolResultContentBlock::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert_eq!(t2, "[binary resource application/octet-stream omitted]");
        assert!(!t2.contains(&bin_b64));

        client.close().await.unwrap();
        srv.abort();
    }

    #[tokio::test]
    async fn c9_mcp_agent_tool_enforces_configured_timeout() {
        use crate::jsonrpc::{JsonRpcMessage, JsonRpcResponse};

        let (client_tr, server_tr) = crate::transport::InMemoryTransport::pair();
        let srv = tokio::spawn(async move {
            while let Some(Ok(msg)) = server_tr.recv().await {
                if let JsonRpcMessage::Request(req) = msg {
                    if req.method == "initialize" {
                        let resp = JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "slow-srv", "version": "1.0" }
                            }),
                        );
                        let _ = server_tr.send(JsonRpcMessage::Response(resp)).await;
                    } else if req.method == "tools/call" {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_tr)).await.unwrap());
        let tool_info = McpToolInfo {
            name: "slow_op".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
            annotations: None,
        };
        let mut used = HashSet::new();
        let tools = create_mcp_agent_tools_with_shared_names_and_timeout(
            "srv",
            vec![tool_info],
            client.clone(),
            &mut used,
            Some(Duration::from_millis(60)),
        );
        let start = std::time::Instant::now();
        let err = tools[0]
            .execute(
                "call-1",
                serde_json::Map::new(),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.contains("timed out"), "expected timeout error, got: {err}");

        client.close().await.unwrap();
        srv.abort();
    }
}
