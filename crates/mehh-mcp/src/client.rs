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

/// Annotations providing execution hints about an MCP tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
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
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(
            default,
            rename = "mimeType",
            alias = "mime_type",
            skip_serializing_if = "Option::is_none"
        )]
        mime_type: Option<String>,
    },
    #[serde(rename = "resource")]
    Resource {
        resource: serde_json::Value,
    },
    #[serde(other)]
    Unknown,
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

/// Progress notification payload from the MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProgressNotification {
    pub progress_token: serde_json::Value,
    pub progress: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub type ProgressCallback = Arc<dyn Fn(McpProgressNotification) + Send + Sync>;
pub type NotificationListener = Arc<dyn Fn(Option<serde_json::Value>) + Send + Sync>;
pub type ErrorListener = Arc<dyn Fn(String) + Send + Sync>;
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;
pub type RequestHandler = Arc<
    dyn Fn(
            Option<serde_json::Value>,
            tokio_util::sync::CancellationToken,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<serde_json::Value>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct McpRequestOptions {
    pub timeout: Option<Duration>,
    pub max_total_timeout: Option<Duration>,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub on_progress: Option<ProgressCallback>,
}

impl McpRequestOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_max_total_timeout(mut self, max_total_timeout: Duration) -> Self {
        self.max_total_timeout = Some(max_total_timeout);
        self
    }

    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_progress(mut self, on_progress: ProgressCallback) -> Self {
        self.on_progress = Some(on_progress);
        self
    }
}

type PendingResponseSender = oneshot::Sender<Result<serde_json::Value, String>>;
type PendingRequestsMap = Arc<std::sync::Mutex<HashMap<String, PendingResponseSender>>>;
type ProgressSendersMap =
    Arc<std::sync::Mutex<HashMap<String, tokio::sync::mpsc::Sender<McpProgressNotification>>>>;
type IncomingTasksMap =
    Arc<std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>;

/// Removes an in-flight incoming server request from both tracking maps on task
/// completion or panic, so cancelled/panicking handlers never leak request IDs.
struct IncomingRequestGuard {
    incoming_map: Arc<std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    tasks_map: IncomingTasksMap,
    key: String,
}

impl Drop for IncomingRequestGuard {
    fn drop(&mut self) {
        lock_std(&self.incoming_map).remove(&self.key);
        lock_std(&self.tasks_map).remove(&self.key);
    }
}

fn lock_std<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const MAX_CONCURRENT_SERVER_REQUESTS: usize = 32;
const PROGRESS_CHANNEL_CAPACITY: usize = 64;
const DEFAULT_MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

fn enqueue_response(
    writer_tx: &tokio::sync::mpsc::Sender<JsonRpcMessage>,
    resp: JsonRpcResponse,
) {
    if let Err(err) = writer_tx.try_send(JsonRpcMessage::Response(resp)) {
        tracing::warn!("MCP writer queue full/closed; dropping response: {err}");
    }
}

/// RAII cleanup for an in-flight request. If the caller's future is dropped
/// (external timeout, `select!`, task cancellation), the pending entries are
/// still removed and a best-effort `notifications/cancelled` is emitted.
struct PendingRequestGuard {
    pending: PendingRequestsMap,
    progress: ProgressSendersMap,
    transport: Arc<dyn McpTransport>,
    key: String,
    request_id: serde_json::Value,
    notify_cancel: bool,
    active: bool,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        lock_std(&self.pending).remove(&self.key);
        lock_std(&self.progress).remove(&self.key);
        if self.notify_cancel {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let notif = JsonRpcNotification::new(
                    "notifications/cancelled",
                    Some(serde_json::json!({
                        "requestId": self.request_id,
                        "reason": "cancelled"
                    })),
                );
                let transport = self.transport.clone();
                handle.spawn(async move {
                    let _ = transport.send(JsonRpcMessage::Notification(notif)).await;
                });
            }
        }
    }
}

/// Asynchronous client for communicating with an MCP server over an `McpTransport`.
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    pending_requests: PendingRequestsMap,
    progress_senders: ProgressSendersMap,
    incoming_requests: Arc<std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    incoming_tasks: IncomingTasksMap,
    request_handlers: Arc<std::sync::Mutex<HashMap<String, RequestHandler>>>,
    notification_listeners: Arc<std::sync::Mutex<HashMap<String, Vec<NotificationListener>>>>,
    error_listeners: Arc<std::sync::Mutex<Vec<ErrorListener>>>,
    close_listeners: Arc<std::sync::Mutex<Vec<CloseListener>>>,
    is_closed: Arc<AtomicBool>,
    server_protocol_version: Arc<RwLock<Option<String>>>,
    server_info: Arc<RwLock<Option<serde_json::Value>>>,
    server_capabilities: Arc<RwLock<Option<serde_json::Value>>>,
    roots: Arc<Vec<McpRoot>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        Self::connect_with_roots_and_timeout(transport, roots, Duration::from_secs(15)).await
    }

    /// Connects to an MCP server providing client workspace roots and an explicit handshake timeout.
    pub async fn connect_with_roots_and_timeout(
        transport: Arc<dyn McpTransport>,
        roots: Vec<McpRoot>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let progress_senders = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let incoming_requests = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let incoming_tasks = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let request_handlers = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let notification_listeners = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let error_listeners = Arc::new(std::sync::Mutex::new(Vec::new()));
        let close_listeners = Arc::new(std::sync::Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let server_protocol_version = Arc::new(RwLock::new(None));
        let server_info = Arc::new(RwLock::new(None));
        let server_capabilities = Arc::new(RwLock::new(None));
        let roots_arc = Arc::new(roots);

        let client = Self {
            transport: transport.clone(),
            pending_requests: pending.clone(),
            progress_senders: progress_senders.clone(),
            incoming_requests: incoming_requests.clone(),
            incoming_tasks: incoming_tasks.clone(),
            request_handlers: request_handlers.clone(),
            notification_listeners: notification_listeners.clone(),
            error_listeners: error_listeners.clone(),
            close_listeners: close_listeners.clone(),
            is_closed: closed.clone(),
            server_protocol_version: server_protocol_version.clone(),
            server_info: server_info.clone(),
            server_capabilities: server_capabilities.clone(),
            roots: roots_arc.clone(),
            reader_task: Mutex::new(None),
            writer_task: Mutex::new(None),
        };

        // Dedicated writer task: the reader loop must never block on transport.send().
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<JsonRpcMessage>(256);
        let transport_writer = transport.clone();
        let closed_writer = closed.clone();
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = writer_rx.recv().await {
                if let Err(err) = transport_writer.send(msg).await {
                    tracing::warn!("MCP writer task failed to send message: {err}");
                    if closed_writer.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        });
        *client.writer_task.lock().await = Some(writer_handle);

        // Start background reader task
        let transport_reader = transport.clone();
        let pending_reader = pending.clone();
        let progress_reader = progress_senders.clone();
        let incoming_reader = incoming_requests.clone();
        let incoming_tasks_reader = incoming_tasks.clone();
        let handlers_reader = request_handlers.clone();
        let notif_reader = notification_listeners.clone();
        let err_reader = error_listeners.clone();
        let close_reader = close_listeners.clone();
        let closed_reader = closed.clone();
        let roots_reader = roots_arc.clone();

        let incoming_permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SERVER_REQUESTS));
        let reader_handle = tokio::spawn(async move {
            while let Some(msg_res) = transport_reader.recv().await {
                match msg_res {
                    Ok(JsonRpcMessage::Response(resp)) => {
                        let key = id_to_key(&resp.id);
                        let mut map = lock_std(&pending_reader);
                        if let Some(tx) = map.remove(&key) {
                            if let Some(err) = resp.error {
                                let msg = if let Some(data) = err.data {
                                    format!("MCP error {}: {} (data: {data})", err.code, err.message)
                                } else {
                                    format!("MCP error {}: {}", err.code, err.message)
                                };
                                let _ = tx.send(Err(msg));
                            } else {
                                let val = resp.result.unwrap_or(serde_json::Value::Null);
                                let _ = tx.send(Ok(val));
                            }
                        }
                    }
                    Ok(JsonRpcMessage::Request(req)) => {
                        let handler = {
                            let map = lock_std(&handlers_reader);
                            map.get(&req.method).cloned()
                        };
                        if let Some(h) = handler {
                            let key = id_to_key(&req.id);
                            {
                                let inc = lock_std(&incoming_reader);
                                if inc.contains_key(&key) {
                                    let resp = JsonRpcResponse::error(
                                        req.id,
                                        -32600,
                                        format!("Duplicate in-flight request id: {key}"),
                                        None,
                                    );
                                    enqueue_response(&writer_tx, resp);
                                    continue;
                                }
                            }
                            let permit = match incoming_permits.clone().try_acquire_owned() {
                                Ok(p) => p,
                                Err(_) => {
                                    let resp = JsonRpcResponse::error(
                                        req.id,
                                        -32000,
                                        "Too many concurrent server requests",
                                        None,
                                    );
                                    enqueue_response(&writer_tx, resp);
                                    continue;
                                }
                            };
                            let cancel = tokio_util::sync::CancellationToken::new();
                            {
                                let mut inc = lock_std(&incoming_reader);
                                inc.insert(key.clone(), cancel.clone());
                            }
                            let writer_for_handler = writer_tx.clone();
                            let id_clone = req.id.clone();
                            let incoming_map = incoming_reader.clone();
                            let tasks_map = incoming_tasks_reader.clone();
                            let task_key = key.clone();
                            let mut tasks_guard = lock_std(&incoming_tasks_reader);
                            tasks_guard.retain(|_, h| !h.is_finished());
                            let handle = tokio::spawn(async move {
                                let _permit = permit;
                                let guard = IncomingRequestGuard {
                                    incoming_map,
                                    tasks_map,
                                    key: task_key,
                                };
                                let res = h(req.params, cancel).await;
                                drop(guard);
                                match res {
                                    Ok(val) => {
                                        let resp = JsonRpcResponse::success(id_clone, val);
                                        let _ = writer_for_handler.send(JsonRpcMessage::Response(resp)).await;
                                    }
                                    Err(e) => {
                                        let resp = JsonRpcResponse::error(id_clone, -32603, e.to_string(), None);
                                        let _ = writer_for_handler.send(JsonRpcMessage::Response(resp)).await;
                                    }
                                }
                            });
                            tasks_guard.insert(key, handle);
                        } else if req.method == "ping" {
                            let resp = JsonRpcResponse::success(req.id, serde_json::json!({}));
                            enqueue_response(&writer_tx, resp);
                        } else if req.method == "roots/list" {
                            let resp = JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({ "roots": *roots_reader }),
                            );
                            enqueue_response(&writer_tx, resp);
                        } else {
                            let resp = JsonRpcResponse::error(
                                req.id,
                                -32601,
                                format!("Method not found: {}", req.method),
                                None,
                            );
                            enqueue_response(&writer_tx, resp);
                        }
                    }
                    Ok(JsonRpcMessage::Notification(notif)) => {
                        if notif.method == "notifications/progress" {
                            if let Some(params) = &notif.params {
                                if let Ok(prog) = serde_json::from_value::<McpProgressNotification>(params.clone()) {
                                    let key = id_to_key(&prog.progress_token);
                                    let tx = {
                                        let map = lock_std(&progress_reader);
                                        map.get(&key).cloned()
                                    };
                                    if let Some(tx) = tx {
                                        let _ = tx.try_send(prog);
                                    }
                                }
                            }
                        } else if notif.method == "notifications/cancelled" {
                            if let Some(params) = &notif.params {
                                if let Some(req_id) = params.get("requestId") {
                                    let key = id_to_key(req_id);
                                    let cancel_token = {
                                        let inc = lock_std(&incoming_reader);
                                        inc.get(&key).cloned()
                                    };
                                    if let Some(cancel) = cancel_token {
                                        cancel.cancel();
                                    }
                                }
                            }
                        }

                        let listeners = {
                            let map = lock_std(&notif_reader);
                            map.get(&notif.method).cloned().unwrap_or_default()
                        };
                        for listener in listeners {
                            listener(notif.params.clone());
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("MCP transport error: {e}");
                        tracing::warn!("{err_msg}");
                        let err_listeners = {
                            let list = lock_std(&err_reader);
                            list.clone()
                        };
                        for listener in err_listeners {
                            listener(err_msg.clone());
                        }
                    }
                }
            }
            let was_closed = closed_reader.swap(true, Ordering::SeqCst);
            let final_reason = "MCP connection closed".to_string();
            let mut map = lock_std(&pending_reader);
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(final_reason.clone()));
            }
            for (_, token) in lock_std(&incoming_reader).drain() {
                token.cancel();
            }
            if !was_closed {
                let close_listeners = {
                    let list = lock_std(&close_reader);
                    list.clone()
                };
                for listener in close_listeners {
                    listener();
                }
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
                "protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0],
                "capabilities": capabilities,
                "clientInfo": {
                    "name": "mehh",
                    "version": "0.1.0"
                }
            });

            let init_res = client
                .request_with_options(
                    "initialize",
                    Some(init_params),
                    McpRequestOptions {
                        timeout: Some(timeout),
                        ..Default::default()
                    },
                )
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

            // Inform the transport so subsequent HTTP requests carry MCP-Protocol-Version.
            transport.on_protocol_negotiated(&negotiated_version).await;
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
                timeout,
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

    /// Registers a listener for notifications of a specific method.
    pub fn on_notification(&self, method: &str, listener: NotificationListener) {
        let mut map = lock_std(&self.notification_listeners);
        map.entry(method.to_string()).or_default().push(listener);
    }

    /// Registers a listener for transport or protocol errors.
    pub fn on_error(&self, listener: ErrorListener) {
        let mut list = lock_std(&self.error_listeners);
        list.push(listener);
    }

    /// Registers a listener invoked when the connection closes.
    pub fn on_close(&self, listener: CloseListener) {
        let mut list = lock_std(&self.close_listeners);
        list.push(listener);
    }

    /// Registers a custom request handler for server-to-client requests.
    pub fn set_request_handler(&self, method: &str, handler: RequestHandler) {
        let mut map = lock_std(&self.request_handlers);
        map.insert(method.to_string(), handler);
    }

    /// Dispatches a JSON-RPC request to the server with options (timeout, cancellation, progress callback).
    pub async fn request_with_options(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        options: McpRequestOptions,
    ) -> anyhow::Result<serde_json::Value> {
        let id = create_request_id();
        let key = id_to_key(&id);
        let (tx, mut rx) = oneshot::channel();
        let mut guard = PendingRequestGuard {
            pending: self.pending_requests.clone(),
            progress: self.progress_senders.clone(),
            transport: self.transport.clone(),
            key: key.clone(),
            request_id: id.clone(),
            notify_cancel: false,
            active: true,
        };
        {
            // Check closed state and register atomically under the same lock as close().
            let mut map = lock_std(&self.pending_requests);
            if self.is_closed.load(Ordering::SeqCst) {
                anyhow::bail!("MCP connection is closed");
            }
            map.insert(key.clone(), tx);
        }

        let mut progress_rx = options.on_progress.as_ref().map(|_| {
            let (progress_tx, rx) = tokio::sync::mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            let mut map = lock_std(&self.progress_senders);
            map.insert(key.clone(), progress_tx);
            rx
        });

        let mut request_params = params;
        if options.on_progress.is_some() {
            let obj = match request_params {
                Some(serde_json::Value::Object(mut map)) => {
                    let mut meta = match map.remove("_meta") {
                        Some(serde_json::Value::Object(m)) => m,
                        _ => serde_json::Map::new(),
                    };
                    meta.insert("progressToken".to_string(), id.clone());
                    map.insert("_meta".to_string(), serde_json::Value::Object(meta));
                    serde_json::Value::Object(map)
                }
                _ => serde_json::json!({
                    "_meta": {
                        "progressToken": id.clone()
                    }
                }),
            };
            request_params = Some(obj);
        }

        let timeout_dur = options.timeout.unwrap_or(Duration::from_secs(30));
        let max_total_dur = options
            .max_total_timeout
            .unwrap_or_else(|| DEFAULT_MAX_TOTAL_TIMEOUT.max(timeout_dur));
        let req = JsonRpcRequest::new(id.clone(), method, request_params);

        let cancel = options.cancel.clone().unwrap_or_default();
        let start = tokio::time::Instant::now();
        let total_deadline = start + max_total_dur;
        let deadline = (start + timeout_dur).min(total_deadline);
        let deadline_sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline_sleep);

        let send_fut = self.transport.send(JsonRpcMessage::Request(req));
        tokio::pin!(send_fut);
        let mut sent = false;
        let mut is_timeout = false;
        let mut is_cancelled = false;

        let res = loop {
            if !sent {
                tokio::select! {
                    send_res = &mut send_fut => {
                        match send_res {
                            Ok(()) => {
                                sent = true;
                                guard.notify_cancel = !matches!(method, "initialize");
                            }
                            Err(e) => {
                                break Err(anyhow::anyhow!("Failed to send MCP request: {e}"));
                            }
                        }
                    }
                    _ = &mut deadline_sleep => {
                        is_timeout = true;
                        break Err(anyhow::anyhow!("MCP request timed out after {}ms", timeout_dur.as_millis()));
                    }
                    _ = cancel.cancelled() => {
                        is_cancelled = true;
                        break Err(anyhow::anyhow!("MCP request cancelled"));
                    }
                }
            } else {
                tokio::select! {
                    response = &mut rx => {
                        if let (Some(cb), Some(ref mut prx)) = (&options.on_progress, progress_rx.as_mut()) {
                            while let Ok(prog) = prx.try_recv() {
                                cb(prog);
                            }
                        }
                        break match response {
                            Ok(Ok(val)) => Ok(val),
                            Ok(Err(err_msg)) => Err(anyhow::anyhow!("{err_msg}")),
                            Err(_) => Err(anyhow::anyhow!("MCP request sender dropped / connection closed")),
                        };
                    }
                    _ = &mut deadline_sleep => {
                        is_timeout = true;
                        let msg = if tokio::time::Instant::now() >= total_deadline {
                            format!(
                                "MCP request timed out after max total duration of {}ms",
                                max_total_dur.as_millis()
                            )
                        } else {
                            format!("MCP request timed out after {}ms", timeout_dur.as_millis())
                        };
                        break Err(anyhow::anyhow!("{msg}"));
                    }
                    _ = cancel.cancelled() => {
                        is_cancelled = true;
                        break Err(anyhow::anyhow!("MCP request cancelled"));
                    }
                    Some(prog) = async {
                        match progress_rx.as_mut() {
                            Some(prx) => prx.recv().await,
                            None => std::future::pending().await,
                        }
                    }, if progress_rx.is_some() => {
                        let now = tokio::time::Instant::now();
                        if now >= total_deadline {
                            is_timeout = true;
                            break Err(anyhow::anyhow!(
                                "MCP request timed out after max total duration of {}ms",
                                max_total_dur.as_millis()
                            ));
                        }
                        let next_deadline = (now + timeout_dur).min(total_deadline);
                        deadline_sleep.as_mut().reset(next_deadline);
                        if let Some(cb) = &options.on_progress {
                            cb(prog);
                        }
                    }
                }
            }
        };

        {
            lock_std(&self.pending_requests).remove(&key);
            if options.on_progress.is_some() {
                lock_std(&self.progress_senders).remove(&key);
            }
        }
        guard.active = false;

        match res {
            Ok(val) => Ok(val),
            Err(err) => {
                if sent && method != "initialize" && (is_cancelled || is_timeout) {
                    let reason = if is_cancelled {
                        "cancelled".to_string()
                    } else {
                        format!("timeout after {}ms", timeout_dur.as_millis())
                    };
                    let notif = JsonRpcNotification::new(
                        "notifications/cancelled",
                        Some(serde_json::json!({
                            "requestId": id,
                            "reason": reason,
                        })),
                    );
                    let transport = self.transport.clone();
                    tokio::spawn(async move {
                        let _ = transport.send(JsonRpcMessage::Notification(notif)).await;
                    });
                }
                Err(err)
            }
        }
    }

    /// Dispatches a JSON-RPC request to the server and awaits the response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_with_options(
            method,
            params,
            McpRequestOptions {
                timeout,
                ..Default::default()
            },
        )
        .await
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
        if cursor.is_some() {
            anyhow::bail!("MCP tools/list exceeded 1000 pages");
        }
        Ok(all_tools)
    }

    /// Calls a specific tool by name with arguments and options (timeout, cancel token, progress callback).
    pub async fn call_tool_with_options(
        &self,
        name: &str,
        arguments: serde_json::Value,
        options: McpRequestOptions,
    ) -> anyhow::Result<McpCallToolResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });
        let res = self.request_with_options("tools/call", Some(params), options).await?;
        let call_res: McpCallToolResult = serde_json::from_value(res)?;
        Ok(call_res)
    }

    /// Calls a specific tool by name with arguments and optional timeout.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Option<Duration>,
    ) -> anyhow::Result<McpCallToolResult> {
        self.call_tool_with_options(
            name,
            arguments,
            McpRequestOptions {
                timeout,
                ..Default::default()
            },
        )
        .await
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
        if cursor.is_some() {
            anyhow::bail!("MCP resources/list exceeded 1000 pages");
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
        if cursor.is_some() {
            anyhow::bail!("MCP resources/templates/list exceeded 1000 pages");
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
        // Set the closed flag and drain pending requests under the same lock used
        // by request_with_options, so no request can register after the drain.
        let was_closed = {
            let mut map = lock_std(&self.pending_requests);
            let was = self.is_closed.swap(true, Ordering::SeqCst);
            for (_, tx) in map.drain() {
                let _ = tx.send(Err("MCP connection closed".to_string()));
            }
            was
        };
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        // Cancel and drain all active incoming server-to-client request handlers.
        for (_, token) in lock_std(&self.incoming_requests).drain() {
            token.cancel();
        }
        let mut handler_handles: Vec<tokio::task::JoinHandle<()>> = lock_std(&self.incoming_tasks)
            .drain()
            .map(|(_, h)| h)
            .collect();
        if !handler_handles.is_empty() {
            let drain_fut = async {
                for handle in &mut handler_handles {
                    let _ = handle.await;
                }
            };
            if tokio::time::timeout(Duration::from_millis(500), drain_fut)
                .await
                .is_err()
            {
                for handle in handler_handles {
                    handle.abort();
                }
            }
        }
        if let Some(handle) = self.writer_task.lock().await.take() {
            handle.abort();
        }
        if !was_closed {
            let listeners = {
                let list = lock_std(&self.close_listeners);
                list.clone()
            };
            for listener in listeners {
                listener();
            }
        }
        self.transport.close().await
    }

    /// Returns a copy of the active incoming request cancellation tokens.
    pub fn incoming_cancellation_tokens(&self) -> HashMap<String, tokio_util::sync::CancellationToken> {
        let map = lock_std(&self.incoming_requests);
        map.clone()
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

impl Drop for McpClient {
    fn drop(&mut self) {
        // Stop background tasks even when callers forget close(). Aborting the
        // writer task drops its transport Arc, which terminates stdio children.
        let was_closed = self.is_closed.swap(true, Ordering::SeqCst);
        for (_, token) in lock_std(&self.incoming_requests).drain() {
            token.cancel();
        }
        for (_, handle) in lock_std(&self.incoming_tasks).drain() {
            handle.abort();
        }
        if let Ok(mut guard) = self.reader_task.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.writer_task.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        // Aborting the tasks alone leaves HTTP SSE tasks and the server-side
        // MCP session alive. Best-effort close on the runtime if one is still
        // available; skip it otherwise so Drop never panics at shutdown.
        if !was_closed {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let transport = self.transport.clone();
                runtime.spawn(async move {
                    let _ = transport.close().await;
                });
            }
        }
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
            pending_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            progress_senders: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            request_handlers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            notification_listeners: Arc::new(std::sync::Mutex::new(HashMap::new())),
            error_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            close_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            is_closed: Arc::new(AtomicBool::new(false)),
            server_protocol_version: Arc::new(RwLock::new(None)),
            server_info: Arc::new(RwLock::new(None)),
            server_capabilities: Arc::new(RwLock::new(None)),
            roots: Arc::new(Vec::new()),
            reader_task: Mutex::new(None),
            writer_task: Mutex::new(None),
        };

        let start = std::time::Instant::now();
        let err = client
            .request("tools/list", None, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
        assert!(lock_std(&client.pending_requests).is_empty());
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

    #[tokio::test]
    async fn cancelled_request_sends_notifications_cancelled() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let server_task = tokio::spawn({
            let server_transport = server_transport.clone();
            async move {
                // Handshake: receive initialize, send response
                if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                    assert_eq!(req.method, "initialize");
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
                // Receive notifications/initialized
                let _ = server_transport.recv().await;

                // Receive tool call
                let mut cancelled_received = false;
                while let Some(Ok(msg)) = server_transport.recv().await {
                    if let JsonRpcMessage::Notification(notif) = msg {
                        if notif.method == "notifications/cancelled" {
                            let params = notif.params.expect("params on cancelled");
                            assert!(params.get("requestId").is_some());
                            cancelled_received = true;
                            break;
                        }
                    }
                }
                assert!(cancelled_received);
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_clone.cancel();
        });

        let res = client
            .call_tool_with_options(
                "slow_tool",
                serde_json::json!({}),
                McpRequestOptions::new().with_cancel(cancel),
            )
            .await;

        assert!(res.is_err());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_on_initialize_does_not_send_cancelled_notification() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let server_task = tokio::spawn({
            let server_transport = server_transport.clone();
            async move {
                // Server does not respond to initialize
                let _ = server_transport.recv().await;
                // Wait to ensure no notifications/cancelled is sent
                let timeout_res = tokio::time::timeout(Duration::from_millis(150), server_transport.recv()).await;
                if let Ok(Some(Ok(JsonRpcMessage::Notification(notif)))) = timeout_res {
                    assert_ne!(notif.method, "notifications/cancelled");
                }
            }
        });

        let client = McpClient {
            transport: client_transport.clone(),
            pending_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            progress_senders: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            request_handlers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            notification_listeners: Arc::new(std::sync::Mutex::new(HashMap::new())),
            error_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            close_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            is_closed: Arc::new(AtomicBool::new(false)),
            server_protocol_version: Arc::new(RwLock::new(None)),
            server_info: Arc::new(RwLock::new(None)),
            server_capabilities: Arc::new(RwLock::new(None)),
            roots: Arc::new(Vec::new()),
            reader_task: Mutex::new(None),
            writer_task: Mutex::new(None),
        };

        let err = client
            .request_with_options(
                "initialize",
                Some(serde_json::json!({})),
                McpRequestOptions::new().with_timeout(Duration::from_millis(50)),
            )
            .await;
        assert!(err.is_err());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn incoming_notifications_cancelled_triggers_server_request_token() {
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

                // Send request from server to client
                let req_msg = JsonRpcMessage::Request(JsonRpcRequest::new(
                    serde_json::json!(42),
                    "custom/long_op",
                    None,
                ));
                server_transport.send(req_msg).await.unwrap();

                tokio::time::sleep(Duration::from_millis(20)).await;

                // Send cancellation
                let cancel_msg = JsonRpcMessage::Notification(JsonRpcNotification::new(
                    "notifications/cancelled",
                    Some(serde_json::json!({ "requestId": 42 })),
                ));
                server_transport.send(cancel_msg).await.unwrap();
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let cancelled_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = cancelled_flag.clone();

        client.set_request_handler(
            "custom/long_op",
            Arc::new(move |_params, token| {
                let flag = flag_clone.clone();
                Box::pin(async move {
                    token.cancelled().await;
                    flag.store(true, Ordering::SeqCst);
                    Ok(serde_json::json!({ "status": "aborted" }))
                })
            }),
        );

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cancelled_flag.load(Ordering::SeqCst));
        let _ = client.close().await;
    }

    #[tokio::test]
    async fn progress_notification_renews_timeout_and_invokes_callback() {
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

                // Receive tool call
                if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                    let token = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("_meta"))
                        .and_then(|m| m.get("progressToken"))
                        .cloned()
                        .expect("progressToken present");

                    // Progress 1 at 100ms
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let p1 = JsonRpcNotification::new(
                        "notifications/progress",
                        Some(serde_json::json!({
                            "progressToken": token,
                            "progress": 1.0,
                            "total": 2.0,
                            "message": "halfway"
                        })),
                    );
                    server_transport.send(JsonRpcMessage::Notification(p1)).await.unwrap();

                    // Progress 2 at 200ms (100ms later)
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let p2 = JsonRpcNotification::new(
                        "notifications/progress",
                        Some(serde_json::json!({
                            "progressToken": token,
                            "progress": 2.0,
                            "total": 2.0,
                            "message": "done"
                        })),
                    );
                    server_transport.send(JsonRpcMessage::Notification(p2)).await.unwrap();

                    // Complete at 300ms (100ms later)
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let resp = JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": "success" }]
                        }),
                    );
                    server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
                }
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let progress_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = progress_count.clone();

        let progress_cb = Arc::new(move |p: McpProgressNotification| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            assert_eq!(p.total, Some(2.0));
        });

        // Timeout is 250ms, but total duration is 300ms. Without renewal it would fail!
        let start = std::time::Instant::now();
        let res = client
            .call_tool_with_options(
                "long_tool",
                serde_json::json!({}),
                McpRequestOptions::new()
                    .with_timeout(Duration::from_millis(250))
                    .with_progress(progress_cb),
            )
            .await
            .expect("should succeed because timeout is renewed");

        assert!(start.elapsed() >= Duration::from_millis(280));
        assert_eq!(progress_count.load(Ordering::SeqCst), 2);
        assert_eq!(res.content.len(), 1);
        let _ = client.close().await;
    }

    #[tokio::test]
    async fn dynamic_notification_and_request_handler_registries_fire() {
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

                // Send notification
                server_transport
                    .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                        "notifications/custom_event",
                        Some(serde_json::json!({ "key": "value" })),
                    )))
                    .await
                    .unwrap();

                // Send request
                server_transport
                    .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                        serde_json::json!(99),
                        "custom/add",
                        Some(serde_json::json!({ "a": 5, "b": 10 })),
                    )))
                    .await
                    .unwrap();

                // Receive response to custom/add
                if let Some(Ok(JsonRpcMessage::Response(resp))) = server_transport.recv().await {
                    assert_eq!(resp.result, Some(serde_json::json!({ "sum": 15 })));
                }
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();

        let notif_received = Arc::new(AtomicBool::new(false));
        let notif_clone = notif_received.clone();
        client.on_notification(
            "notifications/custom_event",
            Arc::new(move |params| {
                if let Some(p) = params {
                    if p.get("key") == Some(&serde_json::json!("value")) {
                        notif_clone.store(true, Ordering::SeqCst);
                    }
                }
            }),
        );

        client.set_request_handler(
            "custom/add",
            Arc::new(|params, _cancel| {
                Box::pin(async move {
                    let p = params.unwrap_or_default();
                    let a = p.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                    let b = p.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                    Ok(serde_json::json!({ "sum": a + b }))
                })
            }),
        );

        let closed_flag = Arc::new(AtomicBool::new(false));
        let closed_clone = closed_flag.clone();
        client.on_close(Arc::new(move || {
            closed_clone.store(true, Ordering::SeqCst);
        }));

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(notif_received.load(Ordering::SeqCst));

        client.close().await.unwrap();
        assert!(closed_flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_server_error_does_not_send_cancelled_notification() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let server_task = tokio::spawn(async move {
            // Handshake
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                assert_eq!(req.method, "initialize");
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": { "name": "err_server", "version": "1.0" }
                    }),
                );
                server_transport
                    .send(JsonRpcMessage::Response(resp))
                    .await
                    .unwrap();
            }
            if let Some(Ok(JsonRpcMessage::Notification(notif))) = server_transport.recv().await {
                assert_eq!(notif.method, "notifications/initialized");
            }

            // Next request: respond with JSON-RPC error
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                assert_eq!(req.method, "tools/call");
                let err_resp = JsonRpcResponse::error(
                    req.id,
                    -32602,
                    "Invalid tool params",
                    None,
                );
                server_transport
                    .send(JsonRpcMessage::Response(err_resp))
                    .await
                    .unwrap();
            }

            // Verify no notifications/cancelled arrives
            let mut got_cancelled = false;
            let check_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
            while tokio::time::Instant::now() < check_deadline {
                if let Ok(Some(Ok(JsonRpcMessage::Notification(n)))) =
                    tokio::time::timeout(Duration::from_millis(20), server_transport.recv()).await
                {
                    if n.method == "notifications/cancelled" {
                        got_cancelled = true;
                        break;
                    }
                }
            }
            assert!(!got_cancelled, "Must not send notifications/cancelled on server error");
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let res = client.call_tool("calc", serde_json::json!({}), None).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Invalid tool params"));

        server_task.await.unwrap();
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_transport_error_does_not_fail_pending_requests() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let server_task = tokio::spawn(async move {
            // Handshake
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                assert_eq!(req.method, "initialize");
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "serverInfo": { "name": "test-srv", "version": "1.0" },
                        "capabilities": {}
                    }),
                );
                server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            if let Some(Ok(JsonRpcMessage::Notification(n))) = server_transport.recv().await {
                assert_eq!(n.method, "notifications/initialized");
            }

            // Receive call_tool request
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                assert_eq!(req.method, "tools/call");
                // Delay response slightly
                tokio::time::sleep(Duration::from_millis(50)).await;
                let ok_resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": "success_after_stray_error" }]
                    }),
                );
                server_transport.send(JsonRpcMessage::Response(ok_resp)).await.unwrap();
            }
        });

        let client = McpClient::connect(client_transport.clone()).await.unwrap();

        let client_ref = Arc::new(client);
        let client_clone = client_ref.clone();

        let call_handle = tokio::spawn(async move {
            client_clone.call_tool("delayed_op", serde_json::json!({}), None).await
        });

        // Wait for request to be in flight
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Transport error arrives (e.g. invalid framing or stray warning from server)
        // Simulate by sending an error through the transport's channel / error listener
        // Note: For in-memory transport or general transports, a non-fatal error must not abort pending requests.
        // We trigger error listener directly or simulate a transport error.
        let reported_errors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let errs_clone = reported_errors.clone();
        client_ref.on_error(Arc::new(move |err| {
            errs_clone.lock().unwrap().push(err);
        }));

        // Trigger error handler
        {
            let listeners = client_ref.error_listeners.lock().unwrap().clone();
            for l in listeners {
                l("Simulated stray transport warning".to_string());
            }
        }

        let res = call_handle.await.unwrap();
        assert!(res.is_ok(), "Pending request must succeed despite transport errors: {:?}", res.err());
        let val = res.unwrap();
        assert_eq!(val.content.len(), 1);

        server_task.await.unwrap();
        client_ref.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_mcp_error_includes_data_payload() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let server_task = tokio::spawn(async move {
            // Handshake
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "serverInfo": { "name": "test-srv", "version": "1.0" },
                        "capabilities": {}
                    }),
                );
                server_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            let _ = server_transport.recv().await; // initialized

            // Request with error and data
            if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                let err_resp = JsonRpcResponse::error(
                    req.id,
                    -32001,
                    "Access denied",
                    Some(serde_json::json!({ "reason": "token_expired_123" })),
                );
                server_transport.send(JsonRpcMessage::Response(err_resp)).await.unwrap();
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let res = client.call_tool("check_auth", serde_json::json!({}), None).await;
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("Access denied"));
        assert!(err_str.contains("token_expired_123"), "Error must include data payload: {err_str}");

        server_task.await.unwrap();
        client.close().await.unwrap();
    }

    #[test]
    fn test_content_block_accepts_spec_mime_type() {
        let image: McpContentBlock = serde_json::from_value(serde_json::json!({
            "type": "image",
            "data": "AQID",
            "mimeType": "image/png"
        }))
        .expect("spec-compliant image block must deserialize");
        match image {
            McpContentBlock::Image { mime_type, .. } => assert_eq!(mime_type, "image/png"),
            _ => panic!("expected image block"),
        }

        let link: McpContentBlock = serde_json::from_value(serde_json::json!({
            "type": "resource_link",
            "uri": "file:///docs/readme.md",
            "name": "readme",
            "mimeType": "text/markdown"
        }))
        .unwrap();
        match link {
            McpContentBlock::ResourceLink { mime_type, .. } => {
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
            }
            _ => panic!("expected resource link block"),
        }
    }

    #[tokio::test]
    async fn test_dropped_request_future_cleans_pending_entries() {
        let client = McpClient {
            transport: Arc::new(SlowSendTransport {
                closed: Arc::new(AtomicBool::new(false)),
            }),
            pending_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            progress_senders: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
            incoming_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            request_handlers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            notification_listeners: Arc::new(std::sync::Mutex::new(HashMap::new())),
            error_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            close_listeners: Arc::new(std::sync::Mutex::new(Vec::new())),
            is_closed: Arc::new(AtomicBool::new(false)),
            server_protocol_version: Arc::new(RwLock::new(None)),
            server_info: Arc::new(RwLock::new(None)),
            server_capabilities: Arc::new(RwLock::new(None)),
            roots: Arc::new(Vec::new()),
            reader_task: Mutex::new(None),
            writer_task: Mutex::new(None),
        };
        let pending = client.pending_requests.clone();
        let progress = client.progress_senders.clone();
        {
            let fut = client.request_with_options(
                "tools/list",
                None,
                McpRequestOptions::new().with_progress(Arc::new(|_| {})),
            );
            let _ = tokio::time::timeout(Duration::from_millis(20), fut).await;
        }
        assert!(lock_std(&pending).is_empty(), "pending entry must be removed on future drop");
        assert!(lock_std(&progress).is_empty(), "progress entry must be removed on future drop");
    }

    #[tokio::test]
    async fn max_total_timeout_terminates_despite_continuous_progress() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let srv = tokio::spawn(async move {
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
                let token = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("_meta"))
                    .and_then(|m| m.get("progressToken"))
                    .cloned()
                    .expect("progressToken");

                // Continuously emit progress every 20ms, never sending a final response.
                for i in 1..=25 {
                    let p = JsonRpcNotification::new(
                        "notifications/progress",
                        Some(serde_json::json!({
                            "progressToken": token,
                            "progress": i as f64,
                            "total": 100.0
                        })),
                    );
                    if server_transport.send(JsonRpcMessage::Notification(p)).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let start = std::time::Instant::now();
        let err = client
            .call_tool_with_options(
                "endless_tool",
                serde_json::json!({}),
                McpRequestOptions::new()
                    .with_timeout(Duration::from_millis(120))
                    .with_max_total_timeout(Duration::from_millis(180))
                    .with_progress(Arc::new(|_| {})),
            )
            .await
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "request must terminate near max_total_timeout"
        );
        assert!(
            err.to_string().contains("max total duration"),
            "expected max total duration timeout, got: {err}"
        );
        client.close().await.unwrap();
        srv.abort();
    }

    #[tokio::test]
    async fn close_cancels_and_drains_incoming_handlers_and_drop_is_idempotent() {
        use std::sync::atomic::AtomicUsize;

        struct CountCloseTransport {
            inner: crate::transport::InMemoryTransport,
            close_calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl McpTransport for CountCloseTransport {
            async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
                self.inner.send(msg).await
            }
            async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
                self.inner.recv().await
            }
            async fn close(&self) -> anyhow::Result<()> {
                self.close_calls.fetch_add(1, Ordering::SeqCst);
                self.inner.close().await
            }
        }

        let (client_inner, server_transport) = crate::transport::InMemoryTransport::pair();
        let close_calls = Arc::new(AtomicUsize::new(0));
        let client_transport = Arc::new(CountCloseTransport {
            inner: client_inner,
            close_calls: close_calls.clone(),
        });

        let srv = tokio::spawn(async move {
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

            server_transport
                .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                    serde_json::json!(77),
                    "custom/wait_shutdown",
                    None,
                )))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        let handler_cancelled = Arc::new(AtomicBool::new(false));
        let handler_finished = Arc::new(AtomicBool::new(false));
        let hc = handler_cancelled.clone();
        let hf = handler_finished.clone();
        client.set_request_handler(
            "custom/wait_shutdown",
            Arc::new(move |_params, cancel| {
                let hc = hc.clone();
                let hf = hf.clone();
                Box::pin(async move {
                    cancel.cancelled().await;
                    hc.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    hf.store(true, Ordering::SeqCst);
                    Ok(serde_json::json!({}))
                })
            }),
        );

        // Wait for handler to start and register in incoming_requests.
        for _ in 0..50 {
            if !client.incoming_cancellation_tokens().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!client.incoming_cancellation_tokens().is_empty());

        client.close().await.unwrap();
        assert!(handler_cancelled.load(Ordering::SeqCst), "close() must cancel incoming handlers");
        assert!(handler_finished.load(Ordering::SeqCst), "close() must drain incoming handlers");
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);

        drop(client);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            close_calls.load(Ordering::SeqCst),
            1,
            "Drop after close() must be idempotent and not call transport.close() again"
        );
        srv.abort();
    }

    #[tokio::test]
    async fn duplicate_inflight_server_request_id_is_rejected_without_orphaning_first() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);
        let srv_transport = server_transport.clone();

        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
        let first_started = Arc::new(Mutex::new(Some(first_started_tx)));
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel::<()>();
        let release_first = Arc::new(tokio::sync::Mutex::new(Some(release_first_rx)));

        let srv = tokio::spawn(async move {
            if let Some(Ok(JsonRpcMessage::Request(req))) = srv_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                srv_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            let _ = srv_transport.recv().await; // notifications/initialized
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        srv.await.unwrap();

        let started_cb = first_started.clone();
        let release_cb = release_first.clone();
        client.set_request_handler(
            "custom/slow",
            Arc::new(move |_params, _cancel| {
                let started_cb = started_cb.clone();
                let release_cb = release_cb.clone();
                Box::pin(async move {
                    if let Some(tx) = started_cb.lock().await.take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_cb.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok(serde_json::json!({ "from": "first" }))
                })
            }),
        );

        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(99),
                "custom/slow",
                None,
            )))
            .await
            .unwrap();
        first_started_rx.await.unwrap();

        // Send a duplicate request with the exact same in-flight ID.
        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(99),
                "custom/slow",
                None,
            )))
            .await
            .unwrap();

        let dup_reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected immediate -32600 error for duplicate ID")
            .unwrap()
            .unwrap();
        match dup_reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(99));
                let err = r.error.expect("duplicate request must return error");
                assert_eq!(err.code, -32600);
            }
            other => panic!("expected Response, got {other:?}"),
        }

        // First handler must still be alive and complete cleanly.
        let _ = release_first_tx.send(());
        let first_reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected first handler response")
            .unwrap()
            .unwrap();
        match first_reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(99));
                assert_eq!(r.result, Some(serde_json::json!({ "from": "first" })));
            }
            other => panic!("expected Response, got {other:?}"),
        }

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_inflight_server_request_id_cannot_be_reused_until_handler_exits() {
        use std::sync::atomic::AtomicUsize;

        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);
        let srv_transport = server_transport.clone();

        let srv = tokio::spawn(async move {
            if let Some(Ok(JsonRpcMessage::Request(req))) = srv_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                srv_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            let _ = srv_transport.recv().await; // notifications/initialized
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        srv.await.unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
        let first_started = Arc::new(Mutex::new(Some(first_started_tx)));
        let (first_cancelled_tx, first_cancelled_rx) = tokio::sync::oneshot::channel::<()>();
        let first_cancelled = Arc::new(Mutex::new(Some(first_cancelled_tx)));
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel::<()>();
        let release_first = Arc::new(Mutex::new(Some(release_first_rx)));
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel::<()>();
        let second_started = Arc::new(Mutex::new(Some(second_started_tx)));
        let second_cancelled = Arc::new(AtomicBool::new(false));
        let second_cancelled_cb = second_cancelled.clone();

        client.set_request_handler(
            "custom/cancellable",
            Arc::new(move |_params, cancel| {
                let call_count = call_count.clone();
                let first_started = first_started.clone();
                let first_cancelled = first_cancelled.clone();
                let release_first = release_first.clone();
                let second_started = second_started.clone();
                let second_cancelled_cb = second_cancelled_cb.clone();
                Box::pin(async move {
                    if call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                        if let Some(tx) = first_started.lock().await.take() {
                            let _ = tx.send(());
                        }
                        cancel.cancelled().await;
                        if let Some(tx) = first_cancelled.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release_first.lock().await.take() {
                            let _ = rx.await;
                        }
                        Ok(serde_json::json!({ "call": 1 }))
                    } else {
                        if let Some(tx) = second_started.lock().await.take() {
                            let _ = tx.send(());
                        }
                        cancel.cancelled().await;
                        second_cancelled_cb.store(true, Ordering::SeqCst);
                        Ok(serde_json::json!({ "call": 2 }))
                    }
                })
            }),
        );

        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(55),
                "custom/cancellable",
                None,
            )))
            .await
            .unwrap();
        first_started_rx.await.unwrap();

        // Cancel handler 1 while it is still running.
        server_transport
            .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                "notifications/cancelled",
                Some(serde_json::json!({ "requestId": 55 })),
            )))
            .await
            .unwrap();
        first_cancelled_rx.await.unwrap();

        // Reusing request ID 55 while handler 1 has not exited yet must be rejected.
        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(55),
                "custom/cancellable",
                None,
            )))
            .await
            .unwrap();
        let dup_reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected immediate -32600 for cancelled-but-still-running ID")
            .unwrap()
            .unwrap();
        match dup_reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(55));
                assert_eq!(r.error.expect("duplicate error").code, -32600);
            }
            other => panic!("expected Response, got {other:?}"),
        }

        // Let handler 1 finish and clean up.
        let _ = release_first_tx.send(());
        let first_reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected handler 1 response")
            .unwrap()
            .unwrap();
        match first_reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(55));
                assert_eq!(r.result, Some(serde_json::json!({ "call": 1 })));
            }
            other => panic!("expected Response, got {other:?}"),
        }

        for _ in 0..50 {
            if client.incoming_cancellation_tokens().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(client.incoming_cancellation_tokens().is_empty());

        // Now ID 55 can be reused for handler 2 with intact cancellation/cleanup.
        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(55),
                "custom/cancellable",
                None,
            )))
            .await
            .unwrap();
        second_started_rx.await.unwrap();
        assert!(!client.incoming_cancellation_tokens().is_empty());

        server_transport
            .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                "notifications/cancelled",
                Some(serde_json::json!({ "requestId": 55 })),
            )))
            .await
            .unwrap();
        let second_reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected handler 2 response")
            .unwrap()
            .unwrap();
        match second_reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(55));
                assert_eq!(r.result, Some(serde_json::json!({ "call": 2 })));
            }
            other => panic!("expected Response, got {other:?}"),
        }
        assert!(second_cancelled.load(Ordering::SeqCst));
        assert!(client.incoming_cancellation_tokens().is_empty());

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn panicking_server_request_handler_cleans_up_request_id() {
        let (client_transport, server_transport) = crate::transport::InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);
        let srv_transport = server_transport.clone();

        let srv = tokio::spawn(async move {
            if let Some(Ok(JsonRpcMessage::Request(req))) = srv_transport.recv().await {
                let resp = JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "name": "test-server", "version": "1.0.0" }
                    }),
                );
                srv_transport.send(JsonRpcMessage::Response(resp)).await.unwrap();
            }
            let _ = srv_transport.recv().await; // notifications/initialized
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        srv.await.unwrap();

        let should_panic = Arc::new(AtomicBool::new(true));
        let panic_entered = Arc::new(tokio::sync::Notify::new());
        let panic_entered_cb = panic_entered.clone();
        client.set_request_handler(
            "custom/panic_once",
            Arc::new(move |_params, _cancel| {
                let should_panic = should_panic.clone();
                let panic_entered_cb = panic_entered_cb.clone();
                Box::pin(async move {
                    if should_panic.swap(false, Ordering::SeqCst) {
                        panic_entered_cb.notify_one();
                        panic!("intentional handler panic");
                    }
                    Ok(serde_json::json!({ "recovered": true }))
                })
            }),
        );

        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(77),
                "custom/panic_once",
                None,
            )))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), panic_entered.notified())
            .await
            .expect("first handler invocation must run and panic");

        let mut cleaned_up = false;
        for _ in 0..200 {
            if client.incoming_cancellation_tokens().is_empty() {
                cleaned_up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            cleaned_up,
            "panicking handler must clean up request ID via Drop guard"
        );

        server_transport
            .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                serde_json::json!(77),
                "custom/panic_once",
                None,
            )))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
            .await
            .expect("expected subsequent request with same ID to run")
            .unwrap()
            .unwrap();
        match reply {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, serde_json::json!(77));
                assert_eq!(r.result, Some(serde_json::json!({ "recovered": true })));
            }
            other => panic!("expected Response, got {other:?}"),
        }

        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn i20_writer_task_survives_transient_send_error() {
        use std::sync::atomic::AtomicUsize;

        struct FlakyResponseTransport {
            inner: crate::transport::InMemoryTransport,
            response_send_attempts: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl McpTransport for FlakyResponseTransport {
            async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
                if matches!(msg, JsonRpcMessage::Response(_)) {
                    let n = self.response_send_attempts.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        anyhow::bail!("transient send failure on first response");
                    }
                }
                self.inner.send(msg).await
            }

            async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
                self.inner.recv().await
            }

            async fn close(&self) -> anyhow::Result<()> {
                self.inner.close().await
            }
        }

        let (client_inner, server_transport) = crate::transport::InMemoryTransport::pair();
        let attempts = Arc::new(AtomicUsize::new(0));
        let client_transport = Arc::new(FlakyResponseTransport {
            inner: client_inner,
            response_send_attempts: attempts.clone(),
        });

        let srv = tokio::spawn({
            let server_transport = server_transport;
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
                    server_transport
                        .send(JsonRpcMessage::Response(resp))
                        .await
                        .unwrap();
                }
                let _ = server_transport.recv().await; // notifications/initialized

                // Ping 1: client writer will fail transiently.
                server_transport
                    .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                        serde_json::json!(101),
                        "ping",
                        None,
                    )))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;

                // Ping 2: writer task must still be alive and deliver this response!
                server_transport
                    .send(JsonRpcMessage::Request(JsonRpcRequest::new(
                        serde_json::json!(102),
                        "ping",
                        None,
                    )))
                    .await
                    .unwrap();

                let reply = tokio::time::timeout(Duration::from_secs(2), server_transport.recv())
                    .await
                    .expect("writer task must stay alive after transient error")
                    .unwrap()
                    .unwrap();
                match reply {
                    JsonRpcMessage::Response(r) => assert_eq!(r.id, serde_json::json!(102)),
                    other => panic!("expected Response for 102, got {other:?}"),
                }
            }
        });

        let client = McpClient::connect(client_transport).await.unwrap();
        srv.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn c9_connect_with_roots_and_timeout_enforces_custom_timeout() {
        let closed = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(SlowSendTransport {
            closed: closed.clone(),
        });
        let start = std::time::Instant::now();
        let err = McpClient::connect_with_roots_and_timeout(
            transport,
            Vec::new(),
            Duration::from_millis(60),
        )
        .await
        .err()
        .expect("connect must time out at custom timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
        assert!(closed.load(Ordering::SeqCst));
    }
}
