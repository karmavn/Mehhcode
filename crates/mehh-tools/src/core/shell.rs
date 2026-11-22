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
        .or_else(|| params.get("timeout"))
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

/// `Child::id()` becomes `None` after `wait()` completes, so callers that kill
/// after the shell has exited must pass the PID captured at spawn time.
pub(crate) async fn kill_process_tree(child: &mut Child, pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let taskkill_bin = std::env::var("SystemRoot")
            .map(|r| format!("{r}\\System32\\taskkill.exe"))
            .unwrap_or_else(|_| "taskkill".to_string());
        let _ = tokio::process::Command::new(taskkill_bin)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    // The shell was spawned in its own process group, so a negative PID kills
    // background children too instead of leaving them holding the output pipe.
    #[cfg(unix)]
    if let Some(pid) = pid {
        let script = format!("kill -KILL -{pid}");
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::process::Command::new("sh")
                .args(["-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        })
        .await;
    }

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ShellKind {
    PosixSh,
    /// PowerShell with the resolved System32 binary (PowerShellTool).
    WindowsPowerShell,
    /// Plain `powershell`, used by BashTool on Windows for compatibility.
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
            let program = match kind {
                ShellKind::WindowsPowerShell => std::env::var("SystemRoot")
                    .ok()
                    .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
                    .filter(|p| Path::new(p).is_file())
                    .unwrap_or_else(|| "powershell".to_string()),
                _ => "powershell".to_string(),
            };
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
    sandbox: Option<&mehh_sandbox::SandboxPlan>,
) -> Result<ShellOutcome, String> {
    let spec = build_command_spec(kind, command);
    let (program, args, degraded) = match sandbox {
        Some(plan) => {
            let invocation = mehh_sandbox::wrap(plan, &spec.program, &spec.args)
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
    // Capture before any wait(): tokio clears id() once the child is reaped.
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

    let mut captured = RollingBytes::new(MAX_STREAMING_PREVIEW_BYTES);
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
                kill_process_tree(&mut child, spawn_pid).await;
                aborted_reason = Some("Command aborted".to_string());
                break;
            }
            _ = &mut deadline, if aborted_reason.is_none() && exit_status.is_none() => {
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
                                cb(mehh_agent::AgentToolResult::text(format_bash_output(
                                    &captured.to_vec(),
                                )));
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
                kill_process_tree(&mut child, spawn_pid).await;
                grace_expired = true;
                break;
            }
        }

        if rx_closed && exit_status.is_some() {
            break;
        }
    }

    let mut text = format_bash_output(&captured.to_vec());
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
    fn zero_timeout_falls_back_to_default() {
        let params = json!({ "timeout": 0 }).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
        let params = json!({}).as_object().unwrap().clone();
        assert_eq!(parse_timeout_ms(&params), DEFAULT_TIMEOUT_MS);
    }
}
