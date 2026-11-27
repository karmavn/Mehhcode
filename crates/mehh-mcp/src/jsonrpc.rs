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

/// Normalizes a JSON-RPC ID into a map key that preserves its JSON type.
///
/// A number `42` and a string `"42"` are distinct JSON-RPC IDs and must not
/// alias each other in pending-request or incoming-request maps.
pub fn id_to_key(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => format!("s:{s}"),
        serde_json::Value::Number(n) => format!("n:{n}"),
        other => format!("o:{other}"),
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
        let mut val = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = val.as_object_mut() {
            let Some(jsonrpc) = obj.remove("jsonrpc").and_then(|v| match v {
                serde_json::Value::String(s) if s == "2.0" => Some(s),
                _ => None,
            }) else {
                return Err(serde::de::Error::custom(
                    "Invalid JSON-RPC version: expected '2.0'",
                ));
            };
            let id_opt = obj.remove("id");
            let has_id = id_opt.as_ref().is_some_and(|v| !v.is_null());
            if let Some(method_val) = obj.remove("method") {
                let method: String =
                    serde_json::from_value(method_val).map_err(serde::de::Error::custom)?;
                let params = match obj.remove("params") {
                    Some(serde_json::Value::Null) | None => None,
                    Some(p) => Some(p),
                };
                if has_id {
                    return Ok(JsonRpcMessage::Request(JsonRpcRequest {
                        jsonrpc,
                        id: id_opt.unwrap_or(serde_json::Value::Null),
                        method,
                        params,
                    }));
                } else {
                    return Ok(JsonRpcMessage::Notification(JsonRpcNotification {
                        jsonrpc,
                        method,
                        params,
                    }));
                }
            } else if let Some(id) = id_opt {
                // A null id is only valid on a response (e.g. a parse-error reply), never on a request.
                let result = match obj.remove("result") {
                    Some(serde_json::Value::Null) | None => None,
                    Some(r) => Some(r),
                };
                let error = match obj.remove("error") {
                    Some(serde_json::Value::Null) | None => None,
                    Some(e) => Some(
                        serde_json::from_value::<JsonRpcError>(e)
                            .map_err(serde::de::Error::custom)?,
                    ),
                };
                return Ok(JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc,
                    id,
                    result,
                    error,
                }));
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
        assert_eq!(id_to_key(&serde_json::json!(42)), "n:42");
        assert_eq!(id_to_key(&serde_json::json!("42")), "s:42");
        assert_ne!(
            id_to_key(&serde_json::json!(42)),
            id_to_key(&serde_json::json!("42"))
        );
        assert_eq!(id_to_key(&serde_json::json!("abc")), "s:abc");
    }

    #[test]
    fn test_message_rejects_wrong_version_and_null_id_request() {
        let bad_version = serde_json::json!({
            "jsonrpc": "1.0", "method": "tools/list", "id": 1
        });
        assert!(serde_json::from_value::<JsonRpcMessage>(bad_version).is_err());

        let null_id_request = serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/list", "id": null
        });
        assert!(matches!(
            serde_json::from_value::<JsonRpcMessage>(null_id_request).unwrap(),
            JsonRpcMessage::Notification(_)
        ));

        let null_id_response = serde_json::json!({
            "jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}
        });
        assert!(matches!(
            serde_json::from_value::<JsonRpcMessage>(null_id_response).unwrap(),
            JsonRpcMessage::Response(_)
        ));
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
