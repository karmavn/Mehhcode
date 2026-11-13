//! MCP tool adapter exposing MCP tools as `mehh_agent::AgentTool` implementations.

use std::sync::Arc;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, TextContent, ToolResultContentBlock};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::client::{McpCallToolResult, McpClient, McpContentBlock, McpToolInfo};

pub const MAX_TOOL_NAME_LENGTH: usize = 64;
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024; // 20KB model-facing limit

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

/// Converts an MCP tool call result into an `AgentToolResult` conforming to `mehh_agent`.
pub fn convert_mcp_result(call_res: McpCallToolResult) -> AgentToolResult {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();

    for block in call_res.content {
        match block {
            McpContentBlock::Text { text } => {
                text_parts.push(text);
            }
            McpContentBlock::Image { data, mime_type } => {
                images.push(ToolResultContentBlock::Image(ImageContent {
                    data,
                    mime_type,
                }));
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
                } else if let (Some(blob), Some(mime)) = (
                    resource.get("blob").and_then(|b| b.as_str()),
                    resource.get("mimeType").and_then(|m| m.as_str()),
                ) {
                    if mime.starts_with("image/") {
                        images.push(ToolResultContentBlock::Image(ImageContent {
                            data: blob.to_string(),
                            mime_type: mime.to_string(),
                        }));
                    } else {
                        text_parts.push("[binary resource omitted]".to_string());
                    }
                } else {
                    text_parts.push("[resource omitted]".to_string());
                }
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
        blocks.push(ToolResultContentBlock::Text(TextContent {
            text: truncated,
            text_signature: None,
        }));
    } else if !combined_text.is_empty() {
        blocks.push(ToolResultContentBlock::Text(TextContent {
            text: combined_text,
            text_signature: None,
        }));
    }

    blocks.extend(images);

    AgentToolResult {
        content: blocks,
        details: call_res.structured_content,
    }
}

/// Agent tool adapter wrapping an MCP tool registered on an MCP client.
pub struct McpAgentTool {
    server_name: String,
    tool_name: String,
    adapter_name: String,
    tool_info: McpToolInfo,
    client: Arc<McpClient>,
}

impl McpAgentTool {
    /// Creates a new MCP agent tool adapter.
    pub fn new(server_name: impl Into<String>, tool_info: McpToolInfo, client: Arc<McpClient>) -> Self {
        let server = server_name.into();
        let adapter_name = create_mcp_tool_name(&server, &tool_info.name);
        let tool_name = tool_info.name.clone();
        Self {
            server_name: server,
            tool_name,
            adapter_name,
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
        self.tool_info.description.as_deref().unwrap_or("")
    }

    fn parameters(&self) -> serde_json::Value {
        self.tool_info.input_schema.clone()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
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

        let arguments = serde_json::Value::Object(params);
        let call_fut = self.client.call_tool(&self.tool_name, arguments, None);

        let res = tokio::select! {
            r = call_fut => r.map_err(|e| e.to_string())?,
            _ = cancel.cancelled() => return Err("Tool execution cancelled".to_string()),
        };

        let is_error = res.is_error;
        let tool_res = convert_mcp_result(res);

        if is_error {
            let err_text = tool_res
                .content
                .iter()
                .filter_map(|b| match b {
                    ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Err(if err_text.is_empty() {
                "Tool execution failed".to_string()
            } else {
                err_text
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
        "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models. Prefer resources over web search when possible."
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
                .request("resources/list", Some(req_params), None)
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

            for (srv, client) in reg {
                match client.list_resources().await {
                    Ok(res_list) => {
                        for r in res_list {
                            if r.uri.starts_with("ui://") || r.uri.contains("profile=mcp-app") {
                                continue;
                            }
                            all_resources.push(serde_json::json!({
                                "server": srv,
                                "uri": r.uri,
                                "name": r.name,
                                "description": r.description,
                                "mimeType": r.mime_type
                            }));
                        }
                    }
                    Err(e) => {
                        errors.push(serde_json::json!({
                            "server": srv,
                            "error": e.to_string()
                        }));
                    }
                }
            }

            serde_json::json!({
                "resources": all_resources,
                "errors": errors
            })
        };

        let formatted = serde_json::to_string_pretty(&result_json).unwrap_or_default();
        Ok(AgentToolResult {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: formatted,
                text_signature: None,
            })],
            details: Some(result_json),
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
        "Lists parameterized RFC 6570 resource templates provided by MCP servers."
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
                .request("resources/templates/list", Some(req_params), None)
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

            for (srv, client) in reg {
                match client.list_resource_templates().await {
                    Ok(tmpl_list) => {
                        for mut t in tmpl_list {
                            if let Some(map) = t.as_object_mut() {
                                map.insert("server".to_string(), serde_json::json!(srv));
                            }
                            all_templates.push(t);
                        }
                    }
                    Err(e) => {
                        errors.push(serde_json::json!({
                            "server": srv,
                            "error": e.to_string()
                        }));
                    }
                }
            }

            serde_json::json!({
                "resourceTemplates": all_templates,
                "errors": errors
            })
        };

        let formatted = serde_json::to_string_pretty(&result_json).unwrap_or_default();
        Ok(AgentToolResult {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: formatted,
                text_signature: None,
            })],
            details: Some(result_json),
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

        let res = client.read_resource(uri).await.map_err(|e| e.to_string())?;

        let mut blocks = Vec::new();
        let mut text_parts = Vec::new();

        if let Some(contents_arr) = res.get("contents").and_then(|v| v.as_array()) {
            for item in contents_arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                } else if let (Some(blob), Some(mime)) = (
                    item.get("blob").and_then(|b| b.as_str()),
                    item.get("mimeType").and_then(|m| m.as_str()),
                ) {
                    if mime.starts_with("image/") {
                        blocks.push(ToolResultContentBlock::Image(ImageContent {
                            data: blob.to_string(),
                            mime_type: mime.to_string(),
                        }));
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
            blocks.insert(
                0,
                ToolResultContentBlock::Text(TextContent {
                    text: combined,
                    text_signature: None,
                }),
            );
        }

        Ok(AgentToolResult {
            content: blocks,
            details: Some(res),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
