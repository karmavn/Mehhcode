use std::path::PathBuf;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::bash::{
    append_rolling, format_bash_output, kill_process_tree, CHUNK_READ_SIZE,
    MAX_STREAMING_PREVIEW_BYTES, UPDATE_THROTTLE, UTF8_OUTPUT_PREFIX,
    WINDOWS_POWERSHELL_EXIT_SUFFIX,
};

#[derive(Debug, Clone)]
pub struct PowerShellTool {
    pub cwd: PathBuf,
}

impl PowerShellTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn name(&self) -> &str {
        "powershell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command in the current working directory. Returns combined stdout and stderr. Output is truncated if it exceeds 30,000 characters."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "PowerShell command to execute"
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

        if !cfg!(windows) {
            return Err("PowerShell is not supported on this platform".to_string());
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

        let full_command = if !command.ends_with('`') {
            format!("{}{}{}", UTF8_OUTPUT_PREFIX, command, WINDOWS_POWERSHELL_EXIT_SUFFIX)
        } else {
            format!("{}{}", UTF8_OUTPUT_PREFIX, command)
        };

        let powershell_bin = if cfg!(windows) {
            std::env::var("SystemRoot")
                .ok()
                .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
                .filter(|p| std::path::Path::new(p).is_file())
                .unwrap_or_else(|| "powershell".to_string())
        } else {
            "powershell".to_string()
        };

        let mut cmd = tokio::process::Command::new(powershell_bin);
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &full_command]);
        cmd.current_dir(&self.cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn PowerShell command: {}", e))?;

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

        let mut output_buf = Vec::new();
        let mut last_update = std::time::Instant::now();

        let wait_result = async {
            let sleep_future = tokio::time::sleep(timeout_duration);
            tokio::pin!(sleep_future);

            let mut process_exited = false;
            let mut streams_closed = false;
            let mut exit_status = None;
            let mut aborted_reason = None;

            loop {
                tokio::select! {
                    _ = &mut sleep_future, if aborted_reason.is_none() => {
                        kill_process_tree(&mut child).await;
                        aborted_reason = Some(format!("Command timed out after {}ms", timeout_ms));
                        break;
                    }
                    _ = cancel.cancelled(), if aborted_reason.is_none() => {
                        kill_process_tree(&mut child).await;
                        aborted_reason = Some("Command aborted".to_string());
                        break;
                    }
                    maybe_bytes = rx.recv(), if !streams_closed => {
                        match maybe_bytes {
                            Some(chunk) => {
                                append_rolling(&mut output_buf, &chunk, MAX_STREAMING_PREVIEW_BYTES);
                                if on_update.is_some() && last_update.elapsed() >= UPDATE_THROTTLE {
                                    if let Some(ref cb) = on_update {
                                        let text = format_bash_output(&output_buf);
                                        cb(AgentToolResult::text(text));
                                    }
                                    last_update = std::time::Instant::now();
                                }
                            }
                            None => {
                                streams_closed = true;
                            }
                        }
                    }
                    status = child.wait(), if !process_exited => {
                        process_exited = true;
                        exit_status = Some(status.map_err(|e| format!("Failed to wait for command: {}", e))?);
                    }
                }

                if process_exited && streams_closed {
                    break;
                }
            }

            if let Some(reason) = aborted_reason {
                let mut drained_any = false;
                while let Ok(chunk) = rx.try_recv() {
                    append_rolling(&mut output_buf, &chunk, MAX_STREAMING_PREVIEW_BYTES);
                    drained_any = true;
                }
                if drained_any {
                    if let Some(ref cb) = on_update {
                        let text = format_bash_output(&output_buf);
                        cb(AgentToolResult::text(text));
                    }
                }
                if let Some(handle) = out_handle {
                    handle.abort();
                }
                if let Some(handle) = err_handle {
                    handle.abort();
                }
                let output_text = format_bash_output(&output_buf);
                return Err(if output_text == "(no output)" {
                    reason
                } else {
                    format!("{reason}:\nPartial output:\n{output_text}")
                });
            }

            if let Some(handle) = out_handle {
                let _ = handle.await;
            }
            if let Some(handle) = err_handle {
                let _ = handle.await;
            }

            let status = exit_status.ok_or_else(|| "Process should have exited".to_string())?;
            Ok(status)
        }
        .await;

        match wait_result {
            Ok(status) => {
                let output_text = format_bash_output(&output_buf);
                if status.success() {
                    Ok(AgentToolResult::text(output_text))
                } else {
                    let code_str = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "terminated by signal".to_string());
                    Err(format!(
                        "Command exited with code {}:\n{}",
                        code_str, output_text
                    ))
                }
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::ToolResultContentBlock;
    use serde_json::json;

    #[tokio::test]
    async fn powershell_tool_executes_on_windows_and_handles_native_stderr() {
        let tool = PowerShellTool::new(PathBuf::from("."));
        let cancel = CancellationToken::new();

        if cfg!(windows) {
            let params = json!({
                "command": "Write-Output 'UTF8-Test-Chào-Bạn'"
            })
            .as_object()
            .unwrap()
            .clone();

            let res = tool.execute("call-1", params, cancel.clone(), None).await.unwrap();
            let text = match &res.content[0] {
                ToolResultContentBlock::Text(t) => &t.text,
                _ => panic!("expected text"),
            };
            assert!(text.contains("UTF8-Test-Chào-Bạn"));

            let params2 = json!({
                "command": "cmd.exe /c 'echo fake error 1>&2'"
            })
            .as_object()
            .unwrap()
            .clone();
            let res2 = tool.execute("call-2", params2, cancel.clone(), None).await;
            assert!(res2.is_ok(), "Native stderr with exit code 0 must succeed");
        } else {
            let params = json!({
                "command": "Get-Process"
            })
            .as_object()
            .unwrap()
            .clone();

            let res = tool.execute("call-1", params, cancel, None).await;
            assert!(res.is_err());
            assert!(res.unwrap_err().contains("not supported"));
        }
    }
}
