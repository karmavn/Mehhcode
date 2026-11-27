use std::future::pending;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mehh_agent::AgentToolUpdateCallback;
use serde_json::{Map, Value};
use tokio::process::Child;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::output::RollingBytes;

pub(crate) const CHUNK_READ_SIZE: usize = 4096;
pub(crate) const MAX_STREAMING_PREVIEW_BYTES: usize = 256 * 1024;
pub(crate) const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
pub(crate) const MAX_OUTPUT_CHARS: usize = 30_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 600_000;
const PIPE_GRACE: Duration = Duration::from_millis(500);

pub(crate) const UTF8_OUTPUT_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";
pub(crate) const WINDOWS_POWERSHELL_EXIT_SUFFIX: &str =
    "\n;\nif ($?) { exit 0 }\nif (($LASTEXITCODE -eq 0) -and (\"$($Error[0].FullyQualifiedErrorId)\" -like \"NativeCommandError*\")) { exit 0 }\nif ($LASTEXITCODE) { exit $LASTEXITCODE } else { exit 1 }";

/// Timeouts are unambiguous milliseconds. `timeoutMs` is an explicit alias;
/// zero or negative values fall back to the default instead of expiring at
/// once.
pub(crate) fn parse_timeout_ms(params: &Map<String, Value>) -> u64 {
    params
        .get("timeoutMs")
        .filter(|v| !v.is_null())
        .or_else(|| params.get("timeout").filter(|v| !v.is_null()))
        .and_then(|v| v.as_i64())
        .map(|t| {
            if t <= 0 {
                DEFAULT_TIMEOUT_MS
            } else {
                (t as u64).min(MAX_TIMEOUT_MS)
            }
        })
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

pub(crate) fn format_bash_output(bytes: &[u8]) -> String {
    let combined = String::from_utf8_lossy(bytes);
    let char_count = combined.chars().count();
    if char_count > MAX_OUTPUT_CHARS {
        let half = MAX_OUTPUT_CHARS / 2;
        let head_end = combined
            .char_indices()
            .nth(half)
            .map(|(i, _)| i)
            .unwrap_or(combined.len());
        let tail_start = combined
            .char_indices()
            .rev()
            .nth(half - 1)
            .map(|(i, _)| i)
            .unwrap_or(0);
        return format!(
            "{}\n\n... [truncated {} characters] ...\n\n{}",
            &combined[..head_end],
            char_count - MAX_OUTPUT_CHARS,
            &combined[tail_start..]
        );
    }
    if combined.trim().is_empty() {
        "(no output)".to_string()
    } else {
        combined.into_owned()
    }
}

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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

/// `Child::id()` becomes `None` after `wait()` completes, so callers that kill
/// after the shell has exited must pass the PID captured at spawn time.
pub(crate) async fn kill_process_tree(child: &mut Child, pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let taskkill_bin = std::env::var("SystemRoot")
            .map(|r| format!("{r}\\System32\\taskkill.exe"))
            .unwrap_or_else(|_| "taskkill".to_string());
        let mut cmd = tokio::process::Command::new(taskkill_bin);
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.status().await;
    }

    // The shell was spawned in its own process group, so a negative PID kills
    // background children too instead of leaving them holding the output pipe.
    #[cfg(unix)]
    if let Some(pid) = pid {
        unix_kill_pgid(pid);
    }

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

/// Ensures the spawned child process tree is killed if the `run_shell` future is
/// dropped before normal completion (for example via outer `tokio::time::timeout`).
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
                // Detach fallback taskkill so Drop never blocks a Tokio worker thread.
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

#[cfg(any(unix, windows))]
async fn terminate_guarded_tree(child: &mut Child, guard: &mut ProcessTreeGuard) {
    let fallback_pid = guard.try_kill_kernel();
    kill_process_tree(child, fallback_pid).await;
    guard.disarm();
}

struct HeadTailCapture {
    head: Vec<u8>,
    tail: RollingBytes,
    head_cap: usize,
    total_bytes: usize,
}

impl HeadTailCapture {
    fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: RollingBytes::new(tail_cap),
            head_cap,
            total_bytes: 0,
        }
    }

    fn extend(&mut self, chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        if self.head.len() < self.head_cap {
            let take = chunk.len().min(self.head_cap - self.head.len());
            self.head.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                self.tail.extend(&chunk[take..]);
            }
        } else {
            self.tail.extend(chunk);
        }
    }

    fn is_empty(&self) -> bool {
        self.total_bytes == 0
    }

    fn format(&self) -> String {
        let tail_bytes = self.tail.to_vec();
        let kept = self.head.len().saturating_add(tail_bytes.len());
        if self.total_bytes <= kept {
            let mut combined = Vec::with_capacity(kept);
            combined.extend_from_slice(&self.head);
            combined.extend_from_slice(&tail_bytes);
            return format_bash_output(&combined);
        }
        let head_str = String::from_utf8_lossy(&self.head);
        let tail_str = String::from_utf8_lossy(&tail_bytes);
        let half = MAX_OUTPUT_CHARS / 2;
        let head_end = head_str
            .char_indices()
            .nth(half)
            .map(|(i, _)| i)
            .unwrap_or(head_str.len());
        let tail_start = tail_str
            .char_indices()
            .rev()
            .nth(half.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let dropped_bytes = self
            .total_bytes
            .saturating_sub(head_end + (tail_str.len() - tail_start));
        format!(
            "{}\n\n... [truncated {} bytes of middle output] ...\n\n{}",
            &head_str[..head_end],
            dropped_bytes,
            &tail_str[tail_start..]
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShellKind {
    PosixSh,
    /// PowerShell with the resolved System32 binary (PowerShellTool).
    WindowsPowerShell,
    /// Windows PowerShell resolved from System32 for BashTool on Windows.
    WindowsPowerShellCompat,
}

struct CommandSpec {
    program: String,
    args: Vec<String>,
}

fn build_command_spec(kind: ShellKind, command: &str) -> CommandSpec {
    match kind {
        ShellKind::PosixSh => CommandSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command.to_string()],
        },
        ShellKind::WindowsPowerShell | ShellKind::WindowsPowerShellCompat => {
            let full_command = if !command.trim_end().ends_with('`') {
                format!(
                    "{}{}{}",
                    UTF8_OUTPUT_PREFIX, command, WINDOWS_POWERSHELL_EXIT_SUFFIX
                )
            } else {
                format!("{}{}", UTF8_OUTPUT_PREFIX, command)
            };
            let program = std::env::var("SystemRoot")
                .ok()
                .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
                .filter(|p| Path::new(p).is_file())
                .unwrap_or_else(|| "powershell".to_string());
            CommandSpec {
                program,
                args: vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-Command".to_string(),
                    full_command,
                ],
            }
        }
    }
}

pub(crate) struct ShellOutcome {
    pub text: String,
    pub exit_status: Option<std::process::ExitStatus>,
    pub aborted_reason: Option<String>,
}

async fn wait_grace(grace: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(sleep) = grace.as_mut() {
        sleep.await;
    } else {
        pending::<()>().await;
    }
}

/// Shared subprocess engine: detached stdin, platform-correct process-tree
/// termination, UTF-8-safe bounded capture, and a short grace period for
/// background children that keep the output pipe open after the shell exits.
pub(crate) async fn run_shell(
    kind: ShellKind,
    cwd: &Path,
    command: &str,
    timeout_ms: u64,
    cancel: CancellationToken,
    on_update: Option<AgentToolUpdateCallback>,
    sandbox_policy: Option<mehh_sandbox::SandboxPolicy>,
) -> Result<ShellOutcome, String> {
    let spec = build_command_spec(kind, command);
    let (program, args, degraded) = match sandbox_policy {
        Some(policy) => {
            let cwd_owned = cwd.to_path_buf();
            let prog = spec.program;
            let spec_args = spec.args;
            let invocation = tokio::task::spawn_blocking(move || {
                let plan = mehh_sandbox::SandboxPlan::new(policy, cwd_owned);
                mehh_sandbox::wrap(&plan, &prog, &spec_args)
            })
            .await
            .map_err(|e| format!("Sandbox setup task failed: {e}"))?
            .map_err(|e| format!("Sandbox setup failed: {e}"))?;
            let degraded = invocation
                .status
                .degraded_reason()
                .map(str::to_string);
            (invocation.program, invocation.args, degraded)
        }
        None => (spec.program, spec.args, None),
    };
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args);
    cmd.current_dir(cwd);
    // Build scripts and shell commands must not inherit agent credentials.
    for (key, _) in std::env::vars_os() {
        if mehh_sandbox::is_secret_env_key(&key.to_string_lossy()) {
            cmd.env_remove(&key);
        }
    }
    // Never let a child consume the agent's stdin (CLI interactive / MCP stdio).
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn command: {e}"))?;
    // Capture PID / assign Windows Job Object immediately before any wait():
    // tokio clears id() and raw_handle() once the child is reaped.
    #[cfg(any(unix, windows))]
    let mut pg_guard = ProcessTreeGuard::new(&child);
    #[cfg(not(any(unix, windows)))]
    let spawn_pid = child.id();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let out_handle = child.stdout.take().map(|mut pipe| {
        let tx_out = tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; CHUNK_READ_SIZE];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if tx_out.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        })
    });
    let err_handle = child.stderr.take().map(|mut pipe| {
        let tx_err = tx.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; CHUNK_READ_SIZE];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if tx_err.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        })
    });
    drop(tx);

    let half_buf = MAX_STREAMING_PREVIEW_BYTES / 2;
    let mut captured = HeadTailCapture::new(half_buf, half_buf);
    let mut last_update = Instant::now()
        .checked_sub(UPDATE_THROTTLE)
        .unwrap_or_else(Instant::now);
    let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(deadline);

    let mut exit_status = None;
    let mut aborted_reason = None;
    let mut rx_closed = false;
    let mut grace: Option<Pin<Box<Sleep>>> = None;
    let mut grace_expired = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled(), if aborted_reason.is_none() => {
                #[cfg(any(unix, windows))]
                terminate_guarded_tree(&mut child, &mut pg_guard).await;
                #[cfg(not(any(unix, windows)))]
                kill_process_tree(&mut child, spawn_pid).await;
                aborted_reason = Some("Command aborted".to_string());
                break;
            }
            _ = &mut deadline, if aborted_reason.is_none() && exit_status.is_none() => {
                #[cfg(any(unix, windows))]
                terminate_guarded_tree(&mut child, &mut pg_guard).await;
                #[cfg(not(any(unix, windows)))]
                kill_process_tree(&mut child, spawn_pid).await;
                aborted_reason = Some(format!("Command timed out after {timeout_ms}ms"));
                break;
            }
            maybe_chunk = rx.recv(), if !rx_closed => {
                match maybe_chunk {
                    Some(chunk) => {
                        captured.extend(&chunk);
                        if let Some(cb) = on_update.as_ref() {
                            if last_update.elapsed() >= UPDATE_THROTTLE {
                                cb(mehh_agent::AgentToolResult::text(captured.format()));
                                last_update = Instant::now();
                            }
                        }
                    }
                    None => rx_closed = true,
                }
            }
            status = child.wait(), if exit_status.is_none() => {
                match status {
                    Ok(s) => {
                        exit_status = Some(s);
                        grace = Some(Box::pin(tokio::time::sleep(PIPE_GRACE)));
                    }
                    Err(e) => return Err(format!("Failed to wait for command: {e}")),
                }
            }
            _ = wait_grace(&mut grace), if exit_status.is_some() && !rx_closed => {
                // The shell exited but a background child still holds the
                // pipe. Stop waiting and reap the whole process group.
                #[cfg(any(unix, windows))]
                terminate_guarded_tree(&mut child, &mut pg_guard).await;
                #[cfg(not(any(unix, windows)))]
                kill_process_tree(&mut child, spawn_pid).await;
                grace_expired = true;
                break;
            }
        }

        if rx_closed && exit_status.is_some() {
            break;
        }
    }

    // Drain chunks already buffered before break (timeout, cancel, or pipe grace)
    // so partial output emitted just before termination is never lost.
    while let Ok(chunk) = rx.try_recv() {
        captured.extend(&chunk);
    }

    let mut text = captured.format();
    if let Some(reason) = degraded {
        text = format!("[sandbox degraded: {reason}]\n{text}");
    }

    if aborted_reason.is_some() || grace_expired {
        if let Some(cb) = on_update.as_ref() {
            if !captured.is_empty() {
                cb(mehh_agent::AgentToolResult::text(text.clone()));
            }
        }
        if let Some(handle) = out_handle {
            handle.abort();
        }
        if let Some(handle) = err_handle {
            handle.abort();
        }
    } else {
        if let Some(handle) = out_handle {
            let _ = handle.await;
        }
        if let Some(handle) = err_handle {
            let _ = handle.await;
        }
    }

    #[cfg(any(unix, windows))]
    pg_guard.disarm();

    Ok(ShellOutcome {
        text,
        exit_status,
        aborted_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timeout_is_always_milliseconds() {
        let params = json!({ "timeout": 500 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), 500);
        let params = json!({ "timeoutMs": 2000 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), 2000);
    }

    #[test]
    fn parse_timeout_ms_ignores_null_alias() {
        let params = json!({ "timeoutMs": null, "timeout": 750 })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(parse_timeout_ms(&params), 750);
    }

    #[test]
    fn zero_timeout_falls_back_to_default() {
        let params = json!({ "timeout": 0 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
        let params = json!({}).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn head_tail_capture_preserves_early_errors_past_256kib() {
        let half = MAX_STREAMING_PREVIEW_BYTES / 2;
        let mut cap = HeadTailCapture::new(half, half);
        cap.extend(b"EARLY_COMPILER_ERROR: missing semicolon\n");
        let filler = vec![b'x'; 300 * 1024];
        cap.extend(&filler);
        cap.extend(b"\nLATE_SUMMARY: build failed\n");

        let rendered = cap.format();
        assert!(
            rendered.contains("EARLY_COMPILER_ERROR"),
            "head error must survive >256 KiB output"
        );
        assert!(
            rendered.contains("LATE_SUMMARY"),
            "tail summary must survive >256 KiB output"
        );
        assert!(
            rendered.contains("truncated"),
            "truncation notice must appear"
        );
    }

    #[test]
    fn head_tail_capture_reports_exact_omitted_bytes() {
        let half = MAX_STREAMING_PREVIEW_BYTES / 2;
        let mut cap = HeadTailCapture::new(half, half);
        let total = 300 * 1024;
        cap.extend(&vec![b'a'; total]);

        let rendered = cap.format();
        // Output displays half MAX_OUTPUT_CHARS from head and half from tail.
        let expected_omitted = total - MAX_OUTPUT_CHARS;
        let expected_notice = format!("[truncated {expected_omitted} bytes of middle output]");
        assert!(
            rendered.contains(&expected_notice),
            "expected {expected_notice:?}, got notice in: {:?}",
            rendered
                .lines()
                .find(|l| l.contains("truncated"))
                .unwrap_or("")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_compat_resolves_system32_binary_when_present() {
        let spec = build_command_spec(ShellKind::WindowsPowerShellCompat, "Write-Output ok");
        if let Ok(root) = std::env::var("SystemRoot") {
            let expected = format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe");
            if Path::new(&expected).is_file() {
                assert_eq!(spec.program, expected);
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_process_tree_guard_kills_child_tree_on_drop() {
        let mut child = tokio::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >nul"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd child");
        let pid = child.id().expect("child pid");
        {
            let _guard = ProcessTreeGuard {
                pid: Some(pid),
                job: None,
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("process tree guard must terminate child promptly on drop")
            .expect("wait on killed child");
        assert!(!status.success(), "killed child should not exit cleanly");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_handle_kills_grandchild_after_parent_exits() {
        use std::os::windows::process::CommandExt;
        use tokio::io::AsyncReadExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = tokio::process::Command::new("cmd.exe");
        cmd.args(["/C", "start /B ping -n 30 127.0.0.1 & echo READY"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd.spawn().expect("spawn parent cmd");
        let guard = ProcessTreeGuard::new(&child);
        assert!(
            guard.job.is_some(),
            "WindowsJobHandle::assign_process must succeed on spawned child"
        );

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut seen = Vec::new();
        let mut tmp = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(3), async {
            while !seen.windows(5).any(|w| w == b"READY") {
                let n = stdout.read(&mut tmp).await.expect("read stdout");
                assert!(n > 0, "stdout closed before READY");
                seen.extend_from_slice(&tmp[..n]);
            }
        })
        .await
        .expect("parent shell must emit READY");

        // Parent cmd.exe exits immediately while grandchild ping.exe stays alive holding stdout.
        let parent_status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("parent shell must exit promptly")
            .expect("wait on parent shell");
        assert!(parent_status.success());

        // Dropping ProcessTreeGuard must terminate the surviving grandchild via Job Object,
        // closing the inherited stdout pipe immediately.
        drop(guard);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stdout.read_to_end(&mut rest))
            .await
            .expect("Job Object drop must terminate grandchild and close pipe promptly")
            .expect("read stdout to EOF");
    }
}
