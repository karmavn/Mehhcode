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

fn cleanup_old_mcp_outputs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Ok(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(24 * 3600)).ok_or(()) else {
        return;
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() && meta.modified().is_ok_and(|m| m < cutoff) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn mcp_output_base_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    if let Some(h) = home {
        PathBuf::from(h).join(".mehh").join("tmp").join("mcp")
    } else {
        std::env::temp_dir().join(format!("mehh-mcp-{}", std::process::id()))
    }
}

pub fn save_mcp_output_file(data: &[u8], ext: &str) -> anyhow::Result<PathBuf> {
    let base_dir = mcp_output_base_dir();
    std::fs::create_dir_all(&base_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base_dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let clean_ext = sanitize_output_ext(ext);

    let filename = format!("mcp-output-{}{clean_ext}", uuid::Uuid::new_v4());
    let file_path = base_dir.join(filename);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let mut file = opts.open(&file_path)?;
        use std::io::Write;
        file.write_all(data)?;
    }

    #[cfg(not(unix))]
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let mut file = opts.open(&file_path)?;
        use std::io::Write;
        file.write_all(data)?;
    }

    Ok(file_path)
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
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024; // 20KB model-facing limit
pub const MCP_RESOURCE_MAX_BYTES: usize = 100 * 1024; // 100KB cap for resources/images

/// Async wrapper so filesystem work never blocks a Tokio worker thread.
pub async fn save_mcp_output_file_async(data: Vec<u8>, ext: String) -> anyhow::Result<PathBuf> {
    tokio::task::spawn_blocking(move || {
        cleanup_old_mcp_outputs(&mcp_output_base_dir());
        save_mcp_output_file(&data, &ext)
    })
    .await
    .context("MCP output save task panicked")?
}

/// Sanitizes untrusted control characters and caps length on a UTF-8 char boundary.
pub fn sanitize_mcp_text(raw: &str, max_bytes: usize) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|&c| {
            let code = c as u32;
            c == '\n' || c == '\t' || (code >= 0x20 && code != 0x7f && !(0x80..=0x9f).contains(&code))
        })
        .collect();
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &cleaned[..end])
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

fn schema_too_big(schema: &serde_json::Value) -> bool {
    serde_json::to_vec(schema)
        .map(|bytes| bytes.len() > MAX_TOOL_SCHEMA_BYTES)
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
    // tool instead of guessing against an empty `{}` schema.
    let mut minimal = serde_json::Map::new();
    if let Some(kind) = schema.get("type") {
        minimal.insert("type".to_string(), kind.clone());
    }
    if let Some(serde_json::Value::Object(props)) = schema.get("properties") {
        let mut kept = serde_json::Map::new();
        for (name, prop) in props {
            let mut slim = serde_json::Map::new();
            if let Some(kind) = prop.get("type") {
                slim.insert("type".to_string(), kind.clone());
            }
            kept.insert(name.clone(), serde_json::Value::Object(slim));
        }
        minimal.insert("properties".to_string(), serde_json::Value::Object(kept));
    }
    if let Some(required) = schema.get("required") {
        minimal.insert("required".to_string(), required.clone());
    }
    minimal.insert(
        "description".to_string(),
        serde_json::json!("[schema truncated: original exceeded size limit]"),
    );
    serde_json::Value::Object(minimal)
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
    tools
        .into_iter()
        .map(|info| {
            let unique_name = resolve_unique_mcp_tool_name(server_name, &info.name, used_names);
            Arc::new(McpAgentTool::with_resolved_name(
                server_name,
                info,
                client.clone(),
                unique_name,
            )) as Arc<dyn AgentTool>
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
    if let Ok(bytes) = serde_json::to_vec(value) {
        if bytes.len() > MCP_OUTPUT_MAX_BYTES * 2 {
            *value = serde_json::json!({
                "truncated": true,
                "originalBytes": bytes.len()
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
                    let display = truncate_middle(text, MCP_RESOURCE_MAX_BYTES)
                        .unwrap_or_else(|| text.to_string());
                    text_parts.push(display);
                } else if let (Some(blob), Some(mime)) = (
                    resource.get("blob").and_then(|b| b.as_str()),
                    resource.get("mimeType").and_then(|m| m.as_str()),
                ) {
                    if mime.starts_with("image/") {
                        push_capped_image(
                            blob.to_string(),
                            mime.to_string(),
                            &mut images,
                            &mut text_parts,
                            &mut total_image_bytes,
                        );
                    } else {
                        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob) {
                            Ok(data) => {
                                if is_text_mime_type(mime) {
                                    if let Ok(text) = String::from_utf8(data) {
                                        text_parts.push(text);
                                    } else {
                                        text_parts.push("[resource omitted]".to_string());
                                    }
                                } else {
                                    let uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                                    let ext = if uri.contains('.') {
                                        let clean = uri.split('?').next().unwrap_or(uri);
                                        std::path::Path::new(clean)
                                            .extension()
                                            .and_then(|s| s.to_str())
                                            .map(|e| format!(".{e}"))
                                            .unwrap_or_else(|| ".bin".to_string())
                                    } else {
                                        ".bin".to_string()
                                    };
                                    let save_res = if let Some(ref s) = saver {
                                        s(data, ext).await
                                    } else {
                                        save_mcp_output_file_async(data, ext).await
                                    };
                                    match save_res {
                                        Ok(p) => {
                                            text_parts.push(format!("[Binary resource {uri} saved to {}]", p.display()));
                                        }
                                        Err(e) => {
                                            text_parts.push(format!("[Binary resource {uri} could not be saved: {e}]"));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                                text_parts.push(format!("[Binary resource {uri} failed to decode base64: {e}]"));
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
        let save_res = if let Some(ref s) = saver {
            s(combined_text.as_bytes().to_vec(), ".txt".to_string()).await.ok()
        } else {
            save_mcp_output_file_async(combined_text.as_bytes().to_vec(), ".txt".to_string())
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
        }
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
        self.tool_info
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

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
                    msg.push_str(&format!(" - {m}"));
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
            timeout: None,
            cancel: Some(cancel.clone()),
            on_progress: progress_cb,
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
            for (srv, outcome) in futures::future::join_all(requests).await {
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
            for (srv, outcome) in futures::future::join_all(requests).await {
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
                } else if let (Some(blob), Some(mime)) = (
                    item.get("blob").and_then(|b| b.as_str()),
                    item.get("mimeType").and_then(|m| m.as_str()),
                ) {
                    if mime.starts_with("image/") {
                        push_capped_image(
                            blob.to_string(),
                            mime.to_string(),
                            &mut blocks,
                            &mut text_parts,
                            &mut total_image_bytes,
                        );
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
}
