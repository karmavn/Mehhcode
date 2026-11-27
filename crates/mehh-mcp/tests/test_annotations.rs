use mehh_mcp::client::{McpClient, McpToolAnnotations, McpToolInfo};
use mehh_mcp::jsonrpc::{JsonRpcMessage, JsonRpcResponse};
use mehh_mcp::tool_adapter::McpAgentTool;
use mehh_mcp::transport::{InMemoryTransport, McpTransport};
use mehh_agent::{AgentTool, ToolExecutionMode};
use std::sync::Arc;

#[test]
fn test_mcp_tool_info_deserializes_annotations() {
    let json = serde_json::json!({
        "name": "read_query",
        "description": "Run SQL query",
        "inputSchema": { "type": "object" },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false
        }
    });
    let info: McpToolInfo = serde_json::from_value(json).expect("deserialize McpToolInfo");
    assert!(info.annotations.is_some());
    let annotations = info.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, None);
    assert_eq!(annotations.open_world_hint, None);
}

#[tokio::test]
async fn test_mcp_agent_tool_reflects_read_only_hint() {
    let (client_transport, server_transport) = InMemoryTransport::pair();
    let client_transport = Arc::new(client_transport);
    let server_transport = Arc::new(server_transport);

    let _server_handle = tokio::spawn(async move {
        while let Some(Ok(msg)) = server_transport.recv().await {
            if let JsonRpcMessage::Request(req) = msg {
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
        }
    });

    let client = Arc::new(McpClient::connect(client_transport).await.unwrap());

    let read_tool_info = McpToolInfo {
        name: "get_weather".to_string(),
        description: Some("Get weather".to_string()),
        input_schema: serde_json::json!({ "type": "object" }),
        annotations: Some(McpToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }),
    };
    let read_tool = McpAgentTool::new("test_server", read_tool_info, client.clone());
    assert_eq!(read_tool.is_read_only(), Some(true));
    assert_eq!(read_tool.execution_mode(), ToolExecutionMode::Parallel);
    assert_eq!(read_tool.is_safe_for_auto_approve(&serde_json::Map::new()), Some(false));

    let mutate_tool_info = McpToolInfo {
        name: "drop_table".to_string(),
        description: Some("Drop table".to_string()),
        input_schema: serde_json::json!({ "type": "object" }),
        annotations: Some(McpToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            ..Default::default()
        }),
    };
    let mutate_tool = McpAgentTool::new("test_server", mutate_tool_info, client.clone());
    assert_eq!(mutate_tool.is_read_only(), Some(false));
    assert_eq!(mutate_tool.execution_mode(), ToolExecutionMode::Sequential);
    assert_eq!(mutate_tool.is_safe_for_auto_approve(&serde_json::Map::new()), Some(false));

    let conflicting_tool_info = McpToolInfo {
        name: "read_and_wipe".to_string(),
        description: Some("Conflicting annotations".to_string()),
        input_schema: serde_json::json!({ "type": "object" }),
        annotations: Some(McpToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            ..Default::default()
        }),
    };
    let conflicting_tool = McpAgentTool::new("test_server", conflicting_tool_info, client.clone());
    assert_eq!(conflicting_tool.is_read_only(), Some(false));
    assert_eq!(conflicting_tool.execution_mode(), ToolExecutionMode::Sequential);
    assert_eq!(
        conflicting_tool.is_safe_for_auto_approve(&serde_json::Map::new()),
        Some(false)
    );

    let unannotated_info = McpToolInfo {
        name: "unknown_action".to_string(),
        description: None,
        input_schema: serde_json::json!({ "type": "object" }),
        annotations: None,
    };
    let unannotated_tool = McpAgentTool::new("test_server", unannotated_info, client);
    assert_eq!(unannotated_tool.is_read_only(), None);
    assert_eq!(unannotated_tool.execution_mode(), ToolExecutionMode::Sequential);
    assert_eq!(
        unannotated_tool.is_safe_for_auto_approve(&serde_json::Map::new()),
        Some(false)
    );
}
