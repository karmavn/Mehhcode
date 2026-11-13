use std::path::PathBuf;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

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
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
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

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        let exit_status = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err("Command aborted".to_string());
            }
            _ = tokio::time::sleep(timeout_duration) => {
                let _ = child.kill().await;
                return Err(format!("Command timed out after {}ms", timeout_ms));
            }
            status_res = async {
                let out_fut = async {
                    if let Some(mut pipe) = stdout_pipe {
                        use tokio::io::AsyncReadExt;
                        let _ = pipe.read_to_end(&mut stdout_buf).await;
                    }
                };
                let err_fut = async {
                    if let Some(mut pipe) = stderr_pipe {
                        use tokio::io::AsyncReadExt;
                        let _ = pipe.read_to_end(&mut stderr_buf).await;
                    }
                };
                tokio::join!(out_fut, err_fut, child.wait()).2
            } => {
                status_res.map_err(|e| format!("Failed to wait for command: {}", e))?
            }
        };

        let stdout_str = String::from_utf8_lossy(&stdout_buf);
        let stderr_str = String::from_utf8_lossy(&stderr_buf);
        let combined = format!("{}{}", stdout_str, stderr_str);

        let char_count = combined.chars().count();
        let formatted_output = if char_count > 30_000 {
            let head: String = combined.chars().take(15_000).collect();
            let tail: String = combined.chars().skip(char_count - 15_000).collect();
            let notice = format!("\n\n... [truncated {} characters] ...\n\n", char_count - 30_000);
            format!("{}{}{}", head, notice, tail)
        } else if combined.trim().is_empty() {
            "(no output)".to_string()
        } else {
            combined
        };

        if !exit_status.success() {
            let code = exit_status.code().unwrap_or(-1);
            Err(format!("Command exited with code {}:\n{}", code, formatted_output))
        } else {
            Ok(AgentToolResult::text(formatted_output))
        }
    }
}
