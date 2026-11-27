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
use tokio::process::{Child, ChildStdout, Command};
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

    /// Notifies the transport of the negotiated MCP protocol version so that
    /// protocol-specific headers can be attached to subsequent requests.
    async fn on_protocol_negotiated(&self, _version: &str) {}

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
    closed_token: tokio_util::sync::CancellationToken,
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
            closed_token: tokio_util::sync::CancellationToken::new(),
        };
        let t2 = Self {
            sender: tx1,
            receiver: Mutex::new(rx2),
            closed: AtomicBool::new(false),
            closed_token: tokio_util::sync::CancellationToken::new(),
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
        tokio::select! {
            msg = rx.recv() => msg.map(Ok),
            _ = self.closed_token.cancelled() => None,
        }
    }

    async fn close(&self) -> anyhow::Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.closed_token.cancel();
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
static SHARED_MCP_HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn shared_mcp_http_client() -> Result<reqwest::Client, String> {
    if let Some(client) = SHARED_MCP_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }
    let client = mehh_ai::http_safety::build_safe_http_client(
        mehh_ai::http_safety::DnsPolicy::PublicAndLoopback,
        Duration::from_secs(15),
        None,
    )
    .map_err(|e| e.to_string())?;
    let _ = SHARED_MCP_HTTP_CLIENT.set(client.clone());
    Ok(client)
}

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

async fn read_limited_body_with_timeout(
    resp: reqwest::Response,
    max_bytes: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    if let Some(content_length) = resp.content_length() {
        if content_length > max_bytes as u64 {
            anyhow::bail!(
                "MCP HTTP response Content-Length ({content_length} bytes) exceeds limit of {max_bytes} bytes"
            );
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
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
                return Err(anyhow::anyhow!(
                    "Error reading MCP HTTP response body: {}",
                    e.without_url()
                ));
            }
            None => return Ok(buf),
        }
    }
}

type StdinWriteMsg = (Vec<u8>, tokio::sync::oneshot::Sender<anyhow::Result<()>>);

#[cfg(windows)]
mod win_job {
    use std::ffi::c_void;
    use std::num::NonZeroUsize;

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(lp_job_attributes: *const c_void, lp_name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            h_job: *mut c_void,
            job_object_information_class: u32,
            lp_job_object_information: *const c_void,
            cb_job_object_information_length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(h_job: *mut c_void, h_process: *mut c_void) -> i32;
        fn TerminateJobObject(h_job: *mut c_void, u_exit_code: u32) -> i32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    /// RAII handle for a Windows Job Object configured with `KILL_ON_JOB_CLOSE`.
    /// Stored as `NonZeroUsize` so the guard remains `Send` across Tokio awaits.
    pub(super) struct WindowsJobHandle {
        handle: Option<NonZeroUsize>,
    }

    impl WindowsJobHandle {
        pub(super) fn assign_process(raw_handle: *mut c_void) -> Option<Self> {
            if raw_handle.is_null() {
                return None;
            }
            let job_raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            let job_nz = NonZeroUsize::new(job_raw as usize)?;
            let job = Self {
                handle: Some(job_nz),
            };
            let mut info = JobObjectExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set_ok = unsafe {
                SetInformationJobObject(
                    job_raw,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    (&info as *const JobObjectExtendedLimitInformation).cast::<c_void>(),
                    std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                )
            };
            if set_ok == 0 {
                return None;
            }
            let assign_ok = unsafe { AssignProcessToJobObject(job_raw, raw_handle) };
            if assign_ok == 0 {
                return None;
            }
            Some(job)
        }

        pub(super) fn terminate_and_close(&mut self) {
            if let Some(handle) = self.handle.take() {
                let raw = handle.get() as *mut c_void;
                unsafe {
                    let _ = TerminateJobObject(raw, 1);
                    let _ = CloseHandle(raw);
                }
            }
        }
    }

    impl Drop for WindowsJobHandle {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let raw = handle.get() as *mut c_void;
                unsafe {
                    let _ = CloseHandle(raw);
                }
            }
        }
    }
}

#[cfg(windows)]
fn run_taskkill_sync(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let taskkill_bin = std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\taskkill.exe"))
        .unwrap_or_else(|_| "taskkill".to_string());
    let _ = std::process::Command::new(taskkill_bin)
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn unix_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn unix_kill_pgid(pid: u32) {
    if pid > 0 && pid <= i32::MAX as u32 {
        unsafe {
            unix_kill(-(pid as i32), 9);
        }
    }
}

#[cfg(any(unix, windows))]
struct ProcessTreeGuard {
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<win_job::WindowsJobHandle>,
}

#[cfg(any(unix, windows))]
impl ProcessTreeGuard {
    fn new(child: &Child) -> Self {
        let pid = child.id();
        #[cfg(windows)]
        let job = child
            .raw_handle()
            .and_then(win_job::WindowsJobHandle::assign_process);
        Self {
            pid,
            #[cfg(windows)]
            job,
        }
    }

    fn disarm(&mut self) {
        self.pid = None;
        #[cfg(windows)]
        {
            self.job = None;
        }
    }

    /// Terminates the process tree in-kernel (Windows Job Object or POSIX
    /// `kill(-pgid, SIGKILL)`). Returns `Some(pid)` only when a fallback
    /// `taskkill.exe` invocation is still needed.
    fn try_kill_kernel(&mut self) -> Option<u32> {
        #[cfg(windows)]
        if let Some(mut job) = self.job.take() {
            job.terminate_and_close();
            self.pid = None;
            return None;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            unix_kill_pgid(pid);
            return None;
        }
        self.pid.take()
    }

    fn kill_now(&mut self) {
        if let Some(pid) = self.try_kill_kernel() {
            #[cfg(windows)]
            {
                std::thread::spawn(move || run_taskkill_sync(pid));
            }
            #[cfg(not(windows))]
            let _ = pid;
        }
    }
}

#[cfg(any(unix, windows))]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

#[cfg(windows)]
fn lookup_env_ci(env: &HashMap<String, String>, key: &str) -> Option<std::ffi::OsString> {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os(key))
}

#[cfg(windows)]
fn resolve_windows_command(command: &str, env: &HashMap<String, String>) -> std::path::PathBuf {
    if command.contains(['/', '\\', ':']) {
        return std::path::PathBuf::from(command);
    }

    let pathext_os = lookup_env_ci(env, "PATHEXT")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
    let pathext_str = pathext_os.to_string_lossy();
    let exts: Vec<String> = pathext_str
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('.') {
                s.to_ascii_lowercase()
            } else {
                format!(".{}", s.to_ascii_lowercase())
            }
        })
        .collect();

    let lower_cmd = command.to_ascii_lowercase();
    let has_known_ext = exts.iter().any(|ext| lower_cmd.ends_with(ext));

    if let Some(path_os) = lookup_env_ci(env, "PATH") {
        for dir in std::env::split_paths(&path_os) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if has_known_ext {
                let candidate = dir.join(command);
                if candidate.is_file() {
                    return candidate;
                }
            } else {
                for ext in &exts {
                    let candidate = dir.join(format!("{command}{ext}"));
                    if candidate.is_file() {
                        return candidate;
                    }
                }
                if Path::new(command).extension().is_some() {
                    let candidate = dir.join(command);
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
    }

    std::path::PathBuf::from(command)
}

#[cfg(windows)]
fn quote_cmd_arg(arg: &str) -> anyhow::Result<String> {
    if arg.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        anyhow::bail!("MCP stdio command/argument contains unsupported control character");
    }
    if arg.is_empty() {
        return Ok("\"\"".to_string());
    }
    if arg.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'\\' | b':' | b'@')
    }) {
        return Ok(arg.to_string());
    }

    let mut out = String::with_capacity(arg.len() + 8);
    let mut in_quotes = false;
    let mut backslashes = 0usize;

    let open_quotes = |out: &mut String, in_quotes: &mut bool| {
        if !*in_quotes {
            out.push('"');
            *in_quotes = true;
        }
    };
    let close_quotes = |out: &mut String, in_quotes: &mut bool| {
        if *in_quotes {
            out.push('"');
            *in_quotes = false;
        }
    };

    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                close_quotes(&mut out, &mut in_quotes);
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                backslashes = 0;
                out.push_str("^^^\"");
            }
            '%' => {
                if backslashes > 0 {
                    open_quotes(&mut out, &mut in_quotes);
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                }
                close_quotes(&mut out, &mut in_quotes);
                out.push_str("^%");
            }
            _ => {
                open_quotes(&mut out, &mut in_quotes);
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }

    if backslashes > 0 {
        open_quotes(&mut out, &mut in_quotes);
        for _ in 0..(backslashes * 2) {
            out.push('\\');
        }
    }
    close_quotes(&mut out, &mut in_quotes);
    Ok(out)
}

#[cfg(windows)]
fn build_stdio_command(command: &str, args: &[String], env: &HashMap<String, String>) -> anyhow::Result<Command> {
    let resolved = resolve_windows_command(command, env);
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("cmd" | "bat")) {
        use std::os::windows::process::CommandExt;
        let cmd_exe = std::env::var("SystemRoot")
            .map(|r| format!("{r}\\System32\\cmd.exe"))
            .unwrap_or_else(|_| "cmd.exe".to_string());
        let resolved_str = resolved.to_string_lossy();
        let mut parts = Vec::with_capacity(args.len() + 1);
        parts.push(quote_cmd_arg(&resolved_str)?);
        for arg in args {
            parts.push(quote_cmd_arg(arg)?);
        }
        let inner = parts.join(" ");
        let mut cmd = Command::new(cmd_exe);
        cmd.args(["/d", "/s", "/c"]);
        cmd.as_std_mut().raw_arg(format!("\"{inner}\""));
        Ok(cmd)
    } else {
        let mut cmd = Command::new(resolved);
        cmd.args(args);
        Ok(cmd)
    }
}

pub struct StdioTransport {
    stdin_tx: std::sync::Mutex<Option<mpsc::Sender<StdinWriteMsg>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stdout: Mutex<Option<BufReader<ChildStdout>>>,
    child: Mutex<Option<Child>>,
    #[cfg(any(unix, windows))]
    process_tree: std::sync::Mutex<Option<ProcessTreeGuard>>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    max_message_bytes: usize,
    closed: AtomicBool,
    closed_token: tokio_util::sync::CancellationToken,
}

impl StdioTransport {
    /// Spawns a child process with standard I/O pipes.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let mut cmd = build_stdio_command(command, args, env)?;
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new(command);
            c.args(args);
            c
        };
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        // Scrub inherited secret-shaped variables first; an explicitly
        // declared cfg.env value is applied afterwards and is intentional.
        for (key, _) in std::env::vars_os() {
            if mehh_sandbox::is_secret_env_key(&key.to_string_lossy()) {
                cmd.env_remove(&key);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn MCP process '{command}'"))?;
        #[cfg(any(unix, windows))]
        let process_tree = std::sync::Mutex::new(Some(ProcessTreeGuard::new(&child)));

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        let stderr = child.stderr.take();

        // Dedicated writer task makes `send()` cancel-safe: once a framed line is
        // handed to the channel, cancelling the caller's future never leaves a
        // half-written JSON line on the pipe.
        let (stdin_tx, writer_task) = if let Some(mut child_stdin) = stdin {
            let (tx, mut rx) = mpsc::channel::<StdinWriteMsg>(64);
            let handle = tokio::spawn(async move {
                while let Some((bytes, ack)) = rx.recv().await {
                    let res = async {
                        child_stdin.write_all(&bytes).await?;
                        child_stdin.flush().await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    let failed = res.is_err();
                    let _ = ack.send(res);
                    if failed {
                        break;
                    }
                }
            });
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));

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
            stdin_tx: std::sync::Mutex::new(stdin_tx),
            writer_task: Mutex::new(writer_task),
            stdout: Mutex::new(stdout),
            child: Mutex::new(Some(child)),
            #[cfg(any(unix, windows))]
            process_tree,
            stderr_tail,
            stderr_task: Mutex::new(stderr_task),
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            closed: AtomicBool::new(false),
            closed_token: tokio_util::sync::CancellationToken::new(),
        })
    }

    /// Returns the captured tail of stderr as a lossy UTF-8 string.
    pub async fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().await;
        let bytes: Vec<u8> = tail.iter().copied().collect();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closed_token.cancel();
        if let Ok(mut guard) = self.stdin_tx.lock() {
            let _ = guard.take();
        }
        if let Some(handle) = self.writer_task.get_mut().take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_task.get_mut().take() {
            handle.abort();
        }
        #[cfg(any(unix, windows))]
        if let Ok(mut guard) = self.process_tree.lock() {
            if let Some(mut pt) = guard.take() {
                pt.kill_now();
            }
        }
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) || self.closed_token.is_cancelled() {
            anyhow::bail!("Stdio transport is closed");
        }
        let tx = {
            let guard = self
                .stdin_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clone()
        }
        .ok_or_else(|| anyhow::anyhow!("Process stdin is unavailable or closed"))?;

        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(b'\n');

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tokio::select! {
            res = tx.send((bytes, ack_tx)) => {
                res.map_err(|_| anyhow::anyhow!("Process stdin writer task is closed"))?;
            }
            _ = self.closed_token.cancelled() => {
                anyhow::bail!("Stdio transport is closed");
            }
        }
        ack_rx
            .await
            .map_err(|_| anyhow::anyhow!("Process stdin writer task dropped"))?
    }

    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
        if self.closed.load(Ordering::SeqCst) || self.closed_token.is_cancelled() {
            return None;
        }
        let mut stdout_guard = tokio::select! {
            guard = self.stdout.lock() => guard,
            _ = self.closed_token.cancelled() => return None,
        };
        let reader = stdout_guard.as_mut()?;

        loop {
            let line_res = tokio::select! {
                res = read_bounded_line(reader, self.max_message_bytes) => res,
                _ = self.closed_token.cancelled() => {
                    let _ = stdout_guard.take();
                    return None;
                }
            };
            match line_res {
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
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.closed_token.cancel();
        {
            let mut guard = self
                .stdin_tx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = guard.take(); // Drops channel sender, causing writer task to drop stdin and signal EOF
        }
        if let Some(mut handle) = self.writer_task.lock().await.take() {
            if tokio::time::timeout(Duration::from_millis(100), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
            }
        }
        if let Some(handle) = self.stderr_task.lock().await.take() {
            handle.abort();
        }
        {
            let mut child_guard = self.child.lock().await;
            #[cfg(any(unix, windows))]
            let mut pt_guard = self
                .process_tree
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(mut child) = child_guard.take() {
                // Give the process a short grace period to exit after stdin EOF.
                let exited_cleanly = tokio::time::timeout(Duration::from_millis(500), child.wait())
                    .await
                    .is_ok();
                #[cfg(any(unix, windows))]
                if let Some(mut pt) = pt_guard.take() {
                    let fallback_pid = pt.try_kill_kernel();
                    if !exited_cleanly {
                        #[cfg(windows)]
                        if let Some(pid) = fallback_pid {
                            use std::os::windows::process::CommandExt;
                            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                            let taskkill_bin = std::env::var("SystemRoot")
                                .map(|r| format!("{r}\\System32\\taskkill.exe"))
                                .unwrap_or_else(|_| "taskkill".to_string());
                            let mut tk = Command::new(taskkill_bin);
                            tk.args(["/PID", &pid.to_string(), "/T", "/F"])
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null());
                            tk.as_std_mut().creation_flags(CREATE_NO_WINDOW);
                            let _ = tk.status().await;
                        }
                        #[cfg(not(windows))]
                        let _ = fallback_pid;
                    }
                    pt.disarm();
                }
                if !exited_cleanly {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
            } else {
                #[cfg(any(unix, windows))]
                if let Some(mut pt) = pt_guard.take() {
                    pt.kill_now();
                }
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
    tracked_request_id: Option<&serde_json::Value>,
    delivered_response: &mut bool,
) -> bool {
    for ev in events {
        if ev.data.is_empty() {
            continue;
        }
        let item = serde_json::from_str::<JsonRpcMessage>(&ev.data)
            .map_err(|e| anyhow::anyhow!("Invalid JSON-RPC payload in MCP SSE event: {e}"));
        if let Ok(JsonRpcMessage::Response(ref resp)) = item {
            if let Some(req_id) = tracked_request_id {
                if crate::jsonrpc::id_to_key(&resp.id) == crate::jsonrpc::id_to_key(req_id) {
                    *delivered_response = true;
                }
            }
        }
        if tx.send(item).await.is_err() {
            return false;
        }
    }
    true
}

async fn emit_synthetic_sse_error_if_pending(
    tx: &mpsc::Sender<anyhow::Result<JsonRpcMessage>>,
    request_id: Option<serde_json::Value>,
    delivered_response: bool,
    reason: String,
) {
    if delivered_response {
        return;
    }
    if let Some(req_id) = request_id {
        let synthetic = crate::jsonrpc::JsonRpcResponse::error(req_id, -32000, reason, None);
        let _ = tx.send(Ok(JsonRpcMessage::Response(synthetic))).await;
    }
}

/// Strips query, fragment, and userinfo from a URL before interpolating it into error strings.
fn sanitize_url_for_error(raw: &str) -> String {
    if let Ok(mut parsed) = reqwest::Url::parse(raw) {
        parsed.set_query(None);
        parsed.set_fragment(None);
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        parsed.to_string()
    } else {
        raw.split(['?', '#']).next().unwrap_or(raw).to_string()
    }
}

/// Validates a Streamable HTTP MCP endpoint URL against SSRF rules (via `mehh_ai::http_safety`)
/// and rejects insecure non-loopback `http://` targets.
pub(crate) fn validate_mcp_http_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = mehh_ai::http_safety::normalize_and_validate_http_url(raw)
        .map_err(|e| anyhow::anyhow!("Invalid MCP HTTP URL: {e}"))?;
    mehh_ai::http_safety::check_url_host_policy(
        mehh_ai::http_safety::DnsPolicy::PublicAndLoopback,
        &url,
    )
    .map_err(|e| anyhow::anyhow!("MCP HTTP URL blocked by SSRF policy: {e}"))?;
    if url.scheme() == "http" {
        let host = mehh_ai::http_safety::normalized_url_host(&url)
            .ok_or_else(|| anyhow::anyhow!("MCP HTTP URL is missing a host"))?;
        if !mehh_ai::http_safety::is_loopback_host(&host) {
            anyhow::bail!(
                "Insecure non-loopback HTTP scheme rejected for MCP URL (use https:// or loopback http://)"
            );
        }
    }
    Ok(url)
}

/// Streamable HTTP transport for remote MCP servers using POST, SSE streams, and session tracking.
pub struct StreamableHttpTransport {
    client: Result<reqwest::Client, String>,
    url: String,
    validated_url: Result<reqwest::Url, String>,
    headers: HashMap<String, String>,
    request_timeout: Duration,
    session_id: Arc<RwLock<Option<String>>>,
    protocol_version: Arc<RwLock<Option<String>>>,
    auth_provider: Option<Arc<dyn McpBearerProvider>>,
    incoming_tx: mpsc::Sender<anyhow::Result<JsonRpcMessage>>,
    incoming_rx: Mutex<mpsc::Receiver<anyhow::Result<JsonRpcMessage>>>,
    sse_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    closed: AtomicBool,
    closed_token: tokio_util::sync::CancellationToken,
}

impl StreamableHttpTransport {
    /// Creates a new Streamable HTTP transport targeting the given endpoint URL,
    /// failing immediately if the URL violates SSRF/HTTPS policy or the HTTP client
    /// cannot be constructed.
    pub fn try_new(
        url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let transport = Self::new(url, headers);
        if let Err(ref e) = transport.validated_url {
            anyhow::bail!("{e}");
        }
        if let Err(ref e) = transport.client {
            anyhow::bail!("{e}");
        }
        Ok(transport)
    }

    /// Creates a new Streamable HTTP transport targeting the given endpoint URL.
    /// Any invalid URL or client-construction error is retained and enforced
    /// fail-closed on every request.
    pub fn new(url: impl Into<String>, headers: HashMap<String, String>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let raw_url = url.into();
        let validated_url = validate_mcp_http_url(&raw_url).map_err(|e| e.to_string());
        let client = shared_mcp_http_client();
        Self {
            client,
            url: raw_url,
            validated_url,
            headers,
            request_timeout: HTTP_REQUEST_TIMEOUT,
            session_id: Arc::new(RwLock::new(None)),
            protocol_version: Arc::new(RwLock::new(None)),
            auth_provider: None,
            incoming_tx: tx,
            incoming_rx: Mutex::new(rx),
            sse_tasks: Arc::new(Mutex::new(Vec::new())),
            closed: AtomicBool::new(false),
            closed_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Configures a dynamic bearer token provider for authentication and 401 token refresh.
    pub fn with_auth_provider(mut self, auth_provider: Arc<dyn McpBearerProvider>) -> Self {
        self.auth_provider = Some(auth_provider);
        self
    }

    /// Configures the per-request HTTP timeout (default: 60s).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
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

    fn collect_secrets<'a>(
        &'a self,
        token: Option<&'a str>,
        new_token: Option<&'a str>,
    ) -> Vec<&'a str> {
        let mut secrets: Vec<&str> = Vec::new();
        if let Some(t) = token {
            secrets.push(t);
        }
        if let Some(nt) = new_token {
            secrets.push(nt);
        }
        for (k, v) in &self.headers {
            let lower = k.to_ascii_lowercase();
            if lower == "authorization"
                || lower == "x-api-key"
                || lower.contains("token")
                || lower.contains("secret")
            {
                secrets.push(v);
            }
        }
        secrets
    }
}

impl Drop for StreamableHttpTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closed_token.cancel();
        if let Ok(mut tasks) = self.sse_tasks.try_lock() {
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
    }
}

#[async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn on_protocol_negotiated(&self, version: &str) {
        self.set_protocol_version(version).await;
    }

    async fn send(&self, msg: JsonRpcMessage) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            anyhow::bail!("Streamable HTTP transport is closed");
        }

        let target_url = self
            .validated_url
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .clone();
        let client = self
            .client
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let request_id = match &msg {
            JsonRpcMessage::Request(req) => Some(req.id.clone()),
            _ => None,
        };
        let is_request = request_id.is_some();
        let mut body = serde_json::to_vec(&msg)?;

        let build_req = |token: Option<&str>| {
            let mut req = client
                .post(target_url.clone())
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json");

            for (k, v) in &self.headers {
                if token.is_some() && k.eq_ignore_ascii_case("authorization") {
                    continue;
                }
                req = req.header(k, v);
            }
            if let Some(t) = token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            req
        };

        let token = if let Some(ref auth) = self.auth_provider {
            auth.bearer_token()
                .await
                .context("Failed to obtain MCP bearer token")?
        } else {
            None
        };
        let mut new_token: Option<String> = None;

        let mut req = build_req(token.as_deref());
        if let Some(sid) = self.session_id.read().await.as_ref() {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(pv) = self.protocol_version.read().await.as_ref() {
            req = req.header("MCP-Protocol-Version", pv);
        }

        let first_body = if self.auth_provider.is_some() {
            body.clone()
        } else {
            std::mem::take(&mut body)
        };

        let mut resp = tokio::select! {
            biased;
            _ = self.closed_token.cancelled() => {
                anyhow::bail!("Streamable HTTP transport is closed");
            }
            out = tokio::time::timeout(self.request_timeout, req.body(first_body).send()) => {
                out.map_err(|_| {
                    let safe_url = sanitize_url_for_error(&self.url);
                    anyhow::anyhow!("Timed out connecting to MCP server '{safe_url}'")
                })?
                .map_err(|e| {
                    let safe_url = sanitize_url_for_error(&self.url);
                    let secrets = self.collect_secrets(token.as_deref(), new_token.as_deref());
                    let safe_err = crate::oauth::redact_oauth_error_body(
                        &e.without_url().to_string(),
                        &secrets,
                    );
                    anyhow::anyhow!("Failed to POST JSON-RPC to '{safe_url}': {safe_err}")
                })?
            }
        };

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(ref auth) = self.auth_provider {
                let www_auth = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok());
                if auth
                    .on_unauthorized(token.as_deref(), www_auth)
                    .await
                    .context("Failed to handle MCP 401 unauthorized challenge")?
                {
                    new_token = auth
                        .bearer_token()
                        .await
                        .context("Failed to obtain refreshed MCP bearer token")?;
                    let mut retry_req = build_req(new_token.as_deref());
                    if let Some(sid) = self.session_id.read().await.as_ref() {
                        retry_req = retry_req.header("Mcp-Session-Id", sid);
                    }
                    if let Some(pv) = self.protocol_version.read().await.as_ref() {
                        retry_req = retry_req.header("MCP-Protocol-Version", pv);
                    }
                    resp = tokio::time::timeout(self.request_timeout, retry_req.body(body).send())
                        .await
                        .map_err(|_| {
                            let safe_url = sanitize_url_for_error(&self.url);
                            anyhow::anyhow!(
                                "Timed out retrying authenticated POST to MCP server '{safe_url}'"
                            )
                        })?
                        .map_err(|e| {
                            let safe_url = sanitize_url_for_error(&self.url);
                            let secrets =
                                self.collect_secrets(token.as_deref(), new_token.as_deref());
                            let safe_err = crate::oauth::redact_oauth_error_body(
                                &e.without_url().to_string(),
                                &secrets,
                            );
                            anyhow::anyhow!(
                                "Failed to retry POST JSON-RPC to '{safe_url}': {safe_err}"
                            )
                        })?;
                }
            }
        }

        let status = resp.status();
        if status.is_redirection() {
            if let Some(location) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
                if let Err(err) = validate_mcp_http_url(location) {
                    let secrets = self.collect_secrets(token.as_deref(), new_token.as_deref());
                    anyhow::bail!(
                        "MCP HTTP redirect ({status}) target rejected: {}",
                        crate::oauth::redact_oauth_error_body(&err.to_string(), &secrets)
                    );
                }
            }
            anyhow::bail!("MCP HTTP redirects are not supported ({status})");
        }
        if status == reqwest::StatusCode::NOT_FOUND && self.session_id.read().await.is_some() {
            *self.session_id.write().await = None;
            anyhow::bail!("MCP session expired (HTTP 404); re-initialize the session");
        }
        if !status.is_success() {
            let err_bytes =
                read_limited_body_with_timeout(resp, MAX_HTTP_ERROR_BYTES, self.request_timeout)
                    .await
                    .unwrap_or_default();
            let err_body = String::from_utf8_lossy(&err_bytes);
            let secrets = self.collect_secrets(token.as_deref(), new_token.as_deref());
            let safe_body = crate::oauth::redact_oauth_error_body(&err_body, &secrets);
            anyhow::bail!("MCP HTTP request failed ({status}): {safe_body}");
        }

        // Extract session ID header candidate; commit it only after validating the response Content-Type.
        let maybe_sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);

        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            if is_request {
                let _ = read_limited_body_with_timeout(
                    resp,
                    MAX_HTTP_ERROR_BYTES,
                    self.request_timeout,
                )
                .await;
                anyhow::bail!("MCP HTTP {status} response is invalid for a JSON-RPC request");
            }
            if let Some(sid_str) = maybe_sid {
                let mut guard = self.session_id.write().await;
                *guard = Some(sid_str);
            }
            let _ =
                read_limited_body_with_timeout(resp, MAX_HTTP_ERROR_BYTES, self.request_timeout)
                    .await;
            return Ok(());
        }

        let raw_content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let media_type = raw_content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        let is_valid_content_type = media_type == "text/event-stream"
            || media_type == "application/json"
            || media_type.ends_with("+json");

        if !is_valid_content_type {
            let err_bytes =
                read_limited_body_with_timeout(resp, MAX_HTTP_ERROR_BYTES, self.request_timeout)
                    .await
                    .unwrap_or_default();
            let preview: String = String::from_utf8_lossy(&err_bytes).chars().take(200).collect();
            let secrets = self.collect_secrets(token.as_deref(), new_token.as_deref());
            let safe_preview = crate::oauth::redact_oauth_error_body(&preview, &secrets);
            anyhow::bail!(
                "Unexpected Content-Type '{raw_content_type}' in MCP HTTP {status} response: {safe_preview}"
            );
        }

        if let Some(sid_str) = maybe_sid {
            let mut guard = self.session_id.write().await;
            *guard = Some(sid_str);
        }

        if media_type == "text/event-stream" {
            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = mehh_ai::sse::SseParser::new();
            let tx = self.incoming_tx.clone();

            let handle = tokio::spawn(async move {
                let mut delivered_response = false;
                loop {
                    let next = tokio::time::timeout(SSE_IDLE_TIMEOUT, byte_stream.next()).await;
                    match next {
                        Ok(Some(Ok(chunk))) => {
                            let events = sse_parser.feed_bytes(&chunk);
                            if sse_parser.has_overflowed() {
                                let reason = format!(
                                    "MCP SSE buffer exceeded {DEFAULT_MAX_MESSAGE_BYTES} byte limit"
                                );
                                let _ = tx.send(Err(anyhow::anyhow!("{reason}"))).await;
                                emit_synthetic_sse_error_if_pending(
                                    &tx,
                                    request_id,
                                    delivered_response,
                                    reason,
                                )
                                .await;
                                return;
                            }
                            if !forward_sse_events(
                                events,
                                &tx,
                                request_id.as_ref(),
                                &mut delivered_response,
                            )
                            .await
                            {
                                return;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            let reason = format!("MCP SSE stream error: {}", e.without_url());
                            let _ = tx.send(Err(anyhow::anyhow!("{reason}"))).await;
                            emit_synthetic_sse_error_if_pending(
                                &tx,
                                request_id,
                                delivered_response,
                                reason,
                            )
                            .await;
                            return;
                        }
                        Ok(None) => break,
                        Err(_) => {
                            let reason = "MCP SSE stream idle timeout".to_string();
                            let _ = tx.send(Err(anyhow::anyhow!("{reason}"))).await;
                            emit_synthetic_sse_error_if_pending(
                                &tx,
                                request_id,
                                delivered_response,
                                reason,
                            )
                            .await;
                            return;
                        }
                    }
                }
                let final_events = sse_parser.finish();
                if sse_parser.has_overflowed() {
                    let reason = format!(
                        "MCP SSE buffer exceeded {DEFAULT_MAX_MESSAGE_BYTES} byte limit"
                    );
                    let _ = tx.send(Err(anyhow::anyhow!("{reason}"))).await;
                    emit_synthetic_sse_error_if_pending(
                        &tx,
                        request_id,
                        delivered_response,
                        reason,
                    )
                    .await;
                    return;
                }
                if !forward_sse_events(
                    final_events,
                    &tx,
                    request_id.as_ref(),
                    &mut delivered_response,
                )
                .await
                {
                    return;
                }
                emit_synthetic_sse_error_if_pending(
                    &tx,
                    request_id,
                    delivered_response,
                    "MCP SSE stream ended before delivering response".to_string(),
                )
                .await;
            });

            let mut tasks = self.sse_tasks.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                handle.abort();
                anyhow::bail!("Streamable HTTP transport is closed");
            }
            tasks.retain(|h| !h.is_finished());
            tasks.push(handle);
        } else if media_type == "application/json" || media_type.ends_with("+json") {
            let raw = read_limited_body_with_timeout(
                resp,
                DEFAULT_MAX_MESSAGE_BYTES,
                self.request_timeout,
            )
            .await?;
            let text = std::str::from_utf8(&raw)
                .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in MCP JSON response: {e}"))?;
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                if trimmed.starts_with('[') {
                    if let Ok(msgs) = serde_json::from_str::<Vec<JsonRpcMessage>>(trimmed) {
                        for m in msgs {
                            let _ = self.incoming_tx.send(Ok(m)).await;
                        }
                        return Ok(());
                    }
                } else if let Ok(m) = serde_json::from_str::<JsonRpcMessage>(trimmed) {
                    let _ = self.incoming_tx.send(Ok(m)).await;
                    return Ok(());
                }
                let preview: String = trimmed.chars().take(200).collect();
                let secrets = self.collect_secrets(token.as_deref(), new_token.as_deref());
                let safe_preview = crate::oauth::redact_oauth_error_body(&preview, &secrets);
                anyhow::bail!("Invalid JSON-RPC payload in MCP HTTP response: {safe_preview}");
            } else if is_request {
                anyhow::bail!("MCP server returned empty JSON response body for a JSON-RPC request");
            }
        }

        Ok(())
    }

    async fn recv(&self) -> Option<anyhow::Result<JsonRpcMessage>> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let mut rx = self.incoming_rx.lock().await;
        tokio::select! {
            msg = rx.recv() => msg,
            _ = self.closed_token.cancelled() => None,
        }
    }

    async fn close(&self) -> anyhow::Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.closed_token.cancel();
        {
            let mut tasks = self.sse_tasks.lock().await;
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        let sid = self.session_id.write().await.take();
        if let Some(session_id) = sid {
            if let (Ok(client), Ok(target_url)) = (&self.client, validate_mcp_http_url(&self.url)) {
                // Bound the entire teardown DELETE (including any auth token lookup) to 1 second.
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    let mut req = client
                        .delete(target_url)
                        .header("Mcp-Session-Id", &session_id);
                    let mut has_auth = false;
                    if let Some(ref auth) = self.auth_provider {
                        match auth.bearer_token().await {
                            Ok(Some(token)) => {
                                req = req.header("Authorization", format!("Bearer {token}"));
                                has_auth = true;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to obtain bearer token when closing MCP session: {e}"
                                );
                            }
                        }
                    }
                    for (k, v) in &self.headers {
                        if has_auth && k.eq_ignore_ascii_case("authorization") {
                            continue;
                        }
                        req = req.header(k, v);
                    }
                    let _ = req.send().await;
                })
                .await;
            }
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

    #[tokio::test]
    async fn stdio_transport_scrubs_secret_env_vars_unless_explicitly_configured() {
        struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl EnvGuard {
            fn set(entries: &[(&'static str, &str)]) -> Self {
                let mut prev = Vec::new();
                for (k, v) in entries {
                    prev.push((*k, std::env::var_os(k)));
                    std::env::set_var(k, v);
                }
                Self(prev)
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (k, prev) in &self.0 {
                    match prev {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }

        let _guard = EnvGuard::set(&[
            ("MEHH_TEST_ANTHROPIC_API_KEY", "LEAK_ANTHROPIC"),
            ("MEHH_TEST_OPENAI_API_KEY", "LEAK_OPENAI"),
            ("MEHH_TEST_AWS_SECRET", "LEAK_AWS"),
            ("MEHH_TEST_GITHUB_TOKEN", "LEAK_GITHUB"),
            ("MEHH_TEST_DB_PASSWORD", "LEAK_PASSWORD"),
            ("DATABASE_URL", "LEAK_DATABASE_URL"),
            ("REDIS_URL", "LEAK_REDIS_URL"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "LEAK_GOOGLE_CREDS"),
            ("CONNECTION_STRING", "LEAK_CONN_STR"),
        ]);

        let mut cfg_env = HashMap::new();
        cfg_env.insert(
            "MEHH_TEST_ANTHROPIC_API_KEY".to_string(),
            "EXPLICIT_OK".to_string(),
        );

        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe",
                vec!["/C".to_string(), "echo __ENV_DUMP__ 1>&2 & set 1>&2".to_string()],
            )
        } else {
            (
                "/bin/sh",
                vec!["-c".to_string(), "echo __ENV_DUMP__ >&2; env >&2".to_string()],
            )
        };

        let transport = StdioTransport::spawn(program, &args, &cfg_env, None).unwrap();
        let dump = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let tail = transport.stderr_tail().await;
                if tail.contains("__ENV_DUMP__") {
                    return tail;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("child never dumped its environment");
        transport.close().await.unwrap();

        assert!(dump.contains("EXPLICIT_OK"), "declared cfg.env must win");
        for leak in [
            "LEAK_ANTHROPIC",
            "LEAK_OPENAI",
            "LEAK_AWS",
            "LEAK_GITHUB",
            "LEAK_PASSWORD",
            "LEAK_DATABASE_URL",
            "LEAK_REDIS_URL",
            "LEAK_GOOGLE_CREDS",
            "LEAK_CONN_STR",
        ] {
            assert!(!dump.contains(leak), "secret leaked to MCP child: {leak}");
        }
    }

    #[tokio::test]
    async fn streamable_http_rejects_ssrf_literals_and_non_loopback_http() {
        for bad in [
            "https://169.254.169.254/mcp",
            "https://10.0.0.1/mcp",
            "https://192.168.1.1/mcp",
            "https://[::ffff:169.254.169.254]/mcp",
            "https://[fe80::1]/mcp",
            "https://metadata.google.internal/mcp",
            "http://example.com/mcp",
            "http://93.184.216.34/mcp",
            "file:///etc/passwd",
        ] {
            assert!(
                StreamableHttpTransport::try_new(bad, HashMap::new()).is_err(),
                "try_new must reject {bad}"
            );
            let tr = StreamableHttpTransport::new(bad, HashMap::new());
            let req = JsonRpcMessage::Notification(crate::jsonrpc::JsonRpcNotification::new(
                "notifications/initialized",
                None,
            ));
            assert!(
                tr.send(req).await.is_err(),
                "send must fail closed for {bad}"
            );
        }

        for good in [
            "http://127.0.0.1:8080/mcp",
            "http://localhost:8080/mcp",
            "http://[::1]:8080/mcp",
            "https://example.com/mcp",
        ] {
            assert!(
                StreamableHttpTransport::try_new(good, HashMap::new()).is_ok(),
                "try_new must allow {good}"
            );
        }
    }

    #[tokio::test]
    async fn streamable_http_captures_session_id_only_on_2xx_and_rejects_bad_content_type() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let step = Arc::new(AtomicUsize::new(0));
        let step_srv = step.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let cur = step_srv.fetch_add(1, Ordering::SeqCst);
                let resp = match cur {
                    // Step 0: 500 error with Mcp-Session-Id header -> must NOT be captured.
                    0 => "HTTP/1.1 500 Internal Server Error\r\nMcp-Session-Id: forged-500-sid\r\nContent-Length: 3\r\nConnection: close\r\n\r\nerr".to_string(),
                    // Step 1: 200 OK with text/html + Mcp-Session-Id -> must fail and NOT capture session ID.
                    1 => "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nMcp-Session-Id: forged-html-sid\r\nContent-Length: 13\r\nConnection: close\r\n\r\n<html></html>".to_string(),
                    // Step 2: 200 OK with application/json + Mcp-Session-Id -> must capture.
                    2 => {
                        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: valid-200-sid\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                    // Step 3: 202 Accepted with empty Content-Type -> must succeed.
                    3 => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    // Step 4: DELETE on close() -> count once.
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let tr = StreamableHttpTransport::try_new(format!("http://{addr}/mcp"), HashMap::new()).unwrap();
        let msg = || {
            JsonRpcMessage::Request(crate::jsonrpc::JsonRpcRequest::new(
                serde_json::json!(1),
                "ping",
                None,
            ))
        };

        // 500 error: session ID must NOT be captured.
        assert!(tr.send(msg()).await.is_err());
        assert_eq!(tr.session_id().await, None);

        // 200 text/html: must return explicit error and not capture session ID.
        let ct_err = tr.send(msg()).await.unwrap_err();
        assert!(
            ct_err.to_string().contains("Unexpected Content-Type"),
            "expected unexpected Content-Type error, got: {ct_err}"
        );
        assert_eq!(tr.session_id().await, None);

        // 200 application/json: session ID must be captured.
        tr.send(msg()).await.unwrap();
        assert_eq!(tr.session_id().await.as_deref(), Some("valid-200-sid"));

        // 202 empty: must succeed.
        let notif = JsonRpcMessage::Notification(crate::jsonrpc::JsonRpcNotification::new(
            "notifications/initialized",
            None,
        ));
        tr.send(notif).await.unwrap();

        // Idempotent close(): DELETE sent at most once.
        tr.close().await.unwrap();
        tr.close().await.unwrap();
        assert_eq!(step.load(Ordering::SeqCst), 5);
        server.abort();
    }

    #[tokio::test]
    async fn streamable_http_propagates_auth_errors_and_redacts_refreshed_token() {
        use tokio::net::TcpListener;

        struct FailingBearerProvider;
        #[async_trait]
        impl McpBearerProvider for FailingBearerProvider {
            async fn bearer_token(&self) -> anyhow::Result<Option<String>> {
                anyhow::bail!("auth store corrupted")
            }
            async fn on_unauthorized(
                &self,
                _failed_token: Option<&str>,
                _www_authenticate: Option<&str>,
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
        }

        let tr_err = StreamableHttpTransport::try_new("http://127.0.0.1:1/mcp", HashMap::new())
            .unwrap()
            .with_auth_provider(Arc::new(FailingBearerProvider));
        let err = tr_err
            .send(JsonRpcMessage::Notification(
                crate::jsonrpc::JsonRpcNotification::new("ping", None),
            ))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to obtain MCP bearer token"),
            "bearer_token() error must propagate, got: {err}"
        );

        struct RefreshingProvider {
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl McpBearerProvider for RefreshingProvider {
            async fn bearer_token(&self) -> anyhow::Result<Option<String>> {
                if self.calls.load(Ordering::SeqCst) == 0 {
                    Ok(Some("old-secret-token-111".to_string()))
                } else {
                    Ok(Some("new-secret-token-999".to_string()))
                }
            }
            async fn on_unauthorized(
                &self,
                _failed_token: Option<&str>,
                _www_authenticate: Option<&str>,
            ) -> anyhow::Result<bool> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = if attempt == 0 {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    let body = "rejected token new-secret-token-999";
                    format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let tr_refresh = StreamableHttpTransport::try_new(format!("http://{addr}/mcp"), HashMap::new())
            .unwrap()
            .with_auth_provider(Arc::new(RefreshingProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }));
        let err = tr_refresh
            .send(JsonRpcMessage::Notification(
                crate::jsonrpc::JsonRpcNotification::new("ping", None),
            ))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("new-secret-token-999"),
            "refreshed token must be redacted from error: {msg}"
        );
        assert!(msg.contains("[REDACTED]"), "expected [REDACTED] in: {msg}");
        srv.abort();
    }

    #[tokio::test]
    async fn read_limited_body_enforces_single_pinned_deadline_across_chunks() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n")
                .await
                .unwrap();
            // Trickle 1 byte every 40ms (total > 120ms); per-chunk timeout would reset and never fire.
            for _ in 0..8 {
                if sock.write_all(b"1\r\na\r\n").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });

        let resp = reqwest::get(format!("http://{addr}/slow")).await.unwrap();
        let err = read_limited_body_with_timeout(resp, 1024, Duration::from_millis(120))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Timed out"),
            "pinned overall deadline must reject slowloris trickle, got: {err}"
        );
        srv.abort();
    }

    #[tokio::test]
    async fn stdio_transport_recv_wakes_on_close_token() {
        let (program, args) = if cfg!(windows) {
            (
                "powershell.exe",
                vec!["-NoProfile".to_string(), "-Command".to_string(), "Start-Sleep -Seconds 10".to_string()],
            )
        } else {
            ("/bin/sh", vec!["-c".to_string(), "sleep 10".to_string()])
        };

        let transport = Arc::new(StdioTransport::spawn(program, &args, &HashMap::new(), None).unwrap());
        let tr_recv = transport.clone();
        let recv_task = tokio::spawn(async move { tr_recv.recv().await });

        tokio::time::sleep(Duration::from_millis(30)).await;
        transport.close().await.unwrap();

        let recv_res = tokio::time::timeout(Duration::from_secs(2), recv_task)
            .await
            .expect("recv() must wake up promptly on close()")
            .unwrap();
        assert!(recv_res.is_none());
    }

    #[tokio::test]
    async fn streamable_http_errors_strip_query_and_fragment_secrets() {
        // Bind and immediately drop a listener so the port refuses connections immediately.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/mcp?api_key=super-secret-query-val#secret-frag");
        let tr = StreamableHttpTransport::try_new(url, HashMap::new()).unwrap();
        let err = tr
            .send(JsonRpcMessage::Notification(
                crate::jsonrpc::JsonRpcNotification::new("ping", None),
            ))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("super-secret-query-val"), "query secret leaked: {msg}");
        assert!(!msg.contains("secret-frag"), "fragment secret leaked: {msg}");
        assert!(msg.contains(&format!("http://{addr}/mcp")), "expected sanitized URL in: {msg}");
    }

    #[tokio::test]
    async fn streamable_http_close_bounds_slow_bearer_provider() {
        struct SlowBearerProvider;
        #[async_trait]
        impl McpBearerProvider for SlowBearerProvider {
            async fn bearer_token(&self) -> anyhow::Result<Option<String>> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(Some("tok".to_string()))
            }
            async fn on_unauthorized(
                &self,
                _failed_token: Option<&str>,
                _www_authenticate: Option<&str>,
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
        }

        let tr = StreamableHttpTransport::try_new("http://127.0.0.1:1/mcp", HashMap::new())
            .unwrap()
            .with_auth_provider(Arc::new(SlowBearerProvider));
        *tr.session_id.write().await = Some("sid-123".to_string());

        let start = std::time::Instant::now();
        tr.close().await.unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "close() must bound slow auth refresh to ~1s, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn read_limited_body_error_strips_url_secrets() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            // Advertise 100 bytes, send only 2, and close immediately to trigger a reqwest body error.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nhi")
                .await;
        });

        let resp = reqwest::get(format!("http://{addr}/mcp?api_key=secret-body-query"))
            .await
            .unwrap();
        let err = read_limited_body_with_timeout(resp, 1024, Duration::from_secs(2))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Error reading MCP HTTP response body"),
            "expected body read error, got: {msg}"
        );
        assert!(
            !msg.contains("secret-body-query"),
            "query secret must be stripped via without_url(): {msg}"
        );
        srv.abort();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn c7_stdio_transport_resolves_bare_cmd_via_pathext_and_quotes_args_safely() {
        let dir = crate::TempTestDir::new("c7_pathext");
        let ps1_path = dir.path().join("echo.ps1");
        std::fs::write(
            &ps1_path,
            "$args | ForEach-Object { [Console]::Error.WriteLine(('ARG:' + $_)) }\r\n",
        )
        .unwrap();
        let cmd_path = dir.path().join("mock_mcp_srv.cmd");
        std::fs::write(
            &cmd_path,
            "@echo off\r\n\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile -NonInteractive -File \"%~dp0echo.ps1\" %*\r\n",
        )
        .unwrap();

        let existing_path = std::env::var("PATH").unwrap_or_default();
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            format!("{};{existing_path}", dir.path().display()),
        );
        env.insert("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string());

        let args = vec![
            "hello & echo INJECTED".to_string(),
            "%OS%".to_string(),
            "quoted\"val".to_string(),
        ];
        let transport = StdioTransport::spawn("mock_mcp_srv", &args, &env, None)
            .expect("bare .cmd command must resolve via PATH + PATHEXT");

        let output = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let tail = transport.stderr_tail().await;
                if tail.contains("ARG:quoted\"val") {
                    return tail;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("timed out waiting for .cmd script arguments on stderr");

        assert!(
            output.contains("ARG:hello & echo INJECTED"),
            "expected literal arg with '&', got: {output}"
        );
        assert!(
            !output.lines().any(|line| line.trim() == "INJECTED"),
            "cmd metacharacter '&' must not execute: {output}"
        );
        assert!(
            output.contains("ARG:%OS%"),
            "percent env var must not expand: {output}"
        );
        assert!(
            output.contains("ARG:quoted\"val"),
            "quoted arg must survive safely: {output}"
        );
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn c7_stdio_transport_reaps_grandchild_on_close_and_drop() {
        use tokio::net::TcpListener;

        for use_drop in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();

            let (program, args) = if cfg!(windows) {
                let child_script = format!(
                    "$c = [Net.Sockets.TcpClient]::new('127.0.0.1', {port}); Start-Sleep -Seconds 30"
                );
                let utf16: Vec<u8> = child_script
                    .encode_utf16()
                    .flat_map(|u| u.to_le_bytes())
                    .collect();
                let encoded = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &utf16,
                );
                let ps_cmd = format!(
                    "Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{encoded}'"
                );
                (
                    "powershell.exe",
                    vec![
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-Command".to_string(),
                        ps_cmd,
                    ],
                )
            } else {
                (
                    "/bin/sh",
                    vec![
                        "-c".to_string(),
                        format!(
                            "python3 -c 'import socket,time; s=socket.create_connection((\"127.0.0.1\",{port})); time.sleep(30)' &"
                        ),
                    ],
                )
            };

            let transport = StdioTransport::spawn(program, &args, &HashMap::new(), None).unwrap();
            let (mut sock, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .expect("grandchild must connect")
                .unwrap();

            if use_drop {
                drop(transport);
            } else {
                transport.close().await.unwrap();
            }

            let mut buf = [0u8; 16];
            let read_res = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut buf))
                .await
                .expect("grandchild socket must close promptly when process tree is reaped");
            assert!(
                matches!(read_res, Ok(0))
                    || matches!(
                        read_res,
                        Err(ref e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            )
                    ),
                "expected EOF or reset from terminated grandchild, got: {read_res:?}"
            );
        }
    }

    #[tokio::test]
    async fn c8_streamable_http_request_vs_notification_status_and_empty_body_handling() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let step = Arc::new(AtomicUsize::new(0));
        let step_srv = step.clone();

        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let cur = step_srv.fetch_add(1, Ordering::SeqCst);
                let resp = match cur {
                    // 0: Request -> 202 Accepted (must fail)
                    0 => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    // 1: Request -> 204 No Content (must fail)
                    1 => "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
                    // 2: Request -> 200 application/json with empty body (must fail)
                    2 => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    // 3: Notification -> 202 Accepted with text/plain body (must succeed)
                    3 => "HTTP/1.1 202 Accepted\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\naccepted",
                    // 4: Notification -> 204 No Content with application/json (must succeed)
                    _ => "HTTP/1.1 204 No Content\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let tr = StreamableHttpTransport::try_new(format!("http://{addr}/mcp"), HashMap::new()).unwrap();
        let mk_req = |id: i64| {
            JsonRpcMessage::Request(crate::jsonrpc::JsonRpcRequest::new(
                serde_json::json!(id),
                "ping",
                None,
            ))
        };
        let mk_notif = || {
            JsonRpcMessage::Notification(crate::jsonrpc::JsonRpcNotification::new(
                "notifications/initialized",
                None,
            ))
        };

        assert!(tr.send(mk_req(1)).await.is_err(), "Request + 202 must error");
        assert!(tr.send(mk_req(2)).await.is_err(), "Request + 204 must error");
        assert!(tr.send(mk_req(3)).await.is_err(), "Request + empty JSON body must error");
        tr.send(mk_notif()).await.expect("Notification + 202 text/plain must succeed");
        tr.send(mk_notif()).await.expect("Notification + 204 application/json must succeed");
        srv.abort();
    }

    #[tokio::test]
    async fn i20_streamable_http_sse_eof_without_response_emits_synthetic_error() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // Send SSE headers + a notification event, then close stream without delivering the response.
            let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progressToken\":42,\"progress\":1}}\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let tr = StreamableHttpTransport::try_new(format!("http://{addr}/mcp"), HashMap::new()).unwrap();
        let req = JsonRpcMessage::Request(crate::jsonrpc::JsonRpcRequest::new(
            serde_json::json!(901),
            "tools/call",
            None,
        ));
        tr.send(req).await.unwrap();

        // First recv: the progress notification.
        let first = tokio::time::timeout(Duration::from_secs(2), tr.recv())
            .await
            .expect("notification must arrive")
            .unwrap()
            .unwrap();
        assert!(matches!(first, JsonRpcMessage::Notification(_)));

        // Second recv: must be synthetic JSON-RPC error for request id 901 instead of hanging!
        let second = tokio::time::timeout(Duration::from_secs(2), tr.recv())
            .await
            .expect("SSE EOF without response must emit synthetic error immediately")
            .unwrap()
            .unwrap();
        match second {
            JsonRpcMessage::Response(resp) => {
                assert_eq!(resp.id, serde_json::json!(901));
                let err = resp.error.expect("synthetic response must carry error");
                assert_eq!(err.code, -32000);
            }
            other => panic!("expected synthetic JsonRpcResponse::error, got {other:?}"),
        }
        srv.abort();
    }

    #[tokio::test]
    async fn c9_streamable_http_with_request_timeout_bounds_slow_post() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let tr = StreamableHttpTransport::try_new(format!("http://{addr}/mcp"), HashMap::new())
            .unwrap()
            .with_request_timeout(Duration::from_millis(80));
        let start = std::time::Instant::now();
        let err = tr
            .send(JsonRpcMessage::Notification(
                crate::jsonrpc::JsonRpcNotification::new("ping", None),
            ))
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("Timed out"));
        srv.abort();
    }
}
