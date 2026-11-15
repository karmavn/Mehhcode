//! Transport layer for MCP: In-memory, Stdio, and Streamable HTTP transports.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::jsonrpc::JsonRpcMessage;
use crate::oauth::McpBearerProvider;

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16MB guard
pub const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024; // 64KB ring buffer

/// Trait defining asynchronous transport for MCP JSON-RPC messages.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Sends a JSON-RPC message over the transport.
    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()>;

    /// Receives the next JSON-RPC message, or None if the transport has closed.
    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>>;

    /// Closes the transport and cleans up associated resources.
    async fn close(&self) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// InMemoryTransport
// ---------------------------------------------------------------------------

/// In-memory transport for unit testing without subprocesses or networks.
pub struct InMemoryTransport {
    sender: mpsc::Sender<JsonRpcMessage>,
    receiver: Mutex<mpsc::Receiver<JsonRpcMessage>>,
    closed: AtomicBool,
}

impl InMemoryTransport {
    /// Creates a connected pair of in-memory transports (client, server).
    pub fn pair() -> (Self, Self) {
        let (tx1, rx1) = mpsc::channel(256);
        let (tx2, rx2) = mpsc::channel(256);

        let t1 = Self {
            sender: tx2,
            receiver: Mutex::new(rx1),
            closed: AtomicBool::new(false),
        };
        let t2 = Self {
            sender: tx1,
            receiver: Mutex::new(rx2),
            closed: AtomicBool::new(false),
        };
        (t1, t2)
    }
}

#[async_trait]
impl McpTransport for InMemoryTransport {
    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("In-memory transport is closed");
        }
        self.sender
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send: receiver dropped"))
    }

    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let mut rx = self.receiver.lock().await;
        rx.recv().await.map(Ok)
    }

    async fn close(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StdioTransport
// ---------------------------------------------------------------------------

/// Stdio transport launching a local subprocess communicating over stdin/stdout.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_HTTP_ERROR_BYTES: usize = 16 * 1024;

async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| anyhow::anyhow!("I/O error reading child process stdout: {e}"))?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Ok(Some(buf));
        }

        if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
            let take_len = newline_pos + 1;
            let new_len = buf.len().saturating_add(take_len);
            if new_len > max_bytes {
                return Err(anyhow::anyhow!(
                    "MCP stdio message length {new_len} exceeds limit of {max_bytes} bytes"
                ));
            }
            buf.extend_from_slice(&available[..take_len]);
            reader.consume(take_len);
            return Ok(Some(buf));
        } else {
            let avail_len = available.len();
            let new_len = buf.len().saturating_add(avail_len);
            if new_len > max_bytes {
                return Err(anyhow::anyhow!(
                    "MCP stdio message length {new_len} exceeds limit of {max_bytes} bytes"
                ));
            }
            buf.extend_from_slice(available);
            reader.consume(avail_len);
        }
    }
}

async fn read_limited_body(
    resp: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    if let Some(content_length) = resp.content_length() {
        if content_length > max_bytes as u64 {
            anyhow::bail!(
                "MCP HTTP response Content-Length ({content_length} bytes) exceeds limit of {max_bytes} bytes"
            );
        }
    }

    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    loop {
        let next = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("Timed out while reading MCP HTTP response body"))?;
        match next {
            Some(Ok(chunk)) => {
                let new_len = buf.len().saturating_add(chunk.len());
                if new_len > max_bytes {
                    anyhow::bail!(
                        "MCP HTTP response body exceeded limit of {max_bytes} bytes"
                    );
                }
                buf.extend_from_slice(&chunk);
            }
            Some(Err(e)) => {
                return Err(anyhow::anyhow!("Error reading MCP HTTP response body: {e}"));
            }
            None => return Ok(buf),
        }
    }
}

pub struct StdioTransport {
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<BufReader<ChildStdout>>>,
    child: Mutex<Option<Child>>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    max_message_bytes: usize,
    closed: AtomicBool,
}

impl StdioTransport {
    /// Spawns a child process with standard I/O pipes.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP process '{command}'"))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take();

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(DEFAULT_MAX_STDERR_BYTES)));

        let stderr_task = if let Some(mut err_reader) = stderr {
            let tail_clone = stderr_tail.clone();
            Some(tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err_reader.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut tail = tail_clone.lock().await;
                    for &b in &buf[..n] {
                        if tail.len() >= DEFAULT_MAX_STDERR_BYTES {
                            tail.pop_front();
                        }
                        tail.push_back(b);
                    }
                }
            }))
        } else {
            None
        };

        Ok(Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(stdout),
            child: Mutex::new(Some(child)),
            stderr_tail,
            stderr_task: Mutex::new(stderr_task),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            closed: AtomicBool::new(false),
        })
    }

    /// Returns the captured tail of stderr as a lossy UTF-8 string.
    pub async fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().await;
        let bytes: Vec<u8> = tail.iter().copied().collect();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("Stdio transport is closed");
        }
        let mut stdin_guard = self.stdin.lock().await;
        let stdin = stdin_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Process stdin is unavailable or closed"))?;

        let bytes = serde_json::to_vec(&msg)?;
        stdin.write_all(&bytes).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let mut stdout_guard = self.stdout.lock().await;
        let reader = stdout_guard.as_mut()?;

        loop {
            match read_bounded_line(reader, self.max_message_bytes).await {
                Ok(None) => return None, // EOF reached
                Ok(Some(bytes)) => {
                    let line = match std::str::from_utf8(&bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            return Some(Err(anyhow::anyhow!(
                                "MCP child stdout contained invalid UTF-8: {e}"
                            )));
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                        Ok(msg) => return Some(Ok(msg)),
                        Err(e) => {
                            return Some(Err(anyhow::anyhow!(
                                "Failed to parse JSON-RPC line from child stdout: {e}"
                            )))
                        }
                    }
                }
                Err(e) => {
                    // Drop stdout reader on framing/size error so partial giant frames are not re-parsed.
                    let _ = stdout_guard.take();
                    return Some(Err(e));
                }
            }
        }
    }

    async fn close(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.stderr_task.lock().await.take() {
            handle.abort();
        }
        {
            let mut stdin_guard = self.stdin.lock().await;
            let _ = stdin_guard.take(); // Drops stdin pipe, signalling EOF to child
        }
        {
            let mut child_guard = self.child.lock().await;
            if let Some(mut child) = child_guard.take() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StreamableHttpTransport
// ---------------------------------------------------------------------------

async fn forward_sse_events(
    events: Vec<mehh_ai::sse::SseEvent>,
    tx: &mpsc::Sender<anyhow::Result<JsonRpcMessage>>,
) -> bool {
    for ev in events {
        if ev.data.is_empty() {
            continue;
        }
        let item = serde_json::from_str::<JsonRpcMessage>(&ev.data)
            .map_err(|e| anyhow::anyhow!("Invalid JSON-RPC payload in MCP SSE event: {e}"));
        if tx.send(item).await.is_err() {
            return false;
        }
    }
    true
}

/// Streamable HTTP transport for remote MCP servers using POST, SSE streams, and session tracking.
pub struct StreamableHttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Arc<RwLock<Option<String>>>,
    protocol_version: Arc<RwLock<Option<String>>>,
    auth_provider: Option<Arc<dyn McpBearerProvider>>,
    incoming_tx: mpsc::Sender<anyhow::Result<JsonRpcMessage>>,
    incoming_rx: Mutex<mpsc::Receiver<anyhow::Result<JsonRpcMessage>>>,
    sse_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    closed: AtomicBool,
}

impl StreamableHttpTransport {
    /// Creates a new Streamable HTTP transport targeting the given endpoint URL.
    pub fn new(url: impl Into<String>, headers: HashMap<String, String>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            url: url.into(),
            headers,
            session_id: Arc::new(RwLock::new(None)),
            protocol_version: Arc::new(RwLock::new(None)),
            auth_provider: None,
            incoming_tx: tx,
            incoming_rx: Mutex::new(rx),
            sse_tasks: Arc::new(Mutex::new(Vec::new())),
            closed: AtomicBool::new(false),
        }
    }

    /// Configures a dynamic bearer token provider for authentication and 401 token refresh.
    pub fn with_auth_provider(mut self, auth_provider: Arc<dyn McpBearerProvider>) -> Self {
        self.auth_provider = Some(auth_provider);
        self
    }

    /// Sets the negotiated protocol version sent with future requests.
    pub async fn set_protocol_version(&self, version: impl Into<String>) {
        let mut guard = self.protocol_version.write().await;
        *guard = Some(version.into());
    }

    /// Returns the active session ID, if established by the server.
    pub async fn session_id(&self) -> Option<String> {
        self.session_id.read().await.clone()
    }
}

#[async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("Streamable HTTP transport is closed");
        }

        let body = serde_json::to_vec(&msg)?;

        let build_req = |token: Option<&str>| {
            let mut req = self
                .client
                .post(&self.url)
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json");

            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            if let Some(t) = token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            req
        };

        let token = if let Some(ref auth) = self.auth_provider {
            auth.bearer_token().await.ok().flatten()
        } else {
            None
        };

        let mut req = build_req(token.as_deref());
        if let Some(sid) = self.session_id.read().await.as_ref() {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(pv) = self.protocol_version.read().await.as_ref() {
            req = req.header("MCP-Protocol-Version", pv);
        }

        let mut resp = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, req.body(body.clone()).send())
            .await
            .map_err(|_| anyhow::anyhow!("Timed out connecting to MCP server '{}'", self.url))?
            .with_context(|| format!("Failed to POST JSON-RPC to '{}'", self.url))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(ref auth) = self.auth_provider {
                let www_auth = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok());
                if let Ok(true) = auth.on_unauthorized(www_auth).await {
                    let new_token = auth.bearer_token().await.ok().flatten();
                    let mut retry_req = build_req(new_token.as_deref());
                    if let Some(sid) = self.session_id.read().await.as_ref() {
                        retry_req = retry_req.header("Mcp-Session-Id", sid);
                    }
                    if let Some(pv) = self.protocol_version.read().await.as_ref() {
                        retry_req = retry_req.header("MCP-Protocol-Version", pv);
                    }
                    resp = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, retry_req.body(body).send())
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "Timed out retrying authenticated POST to MCP server '{}'",
                                self.url
                            )
                        })?
                        .with_context(|| {
                            format!("Failed to retry POST JSON-RPC to '{}'", self.url)
                        })?;
                }
            }
        }

        // Capture session ID header if returned
        if let Some(sid_header) = resp.headers().get("mcp-session-id") {
            if let Ok(sid_str) = sid_header.to_str() {
                let mut guard = self.session_id.write().await;
                *guard = Some(sid_str.to_string());
            }
        }

        let status = resp.status();
        if !status.is_success() {
            let err_bytes = read_limited_body(resp, MAX_HTTP_ERROR_BYTES)
                .await
                .unwrap_or_default();
            let err_body = String::from_utf8_lossy(&err_bytes);
            anyhow::bail!("MCP HTTP request failed ({status}): {err_body}");
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = mehh_ai::sse::SseParser::new();
            let tx = self.incoming_tx.clone();

            let handle = tokio::spawn(async move {
                loop {
                    let next = tokio::time::timeout(SSE_IDLE_TIMEOUT, byte_stream.next()).await;
                    match next {
                        Ok(Some(Ok(chunk))) => {
                            let events = sse_parser.feed_bytes(&chunk);
                            if sse_parser.has_overflowed() {
                                let _ = tx
                                    .send(Err(anyhow::anyhow!(
                                        "MCP SSE buffer exceeded {} byte limit",
                                        DEFAULT_MAX_MESSAGE_BYTES
                                    )))
                                    .await;
                                return;
                            }
                            if !forward_sse_events(events, &tx).await {
                                return;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            let _ = tx
                                .send(Err(anyhow::anyhow!("MCP SSE stream error: {e}")))
                                .await;
                            return;
                        }
                        Ok(None) => break,
                        Err(_) => {
                            let _ = tx
                                .send(Err(anyhow::anyhow!("MCP SSE stream idle timeout")))
                                .await;
                            return;
                        }
                    }
                }
                let final_events = sse_parser.finish();
                if sse_parser.has_overflowed() {
                    let _ = tx
                        .send(Err(anyhow::anyhow!(
                            "MCP SSE buffer exceeded {} byte limit",
                            DEFAULT_MAX_MESSAGE_BYTES
                        )))
                        .await;
                    return;
                }
                let _ = forward_sse_events(final_events, &tx).await;
            });

            let mut tasks = self.sse_tasks.lock().await;
            tasks.retain(|h| !h.is_finished());
            tasks.push(handle);
        } else if content_type.contains("application/json") {
            let raw = read_limited_body(resp, DEFAULT_MAX_MESSAGE_BYTES).await?;
            let text = std::str::from_utf8(&raw)
                .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in MCP JSON response: {e}"))?;
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                if let Ok(m) = serde_json::from_str::<JsonRpcMessage>(trimmed) {
                    let _ = self.incoming_tx.send(Ok(m)).await;
                } else if let Ok(msgs) = serde_json::from_str::<Vec<JsonRpcMessage>>(trimmed) {
                    for m in msgs {
                        let _ = self.incoming_tx.send(Ok(m)).await;
                    }
                } else {
                    let preview: String = trimmed.chars().take(200).collect();
                    anyhow::bail!("Invalid JSON-RPC payload in MCP HTTP response: {preview}");
                }
            }
        }

        Ok(())
    }

    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let mut rx = self.incoming_rx.lock().await;
        rx.recv().await
    }

    async fn close(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut tasks = self.sse_tasks.lock().await;
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        let sid = self.session_id.read().await.clone();
        if let Some(session_id) = sid {
            let mut req = self
                .client
                .delete(&self.url)
                .header("Mcp-Session-Id", &session_id);
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            if let Some(ref auth) = self.auth_provider {
                if let Ok(Some(token)) = auth.bearer_token().await {
                    req = req.header("Authorization", format!("Bearer {token}"));
                }
            }
            // Best effort with 1 second timeout
            let _ = tokio::time::timeout(Duration::from_secs(1), req.send()).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_read_bounded_line_rejects_oversized_before_eof() {
        let oversized = vec![b'a'; 200];
        let mut reader = BufReader::with_capacity(32, Cursor::new(oversized));
        let err = read_bounded_line(&mut reader, 64).await.unwrap_err();
        assert!(err.to_string().contains("exceeds limit"));
    }

    #[tokio::test]
    async fn test_read_bounded_line_valid_multiline() {
        let data = b"{\"jsonrpc\":\"2.0\"}\n  \n{\"ok\":true}";
        let mut reader = BufReader::with_capacity(8, Cursor::new(data.as_slice()));
        let first = read_bounded_line(&mut reader, 128).await.unwrap().unwrap();
        assert_eq!(first, b"{\"jsonrpc\":\"2.0\"}\n");
        let second = read_bounded_line(&mut reader, 128).await.unwrap().unwrap();
        assert_eq!(second, b"  \n");
        let third = read_bounded_line(&mut reader, 128).await.unwrap().unwrap();
        assert_eq!(third, b"{\"ok\":true}");
        assert!(read_bounded_line(&mut reader, 128).await.unwrap().is_none());
    }
}
