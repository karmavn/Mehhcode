use std::path::PathBuf;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::PathPolicy;
use crate::core::shell::{parse_timeout_ms, run_shell, ShellKind};

#[derive(Debug, Clone)]
pub struct PowerShellTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl PowerShellTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn name(&self) -> &str {
        "powershell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command in the current working directory. Returns combined stdout and stderr. Output is truncated if it exceeds 30,000 characters. The timeout parameter is in milliseconds."
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
                    "description": "Timeout in milliseconds (default: 120000; alias: timeoutMs; max: 600000)"
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "Explicit timeout in milliseconds (alias of timeout)"
                }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(false)
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
        let timeout_ms = parse_timeout_ms(&params);

        let outcome =
            run_shell(ShellKind::WindowsPowerShell, &self.cwd, command, timeout_ms, cancel, on_update)
                .await?;

        if let Some(reason) = outcome.aborted_reason {
            return Err(if outcome.text == "(no output)" {
                reason
            } else {
                format!("{reason}:\nPartial output:\n{}", outcome.text)
            });
        }

        match outcome.exit_status {
            Some(status) if !status.success() => Err(format!(
                "Command exited with code {}:\n{}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "terminated by signal".to_string()),
                outcome.text
            )),
            Some(_) => Ok(AgentToolResult::text(outcome.text)),
            None => Err("Command finished without exit status".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::ToolResultContentBlock;

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
