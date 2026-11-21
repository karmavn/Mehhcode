use std::path::PathBuf;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::PathPolicy;
use crate::core::shell::{parse_timeout_ms, run_shell, ShellKind};

#[derive(Debug, Clone)]
pub struct BashTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl BashTool {
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
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the current working directory. Returns combined stdout and stderr. Output is truncated if it exceeds 30,000 characters. The timeout parameter is in milliseconds."
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

        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'command'".to_string())?;
        let timeout_ms = parse_timeout_ms(&params);
        let kind = if cfg!(windows) {
            ShellKind::WindowsPowerShellCompat
        } else {
            ShellKind::PosixSh
        };

        let outcome = run_shell(kind, &self.cwd, command, timeout_ms, cancel, on_update).await?;

        if let Some(reason) = outcome.aborted_reason {
            return Err(if outcome.text == "(no output)" {
                reason
            } else {
                format!("{reason}:\n{}", outcome.text)
            });
        }

        match outcome.exit_status {
            Some(status) if !status.success() => Err(format!(
                "Command exited with code {}:\n{}",
                status.code().unwrap_or(-1),
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
            "Write-Output 'partial_marker'; Start-Sleep -Seconds 10"
        } else {
            "echo partial_marker; sleep 10"
        };
        let params = json!({
            "command": cmd,
            "timeout": 2000
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
