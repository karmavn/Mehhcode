//! Model Context Protocol (MCP) implementation for the mehh agent system.
//! Provides JSON-RPC framing, stdio and streamable HTTP transports, high-level client,
//! configuration management, and `AgentTool` adapters.

pub mod client;
pub mod config;
pub mod jsonrpc;
pub mod oauth;
pub mod tool_adapter;
pub mod transport;

pub use client::*;
pub use config::*;
pub use jsonrpc::*;
pub use oauth::*;
pub use tool_adapter::*;
pub use transport::*;

#[cfg(test)]
pub(crate) struct TempTestDir(std::path::PathBuf);

#[cfg(test)]
impl TempTestDir {
    pub(crate) fn new(prefix: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mehh_mcp_test_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use mehh_agent::AgentTool;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// Helper spawning a mock server task over an `InMemoryTransport` that handles handshake.
    fn spawn_mock_server(
        server_transport: InMemoryTransport,
        mut handler: impl FnMut(JsonRpcRequest) -> JsonRpcResponse + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(Ok(msg)) = server_transport.recv().await {
                match msg {
                    JsonRpcMessage::Request(req) => {
                        let resp = if req.method == "initialize" {
                            JsonRpcResponse::success(
                                req.id,
                                json!({
                                    "protocolVersion": "2025-06-18",
                                    "capabilities": {
                                        "tools": { "listChanged": true }
                                    },
                                    "serverInfo": {
                                        "name": "mock-mcp-server",
                                        "version": "1.0.0"
                                    }
                                }),
                            )
                        } else {
                            handler(req)
                        };
                        let _ = server_transport.send(JsonRpcMessage::Response(resp)).await;
                    }
                    JsonRpcMessage::Notification(_notif) => {
                        // Acknowledge notifications/initialized or others
                    }
                    JsonRpcMessage::Response(_) => {}
                }
            }
        })
    }

    #[tokio::test]
    async fn test_client_connect_and_initialize() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            JsonRpcResponse::success(req.id, json!({}))
        });

        let client = McpClient::connect(Arc::new(client_tr)).await.unwrap();
        assert!(!client.is_closed());
        let version = client.negotiated_protocol_version().await;
        assert_eq!(version.as_deref(), Some("2025-06-18"));

        client.close().await.unwrap();
        assert!(client.is_closed());
    }

    #[tokio::test]
    async fn test_client_list_tools_with_pagination() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            if req.method == "tools/list" {
                let cursor = req.params.as_ref().and_then(|p| p.get("cursor")).and_then(|c| c.as_str());
                if cursor.is_none() {
                    JsonRpcResponse::success(
                        req.id,
                        json!({
                            "tools": [
                                {
                                    "name": "tool_one",
                                    "description": "First tool",
                                    "inputSchema": { "type": "object" }
                                }
                            ],
                            "nextCursor": "page_2"
                        }),
                    )
                } else if cursor == Some("page_2") {
                    JsonRpcResponse::success(
                        req.id,
                        json!({
                            "tools": [
                                {
                                    "name": "tool_two",
                                    "description": "Second tool",
                                    "inputSchema": { "type": "object" }
                                }
                            ]
                        }),
                    )
                } else {
                    JsonRpcResponse::error(req.id, INVALID_PARAMS, "Unknown cursor", None)
                }
            } else {
                JsonRpcResponse::success(req.id, json!({}))
            }
        });

        let client = McpClient::connect(Arc::new(client_tr)).await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_one");
        assert_eq!(tools[1].name, "tool_two");
        assert_eq!(tools[0].description.as_deref(), Some("First tool"));
        assert_eq!(tools[1].description.as_deref(), Some("Second tool"));

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_client_call_tool() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            if req.method == "tools/call" {
                let tool_name = req.params.as_ref().and_then(|p| p.get("name")).and_then(|n| n.as_str());
                assert_eq!(tool_name, Some("calculator"));
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": "Result: 42"
                            }
                        ],
                        "isError": false,
                        "structuredContent": { "value": 42 }
                    }),
                )
            } else {
                JsonRpcResponse::success(req.id, json!({}))
            }
        });

        let client = McpClient::connect(Arc::new(client_tr)).await.unwrap();
        let res = client
            .call_tool("calculator", json!({ "expr": "6 * 7" }), None)
            .await
            .unwrap();

        assert!(!res.is_error);
        assert_eq!(res.content.len(), 1);
        match &res.content[0] {
            McpContentBlock::Text { text } => assert_eq!(text, "Result: 42"),
            _ => panic!("Expected Text block"),
        }
        assert_eq!(res.structured_content, Some(json!({ "value": 42 })));

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_client_error_propagation() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            JsonRpcResponse::error(req.id, METHOD_NOT_FOUND, "Custom server error", None)
        });

        let client = McpClient::connect(Arc::new(client_tr)).await.unwrap();
        let err = client.ping().await.unwrap_err();
        assert!(err.to_string().contains("Custom server error"));

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_mcp_agent_tool_execution() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            if req.method == "tools/call" {
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": "Successfully processed"
                            }
                        ],
                        "isError": false
                    }),
                )
            } else {
                JsonRpcResponse::success(req.id, json!({}))
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_tr)).await.unwrap());
        let tool_info = McpToolInfo {
            name: "echo_action".to_string(),
            description: Some("Echoes actions back".to_string()),
            input_schema: json!({ "type": "object" }),
            annotations: None,
        };

        let agent_tool = McpAgentTool::new("my_server", tool_info, client.clone());
        assert_eq!(agent_tool.name(), "mcp__my_server__echo_action");
        assert_eq!(agent_tool.label(), "echo_action");
        assert_eq!(agent_tool.description(), "Echoes actions back");

        let mut params = serde_json::Map::new();
        params.insert("msg".to_string(), json!("hello"));

        let result = agent_tool
            .execute("tc_1", params, CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => {
                assert_eq!(t.text, "Successfully processed");
            }
            _ => panic!("Expected text block"),
        }

        client.close().await.unwrap();
    }

    #[test]
    fn test_create_mcp_tool_name_truncation() {
        let short = create_mcp_tool_name("serv", "tool");
        assert_eq!(short, "mcp__serv__tool");

        let sanitized = create_mcp_tool_name("serv-name", "tool.action");
        assert_eq!(sanitized, "mcp__serv_name__tool_action");

        let long_serv = "a".repeat(50);
        let long_tool = "b".repeat(50);
        let capped = create_mcp_tool_name(&long_serv, &long_tool);
        assert_eq!(capped.len(), 64);
        assert_eq!(capped.chars().nth(55), Some('_'));
        // Verify last 8 characters are hex digits
        let hash_part = &capped[56..];
        assert_eq!(hash_part.len(), 8);
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_config_loading() {
        let temp_dir = crate::TempTestDir::new("config_loading");
        let mehh_dir = temp_dir.path().join(".mehh");
        std::fs::create_dir_all(&mehh_dir).unwrap();

        let config_json = json!({
            "mcpServers": {
                "local_server": {
                    "command": "python",
                    "args": ["server.py"],
                    "enabled": true,
                    "timeout": 30
                }
            }
        });
        std::fs::write(mehh_dir.join("mcp.json"), serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

        let untrusted = config::load_mcp_configs_from_paths(None, temp_dir.path(), false).unwrap();
        assert!(untrusted.is_empty(), "project mcp.json must be ignored when untrusted");

        let configs = config::load_mcp_configs_from_paths(None, temp_dir.path(), true).unwrap();
        assert!(configs.contains_key("local_server"));
        let srv = &configs["local_server"];
        assert_eq!(srv.command.as_deref(), Some("python"));
        assert_eq!(srv.args, vec!["server.py"]);
        assert!(srv.is_enabled());
    }

    #[tokio::test]
    async fn test_roots_list_server_to_client_request() {
        let (client_tr, server_tr) = InMemoryTransport::pair();

        let roots = vec![McpRoot {
            uri: "file:///workspace/project".to_string(),
            name: Some("project".to_string()),
        }];

        // Server task answering initialize and then sending roots/list
        let roots_check = tokio::spawn(async move {
            let mut init_seen = false;
            while let Some(Ok(msg)) = server_tr.recv().await {
                match msg {
                    JsonRpcMessage::Request(req) if req.method == "initialize" => {
                        init_seen = true;
                        let caps = req.params.as_ref().and_then(|p| p.get("capabilities"));
                        assert!(caps.is_some());
                        let roots_cap = caps.unwrap().get("roots");
                        assert!(roots_cap.is_some());

                        let resp = JsonRpcResponse::success(
                            req.id,
                            json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": { "name": "test-srv", "version": "1.0" }
                            }),
                        );
                        server_tr.send(JsonRpcMessage::Response(resp)).await.unwrap();
                    }
                    JsonRpcMessage::Notification(notif) if notif.method == "notifications/initialized" => {
                        // After initialized notification, send roots/list request to client
                        let req_id = create_request_id();
                        let roots_req = JsonRpcRequest::new(req_id.clone(), "roots/list", None);
                        server_tr.send(JsonRpcMessage::Request(roots_req)).await.unwrap();
                    }
                    JsonRpcMessage::Response(resp) => {
                        // Received response from client for roots/list!
                        let result = resp.result.expect("Expected result from client");
                        let roots_arr = result.get("roots").and_then(|r| r.as_array()).expect("Expected roots array");
                        assert_eq!(roots_arr.len(), 1);
                        assert_eq!(roots_arr[0].get("uri").unwrap().as_str(), Some("file:///workspace/project"));
                        assert_eq!(roots_arr[0].get("name").unwrap().as_str(), Some("project"));
                        break;
                    }
                    _ => {}
                }
            }
            assert!(init_seen);
        });

        let client = McpClient::connect_with_roots(Arc::new(client_tr), roots).await.unwrap();
        assert_eq!(client.roots().len(), 1);

        roots_check.await.unwrap();
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_resource_meta_tools_integration() {
        let (client_tr, server_tr) = InMemoryTransport::pair();
        let _server_handle = spawn_mock_server(server_tr, |req| {
            if req.method == "resources/list" {
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "resources": [
                            {
                                "uri": "file:///docs/readme.md",
                                "name": "Readme",
                                "description": "Project documentation",
                                "mimeType": "text/markdown"
                            },
                            {
                                "uri": "ui://dashboard/view",
                                "name": "App UI",
                                "mimeType": "text/html"
                            }
                        ]
                    }),
                )
            } else if req.method == "resources/templates/list" {
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "resourceTemplates": [
                            {
                                "uriTemplate": "file:///docs/{page}",
                                "name": "Doc page",
                                "mimeType": "text/markdown"
                            }
                        ]
                    }),
                )
            } else if req.method == "resources/read" {
                let uri = req.params.as_ref().and_then(|p| p.get("uri")).and_then(|u| u.as_str());
                assert_eq!(uri, Some("file:///docs/readme.md"));
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "contents": [
                            {
                                "uri": "file:///docs/readme.md",
                                "mimeType": "text/markdown",
                                "text": "# Sample Readme\nThis is content."
                            }
                        ]
                    }),
                )
            } else {
                JsonRpcResponse::success(req.id, json!({}))
            }
        });

        let client = Arc::new(McpClient::connect(Arc::new(client_tr)).await.unwrap());
        let registry: McpServerRegistry = Arc::new(tokio::sync::RwLock::new(vec![("srv1".to_string(), client.clone())]));

        // Test list_mcp_resources tool
        let list_tool = ListMcpResourcesTool::new(registry.clone());
        assert_eq!(list_tool.name(), "list_mcp_resources");

        let mut list_params = serde_json::Map::new();
        list_params.insert("server".to_string(), json!("srv1"));
        let list_res = list_tool
            .execute("call_list_1", list_params, CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(list_res.content.len(), 1);
        let list_json = list_res.details.unwrap();
        let res_arr = list_json.get("resources").and_then(|r| r.as_array()).unwrap();
        // Ensure "ui://" resource was filtered out, leaving 1 resource
        assert_eq!(res_arr.len(), 1);
        assert_eq!(res_arr[0].get("name").unwrap().as_str(), Some("Readme"));

        // Test list_mcp_resource_templates tool
        let tmpl_tool = ListMcpResourceTemplatesTool::new(registry.clone());
        let tmpl_res = tmpl_tool
            .execute("call_tmpl_1", serde_json::Map::new(), CancellationToken::new(), None)
            .await
            .unwrap();
        let tmpl_json = tmpl_res.details.unwrap();
        let tmpls = tmpl_json.get("resourceTemplates").and_then(|t| t.as_array()).unwrap();
        assert_eq!(tmpls.len(), 1);

        // Test read_mcp_resource tool
        let read_tool = ReadMcpResourceTool::new(registry);
        assert_eq!(read_tool.name(), "read_mcp_resource");

        let mut read_params = serde_json::Map::new();
        read_params.insert("server".to_string(), json!("srv1"));
        read_params.insert("uri".to_string(), json!("file:///docs/readme.md"));

        let read_res = read_tool
            .execute("call_read_1", read_params, CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(read_res.content.len(), 1);
        match &read_res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => {
                assert!(t.text.contains("# Sample Readme"));
            }
            _ => panic!("Expected text content block"),
        }

        client.close().await.unwrap();
    }
}
