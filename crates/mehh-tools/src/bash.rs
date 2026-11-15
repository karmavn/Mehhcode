use std::path::PathBuf;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const CHUNK_READ_SIZE: usize = 4096;
const MAX_STREAMING_PREVIEW_BYTES: usize = 256 * 1024;
const UPDATE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_OUTPUT_CHARS: usize = 30_000;

fn append_rolling(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if chunk.len() >= cap {
        buf.clear();
        buf.extend_from_slice(&chunk[chunk.len() - cap..]);
        return;
    }
    let overflow = buf.len().saturating_add(chunk.len()).saturating_sub(cap);
    if overflow > 0 {
        buf.drain(..overflow);
    }
    buf.extend_from_slice(chunk);
}

fn format_bash_output(bytes: &[u8]) -> String {
    let combined = String::from_utf8_lossy(bytes);
    if combined.len() > MAX_OUTPUT_CHARS {
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
    }
    if combined.trim().is_empty() {
        "(no output)".to_string()
    } else {
        combined.into_owned()
    }
}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
}

#[derive(Debug, Clone)]
pub struct BashTool {
    pub cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the current working directory. Returns combined stdout and stderr. Output is truncated if it exceeds 30,000 characters."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (or seconds if <= 1000, default: 120000 ms)"
                }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Command aborted".to_string());
        }

        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'command'".to_string())?;

        let timeout_ms = params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .map(|t| if t <= 1000 { t * 1000 } else { t })
            .unwrap_or(120_000);

        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", command]);
            c
        };

        cmd.current_dir(&self.cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn command: {}", e))?;

        let timeout_duration = std::time::Duration::from_millis(timeout_ms);
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

        let mut captured = Vec::new();
        let mut last_update = std::time::Instant::now()
            .checked_sub(UPDATE_THROTTLE)
            .unwrap_or_else(std::time::Instant::now);
        let deadline = tokio::time::sleep(timeout_duration);
        tokio::pin!(deadline);

        let mut exit_status: Option<std::process::ExitStatus> = None;
        let mut aborted_reason: Option<String> = None;
        let mut rx_closed = false;

        while !rx_closed || exit_status.is_none() {
            tokio::select! {
                _ = cancel.cancelled(), if aborted_reason.is_none() => {
                    kill_process_tree(&mut child).await;
                    aborted_reason = Some("Command aborted".to_string());
                    break;
                }
                _ = &mut deadline, if aborted_reason.is_none() => {
                    kill_process_tree(&mut child).await;
                    aborted_reason = Some(format!("Command timed out after {}ms", timeout_ms));
                    break;
                }
                maybe_chunk = rx.recv(), if !rx_closed => {
                    match maybe_chunk {
                        Some(chunk) => {
                            append_rolling(&mut captured, &chunk, MAX_STREAMING_PREVIEW_BYTES);
                            if let Some(cb) = on_update.as_ref() {
                                if last_update.elapsed() >= UPDATE_THROTTLE {
                                    cb(AgentToolResult::text(format_bash_output(&captured)));
                                    last_update = std::time::Instant::now();
                                }
                            }
                        }
                        None => {
                            rx_closed = true;
                        }
                    }
                }
                status_res = child.wait(), if exit_status.is_none() => {
                    match status_res {
                        Ok(s) => exit_status = Some(s),
                        Err(e) => return Err(format!("Failed to wait for command: {}", e)),
                    }
                }
            }
        }

        if let Some(reason) = aborted_reason {
            while let Ok(chunk) = rx.try_recv() {
                append_rolling(&mut captured, &chunk, MAX_STREAMING_PREVIEW_BYTES);
            }
            if let Some(h) = out_handle {
                h.abort();
            }
            if let Some(h) = err_handle {
                h.abort();
            }
            let partial = String::from_utf8_lossy(&captured);
            if partial.trim().is_empty() {
                return Err(reason);
            }
            return Err(format!("{reason}:\n{}", format_bash_output(&captured)));
        }

        if let Some(h) = out_handle {
            let _ = h.await;
        }
        if let Some(h) = err_handle {
            let _ = h.await;
        }

        let status = exit_status.ok_or_else(|| "Command finished without exit status".to_string())?;
        let formatted_output = format_bash_output(&captured);

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            Err(format!("Command exited with code {}:\n{}", code, formatted_output))
        } else {
            Ok(AgentToolResult::text(formatted_output))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_bash_streams_updates_and_preserves_partial_output_on_timeout() {
        let dir = crate::TempTestDir::new("bash_stream");
        let tool = BashTool::new(dir.path().to_path_buf());
        let updates: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let updates_clone = updates.clone();
        let cb: AgentToolUpdateCallback = Arc::new(move |res| {
            if let Some(mehh_ai::ToolResultContentBlock::Text(t)) = res.content.first() {
                updates_clone.lock().unwrap().push(t.text.clone());
            }
        });

        let cmd = if cfg!(windows) {
            "Write-Output 'partial_marker'; Start-Sleep -Seconds 5"
        } else {
            "echo partial_marker; sleep 5"
        };
        let params = json!({
            "command": cmd,
            "timeout": 1200
        })
        .as_object()
        .unwrap()
        .clone();

        let err = tool
            .execute("tc", params, CancellationToken::new(), Some(cb))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "unexpected err: {err}");
        assert!(err.contains("partial_marker"), "missing partial output in: {err}");
        assert!(!updates.lock().unwrap().is_empty());
    }
}
