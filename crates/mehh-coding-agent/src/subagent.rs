//! Depth-bounded, multi-mode subagent tool for delegated tasks and parallel execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use mehh_agent::{
    run_agent_loop, AgentEvent, AgentLoopConfig, AgentTool, AgentToolResult,
    AgentToolUpdateCallback,
};
use mehh_ai::{now_ms, LlmProvider, Message, Model, StopReason, UserContent, UserMessage};

use crate::tool_inspection::ToolInspectionManager;

/// Maximum nested subagent recursion depth.
pub const MAX_SUBAGENT_DEPTH: usize = 2;

/// Maximum assistant turns allowed per subagent invocation.
pub const SUBAGENT_MAX_TURNS: usize = 20;

/// Maximum wall-clock timeout for a subagent invocation in seconds.
pub const SUBAGENT_TIMEOUT_SECS: u64 = 180;

/// Runs a delegated subagent in an isolated context with bounded turns and mode-specific tools.
pub struct SubagentTool {
    pub cwd: PathBuf,
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub api_key: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub depth: usize,
    pub inspection_manager: Option<Arc<ToolInspectionManager>>,
    pub approval_handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
}

impl SubagentTool {
    pub fn new(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        api_key: Option<String>,
        tools: Vec<Arc<dyn AgentTool>>,
        depth: usize,
    ) -> Self {
        Self {
            cwd,
            model,
            provider,
            api_key,
            tools,
            depth,
            inspection_manager: None,
            approval_handler: None,
        }
    }

    pub fn with_inspection_manager(mut self, mgr: Arc<ToolInspectionManager>) -> Self {
        self.inspection_manager = Some(mgr);
        self
    }

    pub fn with_approval_handler(
        mut self,
        handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
    ) -> Self {
        self.approval_handler = handler;
        self
    }

    /// Parameter schema defining modes and parallel batch tasks.
    pub fn parameters_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Self-contained task description for single-task subagent."
                },
                "mode": {
                    "type": "string",
                    "enum": ["research", "coding", "review"],
                    "description": "Execution mode: 'research' (read-only code & doc search), 'coding' (full editing & tests, recursion disabled), 'review' (read-only code review)."
                },
                "tasks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of independent tasks to execute concurrently (up to 4 in parallel)."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system instructions for the subagent."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Optional turn budget override (capped at 20)."
                }
            }
        })
    }

    /// Filters available tools for the specified execution mode.
    /// Strictly excludes recursive tools ('subagent', 'codemode', 'todo') in coding mode.
    pub fn filter_tools_for_mode(&self, mode: &str) -> Vec<Arc<dyn AgentTool>> {
        match mode {
            "coding" => self
                .tools
                .iter()
                .filter(|t| !matches!(t.name(), "subagent" | "codemode" | "todo"))
                .cloned()
                .collect(),
            "review" | "research" => self
                .tools
                .iter()
                .filter(|t| matches!(t.name(), "read" | "grep" | "find" | "ls" | "analyze"))
                .cloned()
                .collect(),
            _ => self.filter_tools_for_mode("research"),
        }
    }

    async fn run_child_prompt(
        &self,
        task_prompt: &str,
        mode: &str,
        custom_sys: Option<&str>,
        max_turns: usize,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<String, String> {
        let child_tools = self.filter_tools_for_mode(mode);
        let default_sys = match mode {
            "coding" => format!(
                "You are an autonomous coding worker operating in {}. Read files, write code, edit, and run verification commands directly. Be concise and return what was changed.",
                self.cwd.display()
            ),
            "review" => format!(
                "You are an expert code reviewer analyzing {}. Read the code thoroughly and report concrete bugs, design issues, or verification findings with exact file line numbers.",
                self.cwd.display()
            ),
            _ => format!(
                "You are a focused read-only research subagent working in {}. Investigate the request and report concise findings with exact file paths.",
                self.cwd.display()
            ),
        };

        let mut config = AgentLoopConfig::new(self.model.clone(), Arc::clone(&self.provider));
        config.cancel = cancel.child_token();
        config.max_turns = Some(max_turns);
        config.tools = child_tools;
        config.system_prompt = Some(custom_sys.map(str::to_string).unwrap_or(default_sys));

        if let Some(ref mgr) = self.inspection_manager {
            let mgr = mgr.clone();
            let session_cwd = self.cwd.clone();
            let approval_h = self.approval_handler.clone();
            config.before_tool_call = Some(Arc::new(move |before_ctx, _cancel| {
                let mgr = mgr.clone();
                let session_cwd = session_cwd.clone();
                let approval_h = approval_h.clone();
                Box::pin(async move {
                    let tool_name = before_ctx.tool_call.name.clone();
                    let tool_call_id = before_ctx.tool_call.id.clone();
                    let is_read_only = before_ctx
                        .context
                        .tools
                        .as_ref()
                        .and_then(|tools| tools.iter().find(|t| t.name() == tool_name))
                        .and_then(|t| t.is_read_only());

                    let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                        tool_call_id,
                        tool_name,
                        args: before_ctx.args,
                        is_read_only,
                        cwd: session_cwd,
                    };
                    mgr.inspect_and_authorize(&inspect_ctx, approval_h.as_deref()).await
                })
            }));
        }

        if let Some(ref key) = self.api_key {
            config.stream_options.api_key = Some(key.clone());
        } else if self.model.provider != "faux" && self.model.api != "faux" {
            if let Ok(Some(key)) =
                crate::auth::resolve_api_key_async(&self.model.provider, None, None).await
            {
                config.stream_options.api_key = Some(key);
            }
        }

        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(task_prompt.to_string()),
            timestamp: now_ms(),
        });

        let event_listener = on_update.map(|cb| {
            let mode_str = mode.to_string();
            let listener: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
                match event {
                    AgentEvent::ToolExecutionStart { tool_name, .. } => {
                        cb(AgentToolResult::text(format!(
                            "[subagent:{mode_str}] Executing tool {}",
                            tool_name
                        )));
                    }
                    AgentEvent::ToolExecutionEnd { is_error, .. } => {
                        let status = if is_error { "failed" } else { "completed" };
                        cb(AgentToolResult::text(format!(
                            "[subagent:{mode_str}] Tool {}",
                            status
                        )));
                    }
                    _ => {}
                }
            });
            listener
        });

        let messages = tokio::time::timeout(
            Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
            run_agent_loop(vec![user_msg], Vec::new(), config, event_listener),
        )
        .await
        .map_err(|_| format!("Subagent timed out after {SUBAGENT_TIMEOUT_SECS}s"))?;

        let final_ast = messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        });

        match final_ast {
            Some(a) if a.stop_reason == StopReason::Error => Err(a
                .error_message
                .clone()
                .unwrap_or_else(|| "Subagent model call failed".to_string())),
            Some(a) if a.stop_reason == StopReason::Aborted => {
                Err("Subagent execution was aborted".to_string())
            }
            Some(a) => Ok(a.text_content()),
            None => Err("Subagent produced no assistant response".to_string()),
        }
    }
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Delegate tasks to an isolated subagent. Supports 'research', 'coding', and 'review' modes, plus parallel 'tasks' batch execution."
    }

    fn parameters(&self) -> Value {
        Self::parameters_schema()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!(
                "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached"
            ));
        }

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("research");

        let custom_sys = params.get("system_prompt").and_then(|v| v.as_str());

        let max_turns = params
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, SUBAGENT_MAX_TURNS))
            .unwrap_or(SUBAGENT_MAX_TURNS);

        // Check if parallel batch of tasks was provided
        if let Some(tasks_arr) = params.get("tasks").and_then(|v| v.as_array()) {
            let task_prompts: Vec<String> = tasks_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                .collect();

            if !task_prompts.is_empty() {
                let semaphore = Arc::new(Semaphore::new(4));
                let mut handles = Vec::new();

                for (idx, task_p) in task_prompts.iter().enumerate() {
                    let sem = semaphore.clone();
                    let task_p = task_p.clone();
                    let cancel_token = cancel.clone();
                    let on_update_child = on_update.clone();
                    let mode_str = mode.to_string();
                    let custom_sys_owned = custom_sys.map(str::to_string);
                    let sub = Self {
                        cwd: self.cwd.clone(),
                        model: self.model.clone(),
                        provider: self.provider.clone(),
                        api_key: self.api_key.clone(),
                        tools: self.tools.clone(),
                        depth: self.depth + 1,
                        inspection_manager: self.inspection_manager.clone(),
                        approval_handler: self.approval_handler.clone(),
                    };

                    handles.push(tokio::spawn(async move {
                        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
                        if let Some(ref cb) = on_update_child {
                            cb(AgentToolResult::text(format!(
                                "[subagent:{mode_str}] Starting task #{}",
                                idx + 1
                            )));
                        }
                        sub.run_child_prompt(
                            &task_p,
                            &mode_str,
                            custom_sys_owned.as_deref(),
                            max_turns,
                            cancel_token,
                            on_update_child,
                        )
                        .await
                    }));
                }

                let mut report = String::new();
                for (idx, h) in handles.into_iter().enumerate() {
                    let task_name = &task_prompts[idx];
                    report.push_str(&format!("=== Task #{}: {} ===\n", idx + 1, task_name));
                    match h.await {
                        Ok(Ok(out)) => report.push_str(&out),
                        Ok(Err(e)) => report.push_str(&format!("Error: {}", e)),
                        Err(join_err) => report.push_str(&format!("Join error: {}", join_err)),
                    }
                    report.push_str("\n\n");
                }

                return Ok(AgentToolResult::text(report.trim().to_string()));
            }
        }

        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'prompt' or 'tasks' parameter".to_string())?;

        let child_sub = Self {
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api_key: self.api_key.clone(),
            tools: self.tools.clone(),
            depth: self.depth + 1,
            inspection_manager: self.inspection_manager.clone(),
            approval_handler: self.approval_handler.clone(),
        };

        let result_text = child_sub
            .run_child_prompt(prompt, mode, custom_sys, max_turns, cancel, on_update)
            .await?;

        Ok(AgentToolResult::text(result_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};

    #[tokio::test]
    async fn test_subagent_runs_isolated_prompt_and_respects_max_depth() {
        let faux = Arc::new(FauxProvider::new());
        faux.add_response(faux_assistant_message("Found 3 matching files."));

        let cwd = std::env::temp_dir();
        let tools = mehh_tools::create_coding_tools(cwd.clone());
        let sub = SubagentTool::new(cwd.clone(), faux_model(), faux.clone(), None, tools.clone(), 0);
        assert!(sub.filter_tools_for_mode("research").iter().all(|t| matches!(t.name(), "read" | "grep" | "find" | "ls" | "analyze")));

        let mut params = Map::new();
        params.insert("prompt".to_string(), Value::String("Inspect crates".to_string()));
        let res = sub
            .execute("tc_sub_1", params.clone(), CancellationToken::new(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!("Expected text output"),
        };
        assert_eq!(text, "Found 3 matching files.");

        // Depth at MAX_SUBAGENT_DEPTH fails closed
        let deep = SubagentTool::new(cwd, faux_model(), faux, None, tools, MAX_SUBAGENT_DEPTH);
        let err = deep
            .execute("tc_sub_2", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("Maximum subagent nesting depth"));
    }
}
