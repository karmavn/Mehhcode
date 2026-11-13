//! JSON-RPC 2.0 protocol types and framing for MCP.

use std::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Returns the next sequential monotonic request ID as a u64.
pub fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Generates a unique monotonic request ID formatted as a `serde_json::Value::Number`.
pub fn create_request_id() -> serde_json::Value {
    serde_json::Value::from(next_request_id())
}

/// Normalizes a JSON-RPC ID (number or string) into a consistent string key for map indexing.
pub fn id_to_key(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Standard JSON-RPC 2.0 Request message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: serde_json::Value, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// Standard JSON-RPC 2.0 Notification message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }
}

/// Standard JSON-RPC 2.0 Error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

/// Standard JSON-RPC 2.0 Response message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: serde_json::Value, code: i64, message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError::new(code, message, data)),
        }
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// Unified JSON-RPC 2.0 wire message enum.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

impl Serialize for JsonRpcMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            JsonRpcMessage::Request(r) => r.serialize(serializer),
            JsonRpcMessage::Notification(n) => n.serialize(serializer),
            JsonRpcMessage::Response(resp) => resp.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = val.as_object() {
            let has_id = obj.contains_key("id");
            let has_method = obj.contains_key("method");
            if has_method {
                if has_id {
                    let req: JsonRpcRequest = serde_json::from_value(val).map_err(serde::de::Error::custom)?;
                    return Ok(JsonRpcMessage::Request(req));
                } else {
                    let notif: JsonRpcNotification = serde_json::from_value(val).map_err(serde::de::Error::custom)?;
                    return Ok(JsonRpcMessage::Notification(notif));
                }
            } else if has_id {
                let resp: JsonRpcResponse = serde_json::from_value(val).map_err(serde::de::Error::custom)?;
                return Ok(JsonRpcMessage::Response(resp));
            }
        }
        Err(serde::de::Error::custom("Invalid JSON-RPC 2.0 message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_request_id() {
        let id1 = create_request_id();
        let id2 = create_request_id();
        assert_ne!(id1, id2);
        assert!(id1.is_number());
    }

    #[test]
    fn test_id_to_key() {
        assert_eq!(id_to_key(&serde_json::json!(42)), "42");
        assert_eq!(id_to_key(&serde_json::json!("abc")), "abc");
    }

    #[test]
    fn test_request_serde() {
        let req = JsonRpcRequest::new(serde_json::json!(1), "tools/list", Some(serde_json::json!({ "cursor": "c1" })));
        let json_str = serde_json::to_string(&req).unwrap();
        assert!(json_str.contains("\"method\":\"tools/list\""));
        assert!(json_str.contains("\"id\":1"));

        let msg: JsonRpcMessage = serde_json::from_str(&json_str).unwrap();
        match msg {
            JsonRpcMessage::Request(r) => {
                assert_eq!(r.method, "tools/list");
                assert_eq!(r.id, serde_json::json!(1));
            }
            _ => panic!("Expected Request variant"),
        }
    }

    #[test]
    fn test_notification_serde() {
        let notif = JsonRpcNotification::new("notifications/initialized", None);
        let json_str = serde_json::to_string(&notif).unwrap();
        assert!(!json_str.contains("\"id\""));

        let msg: JsonRpcMessage = serde_json::from_str(&json_str).unwrap();
        match msg {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(n.method, "notifications/initialized");
                assert!(n.params.is_none());
            }
            _ => panic!("Expected Notification variant"),
        }
    }

    #[test]
    fn test_response_serde() {
        let resp = JsonRpcResponse::success(serde_json::json!(2), serde_json::json!({ "status": "ok" }));
        let json_str = serde_json::to_string(&resp).unwrap();
        let msg: JsonRpcMessage = serde_json::from_str(&json_str).unwrap();
        match msg {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(2));
                assert!(r.is_success());
                assert_eq!(r.result.unwrap()["status"], "ok");
            }
            _ => panic!("Expected Response variant"),
        }

        let err_resp = JsonRpcResponse::error(serde_json::json!(3), METHOD_NOT_FOUND, "Method not found", None);
        let err_json = serde_json::to_string(&err_resp).unwrap();
        let msg_err: JsonRpcMessage = serde_json::from_str(&err_json).unwrap();
        match msg_err {
            JsonRpcMessage::Response(r) => {
                assert!(!r.is_success());
                assert_eq!(r.error.unwrap().code, METHOD_NOT_FOUND);
            }
            _ => panic!("Expected Response variant"),
        }
    }
}
