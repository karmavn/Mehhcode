//! High-level MCP client handling handshake, tool invocation, and resource management.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex, RwLock};

use crate::jsonrpc::{
    create_request_id, id_to_key, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse,
};
use crate::transport::McpTransport;

pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// Root path exposed to the MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpRoot {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Information describing a tool registered on an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(alias = "input_schema")]
    pub input_schema: serde_json::Value,
}

/// Information describing a resource hosted by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(alias = "mime_type")]
    pub mime_type: Option<String>,
}

/// Content blocks returned from MCP tool calls or resource reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(alias = "mime_type")]
        mime_type: String,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(alias = "mime_type")]
        mime_type: String,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, alias = "mime_type", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    #[serde(rename = "resource")]
    Resource {
        resource: serde_json::Value,
    },
}

/// Result returned from an MCP `tools/call` invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpCallToolResult {
    #[serde(default)]
    pub content: Vec<McpContentBlock>,
    #[serde(default, alias = "is_error")]
    pub is_error: bool,
    #[serde(default, alias = "structured_content", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
}

type PendingResponseSender = oneshot::Sender<Result<serde_json::Value, String>>;
type PendingRequestsMap = Arc<Mutex<HashMap<String, PendingResponseSender>>>;

/// Asynchronous client for communicating with an MCP server over an `McpTransport`.
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    pending_requests: PendingRequestsMap,
    is_closed: Arc<AtomicBool>,
    server_protocol_version: Arc<RwLock<Option<String>>>,
    server_info: Arc<RwLock<Option<serde_json::Value>>>,
    server_capabilities: Arc<RwLock<Option<serde_json::Value>>>,
    roots: Arc<Vec<McpRoot>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl McpClient {
    /// Connects to an MCP server using the provided transport, negotiating protocol version
    /// and initializing the session.
    pub async fn connect(transport: Arc<dyn McpTransport>) -> anyhow::Result<Self> {
        Self::connect_with_roots(transport, Vec::new()).await
    }

    /// Connects to an MCP server providing client workspace roots and handling incoming roots/list requests.
    pub async fn connect_with_roots(
        transport: Arc<dyn McpTransport>,
        roots: Vec<McpRoot>,
    ) -> anyhow::Result<Self> {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let server_protocol_version = Arc::new(RwLock::new(None));
        let server_info = Arc::new(RwLock::new(None));
        let server_capabilities = Arc::new(RwLock::new(None));
        let roots_arc = Arc::new(roots);

        let client = Self {
            transport: transport.clone(),
            pending_requests: pending.clone(),
            is_closed: closed.clone(),
            server_protocol_version: server_protocol_version.clone(),
            server_info: server_info.clone(),
            server_capabilities: server_capabilities.clone(),
            roots: roots_arc.clone(),
            reader_task: Mutex::new(None),
        };

        // Start background reader task
        let transport_reader = transport.clone();
        let pending_reader = pending.clone();
        let closed_reader = closed.clone();
        let roots_reader = roots_arc.clone();

        let reader_handle = tokio::spawn(async move {
            let mut close_reason: Option<String> = None;
            while let Some(msg_res) = transport_reader.recv().await {
                match msg_res {
                    Ok(JsonRpcMessage::Response(resp)) => {
                        let key = id_to_key(&resp.id);
                        let mut map = pending_reader.lock().await;
                        if let Some(tx) = map.remove(&key) {
                            if let Some(err) = resp.error {
                                let _ = tx.send(Err(format!("MCP error {}: {}", err.code, err.message)));
                            } else {
                                let val = resp.result.unwrap_or(serde_json::Value::Null);
                                let _ = tx.send(Ok(val));
                            }
                        }
                    }
                    Ok(JsonRpcMessage::Request(req)) => {
                        if req.method == "ping" {
                            let resp = JsonRpcResponse::success(req.id, serde_json::json!({}));
                            let _ = transport_reader.send(JsonRpcMessage::Response(resp)).await;
                        } else if req.method == "roots/list" {
                            let resp = JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({ "roots": *roots_reader }),
                            );
                            let _ = transport_reader.send(JsonRpcMessage::Response(resp)).await;
                        }
                    }
                    Ok(JsonRpcMessage::Notification(_notif)) => {
                        // Notifications (e.g. progress, log messages) are acknowledged silently
                    }
                    Err(e) => {
                        let err_msg = format!("MCP transport error: {e}");
                        tracing::warn!("{err_msg}");
                        let mut map = pending_reader.lock().await;
                        for (_, tx) in map.drain() {
                            let _ = tx.send(Err(err_msg.clone()));
                        }
                        close_reason = Some(err_msg);
                    }
                }
            }
            closed_reader.store(true, Ordering::SeqCst);
            let final_reason = close_reason.unwrap_or_else(|| "MCP connection closed".to_string());
            let mut map = pending_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(final_reason.clone()));
            }
        });
        *client.reader_task.lock().await = Some(reader_handle);

        let handshake_res: anyhow::Result<()> = async {
            // Send initialize request
            let capabilities = if roots_arc.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({
                    "roots": {
                        "listChanged": false
                    }
                })
            };

            let init_params = serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": {
                    "name": "mehh",
                    "version": "0.1.0"
                }
            });

            let init_res = client
                .request("initialize", Some(init_params), Some(Duration::from_secs(15)))
                .await?;

            let negotiated_version = init_res
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05")
                .to_string();

            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated_version.as_str()) {
                anyhow::bail!(
                    "MCP server selected unsupported protocol version: {negotiated_version}"
                );
            }

            {
                let mut pv_guard = server_protocol_version.write().await;
                *pv_guard = Some(negotiated_version);
            }
            if let Some(s_info) = init_res.get("serverInfo") {
                let mut info_guard = server_info.write().await;
                *info_guard = Some(s_info.clone());
            }
            if let Some(s_caps) = init_res.get("capabilities") {
                let mut caps_guard = server_capabilities.write().await;
                *caps_guard = Some(s_caps.clone());
            }

            // Send notifications/initialized
            let notif = JsonRpcNotification::new("notifications/initialized", None);
            tokio::time::timeout(
                Duration::from_secs(15),
                transport.send(JsonRpcMessage::Notification(notif)),
            )
            .await
            .map_err(|_| anyhow::anyhow!("Timed out sending notifications/initialized"))??;

            Ok(())
        }
        .await;

        if let Err(e) = handshake_res {
            let _ = client.close().await;
            return Err(e);
        }

        Ok(client)
    }

    /// Dispatches a JSON-RPC request to the server and awaits the response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<serde_json::Value> {
        if self.is_closed.load(Ordering::SeqCst) {
            anyhow::bail!("MCP connection is closed");
        }

        let id = create_request_id();
        let key = id_to_key(&id);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending_requests.lock().await;
            map.insert(key.clone(), tx);
        }

        let timeout_dur = timeout.unwrap_or(Duration::from_secs(30));
        let req = JsonRpcRequest::new(id, method, params);
        let outcome = tokio::time::timeout(timeout_dur, async {
            self.transport.send(JsonRpcMessage::Request(req)).await?;
            match rx.await {
                Ok(Ok(val)) => Ok(val),
                Ok(Err(err_msg)) => anyhow::bail!("{err_msg}"),
                Err(_) => anyhow::bail!("MCP request sender dropped / connection closed"),
            }
        })
        .await;

        match outcome {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(e)) => {
                let mut map = self.pending_requests.lock().await;
                map.remove(&key);
                Err(e)
            }
            Err(_) => {
                let mut map = self.pending_requests.lock().await;
                map.remove(&key);
                anyhow::bail!("MCP request timed out after {}ms", timeout_dur.as_millis());
            }
        }
    }

    /// Sends a JSON-RPC notification message.
    pub async fn notify(&self, method: &str, params: Option<serde_json::Value>) -> anyhow::Result<()> {
        if self.is_closed.load(Ordering::SeqCst) {
            anyhow::bail!("MCP connection is closed");
        }
        let notif = JsonRpcNotification::new(method, params);
        self.transport.send(JsonRpcMessage::Notification(notif)).await
    }

    /// Fetches the complete list of tools from the server, traversing paginated cursors.
    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = std::collections::HashSet::new();

        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let res = self.request("tools/list", Some(params), None).await?;
            if let Some(tools_arr) = res.get("tools").and_then(|v| v.as_array()) {
                for tool_val in tools_arr {
                    let tool_info: McpToolInfo = serde_json::from_value(tool_val.clone())?;
                    all_tools.push(tool_info);
                }
            }
            cursor = res
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            match &cursor {
                None => break,
                Some(c) => {
                    if !seen_cursors.insert(c.clone()) {
                        anyhow::bail!("MCP tools/list returned duplicate cursor: {c}");
                    }
                }
            }
        }
        Ok(all_tools)
    }

    /// Calls a specific tool by name with arguments and optional timeout.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Option<Duration>,
    ) -> anyhow::Result<McpCallToolResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });
        let res = self.request("tools/call", Some(params), timeout).await?;
        let call_res: McpCallToolResult = serde_json::from_value(res)?;
        Ok(call_res)
    }

    /// Fetches the complete list of resources from the server, traversing paginated cursors.
    pub async fn list_resources(&self) -> anyhow::Result<Vec<McpResourceInfo>> {
        let mut all_resources = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = std::collections::HashSet::new();

        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let res = self.request("resources/list", Some(params), None).await?;
            if let Some(res_arr) = res.get("resources").and_then(|v| v.as_array()) {
                for r_val in res_arr {
                    let r_info: McpResourceInfo = serde_json::from_value(r_val.clone())?;
                    all_resources.push(r_info);
                }
            }
            cursor = res
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            match &cursor {
                None => break,
                Some(c) => {
                    if !seen_cursors.insert(c.clone()) {
                        anyhow::bail!("MCP resources/list returned duplicate cursor: {c}");
                    }
                }
            }
        }
        Ok(all_resources)
    }

    /// Reads a specified resource by its URI.
    pub async fn read_resource(&self, uri: &str) -> anyhow::Result<serde_json::Value> {
        let params = serde_json::json!({ "uri": uri });
        let res = self.request("resources/read", Some(params), None).await?;
        Ok(res)
    }

    /// Fetches the complete list of resource templates from the server, traversing paginated cursors.
    pub async fn list_resource_templates(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut all_templates = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = std::collections::HashSet::new();

        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let res = self
                .request("resources/templates/list", Some(params), None)
                .await?;
            if let Some(templates_arr) = res.get("resourceTemplates").and_then(|v| v.as_array()) {
                for t_val in templates_arr {
                    all_templates.push(t_val.clone());
                }
            }
            cursor = res
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            match &cursor {
                None => break,
                Some(c) => {
                    if !seen_cursors.insert(c.clone()) {
                        anyhow::bail!("MCP resources/templates/list returned duplicate cursor: {c}");
                    }
                }
            }
        }
        Ok(all_templates)
    }

    /// Returns the roots configured for this client.
    pub fn roots(&self) -> &[McpRoot] {
        &self.roots
    }

    /// Sends a ping request to the MCP server.
    pub async fn ping(&self) -> anyhow::Result<()> {
        self.request("ping", None, None).await?;
        Ok(())
    }

    /// Closes the client and tears down the underlying transport.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.is_closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }
        {
            let mut map = self.pending_requests.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err("MCP connection closed".to_string()));
            }
        }
        self.transport.close().await
    }

    /// Returns whether the client connection is closed.
    pub fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }

    /// Returns the negotiated protocol version, if initialized.
    pub async fn negotiated_protocol_version(&self) -> Option<String> {
        self.server_protocol_version.read().await.clone()
    }

    /// Returns the server information negotiated during initialization, if available.
    pub async fn server_info(&self) -> Option<serde_json::Value> {
        self.server_info.read().await.clone()
    }

    /// Returns the server capabilities negotiated during initialization, if available.
    pub async fn server_capabilities(&self) -> Option<serde_json::Value> {
        self.server_capabilities.read().await.clone()
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use async_trait::async_trait;

    struct SlowSendTransport {
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl McpTransport for SlowSendTransport {
        async fn send(&self, _msg: JsonRpcMessage) -> anyhow::Result<()> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        }

        async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            None
        }

        async fn close(&self) -> anyhow::Result<()> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_request_timeout_covers_send_phase() {
        let closed = Arc::new(AtomicBool::new(false));
        let client = McpClient {
            transport: Arc::new(SlowSendTransport {
                closed: closed.clone(),
            }),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            is_closed: Arc::new(AtomicBool::new(false)),
            server_protocol_version: Arc::new(RwLock::new(None)),
            server_info: Arc::new(RwLock::new(None)),
            server_capabilities: Arc::new(RwLock::new(None)),
            roots: Arc::new(Vec::new()),
            reader_task: Mutex::new(None),
        };

        let start = std::time::Instant::now();
        let err = client
            .request("tools/list", None, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
        assert!(client.pending_requests.lock().await.is_empty());
    }

    struct FailingInitTransport {
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl McpTransport for FailingInitTransport {
        async fn send(&self, _msg: JsonRpcMessage) -> anyhow::Result<()> {
            anyhow::bail!("mock send failure")
        }

        async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            None
        }

        async fn close(&self) -> anyhow::Result<()> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_connect_failure_closes_transport() {
        let closed = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(FailingInitTransport {
            closed: closed.clone(),
        });
        let res = McpClient::connect(transport).await;
        assert!(res.is_err());
        assert!(closed.load(Ordering::SeqCst));
    }
}
