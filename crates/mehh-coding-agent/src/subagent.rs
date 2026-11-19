//! Depth-bounded, multi-mode subagent tool for delegated tasks and parallel execution.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use mehh_agent::{
    run_agent_loop, AgentEvent, AgentLoopConfig, AgentTool, AgentToolResult,
    AgentToolUpdateCallback, QueueMode,
};
use mehh_ai::{now_ms, LlmProvider, Message, Model, StopReason, UserContent, UserMessage};

use crate::subagent_lifecycle::{
    agent_store_dir, classify_failure, derive_state, new_agent_id, new_manifest, write_manifest,
    SubagentEvent, SubagentEventKind, SubagentFailureClass, SubagentManifest, SubagentStatus,
};
use crate::subagent_control::{global_control, SubagentControl};
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
    pub control: Option<Arc<SubagentControl>>,
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
            control: None,
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

    pub fn with_control(mut self, control: Arc<SubagentControl>) -> Self {
        self.control = Some(control);
        self
    }

    /// Effective control for this tool: the explicit one when set, otherwise
    /// the process-wide default, so standalone tools share one global budget.
    pub fn control(&self) -> Arc<SubagentControl> {
        self.control
            .clone()
            .unwrap_or_else(|| Arc::clone(global_control()))
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
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 180,
                    "description": "Optional wall-clock timeout override in seconds (default 180)."
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

    /// Builds the structured subagent update result for a lifecycle event.
    pub fn subagent_update(child_id: &str, mode: &str, event: &SubagentEvent) -> AgentToolResult {
        let event_kind = serde_json::to_value(&event.kind).unwrap_or_else(|_| json!("unknown"));
        let details = json!({
            "subagent": {
                "child_id": child_id,
                "mode": mode,
                "event_kind": event_kind,
                "detail": event.detail,
            }
        });
        let text = match event.kind {
            SubagentEventKind::Started => format!("[subagent:{mode}] started"),
            SubagentEventKind::ToolStarted => format!("[subagent:{mode}] Executing tool {}", event.detail.as_deref().unwrap_or("")),
            SubagentEventKind::ToolFinished => format!("[subagent:{mode}] Tool {}", event.detail.as_deref().unwrap_or("completed")),
            SubagentEventKind::Blocked => format!("[subagent:{mode}] blocked"),
            SubagentEventKind::Finished => format!("[subagent:{mode}] finished"),
            SubagentEventKind::Aborted => format!("[subagent:{mode}] aborted"),
            SubagentEventKind::TimedOut => format!("[subagent:{mode}] timed out"),
        };
        let mut result = AgentToolResult::text(text);
        result.details = Some(details);
        result
    }

    /// Single finalization step: applies the terminal status, pushes the
    /// terminal lifecycle event, forwards it as a structured update, and
    /// persists the manifest. Always runs exactly once per child.
    fn finalize_subagent_run(
        cwd: &Path,
        manifest: &mut SubagentManifest,
        events: &Mutex<Vec<SubagentEvent>>,
        outcome: Result<String, String>,
        cancel: &CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<String, String> {
        let (status, failure_class, error, result, terminal_kind, terminal_detail) = match &outcome {
            Ok(out) => (
                SubagentStatus::Completed,
                None,
                None,
                Some(out.clone()),
                SubagentEventKind::Finished,
                Some(out.clone()),
            ),
            Err(e) => {
                let cancelled = cancel.is_cancelled();
                let failure_class = if cancelled {
                    SubagentFailureClass::Aborted
                } else if e.contains("timed out") {
                    SubagentFailureClass::Timeout
                } else if e.contains("aborted") || e.contains("panicked") {
                    SubagentFailureClass::Aborted
                } else {
                    classify_failure(e)
                };
                let terminal_kind = if failure_class == SubagentFailureClass::Timeout {
                    SubagentEventKind::TimedOut
                } else {
                    SubagentEventKind::Aborted
                };
                (
                    SubagentStatus::Failed,
                    Some(failure_class),
                    Some(e.clone()),
                    None,
                    terminal_kind,
                    Some(e.clone()),
                )
            }
        };

        let derived_state = derive_state(&status, failure_class.as_ref()).to_string();
        manifest.status = status;
        manifest.failure_class = failure_class;
        manifest.derived_state = derived_state;
        manifest.completed_at_ms = Some(now_ms());
        manifest.result = result;
        manifest.error = error;

        let terminal_event = SubagentEvent {
            kind: terminal_kind,
            at_ms: now_ms(),
            detail: terminal_detail,
        };
        events.lock().unwrap().push(terminal_event.clone());
        manifest.events = events.lock().unwrap().clone();

        if let Some(ref cb) = on_update {
            cb(Self::subagent_update(&manifest.agent_id, &manifest.mode, &terminal_event));
        }

        // Never discard the run's own outcome: a write failure is appended to
        // it so the caller sees both the real result and the persistence gap.
        match write_manifest(&agent_store_dir(cwd), manifest) {
            Ok(_) => outcome,
            Err(write_err) => Err(match outcome {
                Ok(_) => format!("subagent run completed but manifest write failed: {write_err}"),
                Err(original) => format!("{original}; manifest write failed: {write_err}"),
            }),
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
        manifest: &mut SubagentManifest,
        events: Arc<Mutex<Vec<SubagentEvent>>>,
        timeout_secs: u64,
        steering_queue: Option<Arc<Mutex<VecDeque<Message>>>>,
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

        // The caller owns the shared event log so it can still finalize the
        // manifest (including in-flight tool events) if this task panics.
        let child_agent_id = manifest.agent_id.clone();
        let started_event = SubagentEvent {
            kind: SubagentEventKind::Started,
            at_ms: now_ms(),
            detail: None,
        };
        if let Some(ref cb) = on_update {
            cb(Self::subagent_update(&child_agent_id, mode, &started_event));
        }

        let mode_owned = mode.to_string();
        let listener_events = events.clone();
        let listener_on_update = on_update.clone();
        let listener: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
            let mode_str = mode_owned.clone();
            let agent_id = child_agent_id.clone();
            let events = listener_events.clone();
            let on_update = listener_on_update.clone();
            match event {
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    let ev = SubagentEvent {
                        kind: SubagentEventKind::ToolStarted,
                        at_ms: now_ms(),
                        detail: Some(tool_name),
                    };
                    events.lock().unwrap().push(ev.clone());
                    if let Some(ref cb) = on_update {
                        cb(Self::subagent_update(&agent_id, &mode_str, &ev));
                    }
                }
                AgentEvent::ToolExecutionEnd { is_error, .. } => {
                    let status = if is_error { "failed" } else { "completed" };
                    let ev = SubagentEvent {
                        kind: SubagentEventKind::ToolFinished,
                        at_ms: now_ms(),
                        detail: Some(status.to_string()),
                    };
                    events.lock().unwrap().push(ev.clone());
                    if let Some(ref cb) = on_update {
                        cb(Self::subagent_update(&agent_id, &mode_str, &ev));
                    }
                }
                _ => {}
            }
        });

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

        if let Some(ref queue) = steering_queue {
            config.steering_queue = Arc::clone(queue);
            config.steering_mode = QueueMode::OneAtATime;
        }

        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(task_prompt.to_string()),
            timestamp: now_ms(),
        });

        let outcome: Result<String, String> = async {
            let messages: Result<Vec<Message>, String> = tokio::select! {
                res = tokio::time::timeout(
                    Duration::from_secs(timeout_secs),
                    run_agent_loop(vec![user_msg], Vec::new(), config, Some(listener)),
                ) => res.map_err(|_| format!("Subagent timed out after {timeout_secs}s")),
                _ = cancel.cancelled() => Err("Subagent execution was aborted".to_string()),
            };

            match messages {
                Ok(msgs) => {
                    let final_ast = msgs.iter().rev().find_map(|m| match m {
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
                Err(e) => Err(e),
            }
        }
        .await;

        Ok(Self::finalize_subagent_run(
            &self.cwd,
            manifest,
            &events,
            outcome,
            &cancel,
            on_update,
        )?)
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

        let timeout_secs = params
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 180))
            .unwrap_or(SUBAGENT_TIMEOUT_SECS);

        // Check if parallel batch of tasks was provided
        if let Some(tasks_arr) = params.get("tasks").and_then(|v| v.as_array()) {
            let task_prompts: Vec<String> = tasks_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                .collect();

            if !task_prompts.is_empty() {
                let control = self.control();
                let mut handles = Vec::new();
                let mut child_manifests: Vec<SubagentManifest> = Vec::new();
                let mut child_events: Vec<Arc<Mutex<Vec<SubagentEvent>>>> = Vec::new();
                let mut task_ids: Vec<String> = Vec::new();
                let mut any_failed = false;
                let mut all_completed = true;

                for (idx, task_p) in task_prompts.iter().enumerate() {
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
                        control: self.control.clone(),
                    };
                    let task_agent_id = new_agent_id();
                    task_ids.push(task_agent_id.clone());
                    let mut task_manifest_for_task =
                        new_manifest(task_agent_id.clone(), mode_str.clone(), self.model.id.clone(), now_ms());
                    child_manifests.push(task_manifest_for_task.clone());
                    // Shared with the spawned task so the parent can finalize
                    // the manifest from the child's live event log even when
                    // the child panics before finalizing it itself.
                    let task_events =
                        Arc::new(Mutex::new(task_manifest_for_task.events.clone()));
                    child_events.push(task_events.clone());

                    let task_control = control.clone();
                    handles.push(tokio::spawn(async move {
                        let _permit = task_control.acquire().await;
                        let handle = task_control.register(
                            task_agent_id,
                            mode_str.clone(),
                            cancel_token.child_token(),
                        );
                        if let Some(ref cb) = on_update_child {
                            cb(AgentToolResult::text(format!(
                                "[subagent:{mode_str}] Starting task #{}",
                                idx + 1
                            )));
                        }
                        let res = sub
                            .run_child_prompt(
                                &task_p,
                                &mode_str,
                                custom_sys_owned.as_deref(),
                                max_turns,
                                handle.cancel.clone(),
                                on_update_child,
                                &mut task_manifest_for_task,
                                task_events,
                                timeout_secs,
                                Some(handle.steering_queue.clone()),
                            )
                            .await;
                        task_control.unregister(&handle.agent_id);
                        res
                    }));
                }

                let mut report = String::new();
                for (idx, h) in handles.into_iter().enumerate() {
                    let task_name = &task_prompts[idx];
                    report.push_str(&format!("=== Task #{}: {} ===\n", idx + 1, task_name));
                    match h.await {
                        Ok(Ok(out)) => {
                            report.push_str(&out);
                        }
                        Ok(Err(e)) => {
                            report.push_str(&format!("Error: {}", e));
                            any_failed = true;
                            all_completed = false;
                        }
                        Err(join_err) => {
                            report.push_str(&format!("Join error: {}", join_err));
                            control.unregister(&task_ids[idx]);
                            let mut child_manifest = child_manifests[idx].clone();
                            // Finalize from the shared event log, not the
                            // initial snapshot: the panicked task's in-flight
                            // events (started/tool_started) live there.
                            let shared_events = child_events[idx].clone();
                            if let Err(e) = SubagentTool::finalize_subagent_run(
                                &self.cwd,
                                &mut child_manifest,
                                &shared_events,
                                Err(format!("subagent task panicked: {join_err}")),
                                &cancel,
                                None,
                            ) {
                                report.push_str(&format!("Error: {}", e));
                            }
                            any_failed = true;
                            all_completed = false;
                        }
                    }
                    report.push_str("\n\n");
                }

                // Aggregate over every child instead of the last-writer value:
                // failed if any child failed, completed only when all
                // completed, running while any child is still in flight
                // (defensive: all handles are awaited above).
                let aggregate_status = if any_failed {
                    "failed"
                } else if all_completed {
                    "completed"
                } else {
                    "running"
                };

                let mut result = AgentToolResult::text(report.trim().to_string());
                if any_failed {
                    result.is_error = Some(true);
                }
                if let Some(child_manifest) = child_manifests.last() {
                    let path = agent_store_dir(&self.cwd).join(format!("{}.json", child_manifest.agent_id));
                    result.details = Some(json!({
                        "subagent": {
                            "child_id": child_manifest.agent_id,
                            "mode": mode,
                            "status": aggregate_status,
                            "manifest": path,
                        }
                    }));
                }
                return Ok(result);
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
            control: self.control.clone(),
        };

        let control = self.control();
        let _permit = control.acquire().await;
        let handle = control.register(new_agent_id(), mode.to_string(), cancel.child_token());
        let mut manifest = new_manifest(handle.agent_id.clone(), mode.to_string(), self.model.id.clone(), now_ms());
        // Shared event log: finalize copies it back into the manifest, and the
        // parent can read it even if the child task dies unexpectedly.
        let events = Arc::new(Mutex::new(manifest.events.clone()));
        let outcome = child_sub
            .run_child_prompt(
                prompt,
                mode,
                custom_sys,
                max_turns,
                handle.cancel.clone(),
                on_update,
                &mut manifest,
                events,
                timeout_secs,
                Some(handle.steering_queue.clone()),
            )
            .await;
        control.unregister(&handle.agent_id);

        let path = agent_store_dir(&self.cwd).join(format!("{}.json", manifest.agent_id));
        let details = json!({
            "subagent": {
                "child_id": manifest.agent_id,
                "mode": mode,
                "status": manifest.status,
                "manifest": path,
            }
        });

        // Domain failures (timeout, abort, model error, manifest write
        // failure) are results, not transport errors: return them as an
        // `is_error` result so `details.subagent` (manifest pointer and
        // terminal status) survives on the returned and persisted
        // ToolResult. `Err(String)` is reserved for preconditions that can
        // never produce a manifest (depth limit, missing prompt).
        let (result_text, is_error) = match outcome {
            Ok(text) => (text, false),
            Err(err) => (err, true),
        };
        let mut result = AgentToolResult::text(result_text);
        if is_error {
            result.is_error = Some(true);
        }
        result.details = Some(details);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};

    #[test]
    fn finalize_subagent_run_keeps_original_error_when_manifest_write_fails() {
        let bad_cwd = std::env::temp_dir().join(format!("mehh_fin_bad_{}", uuid::Uuid::now_v7()));
        std::fs::write(&bad_cwd, "not a directory").unwrap();
        let mut manifest = new_manifest("sub-fin-err".into(), "research".into(), "m".into(), 1000);
        let events = Mutex::new(manifest.events.clone());
        let cancel = CancellationToken::new();
        let err = SubagentTool::finalize_subagent_run(
            &bad_cwd,
            &mut manifest,
            &events,
            Err("child exploded".to_string()),
            &cancel,
            None,
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&bad_cwd);
        assert!(
            err.contains("child exploded"),
            "the original outcome error must survive a write failure, got: {err}"
        );
        assert!(
            err.contains("manifest write failed"),
            "the write failure must also be reported, got: {err}"
        );
    }

    #[test]
    fn finalize_subagent_run_reports_manifest_write_failure_after_success() {
        let bad_cwd = std::env::temp_dir().join(format!("mehh_fin_ok_{}", uuid::Uuid::now_v7()));
        std::fs::write(&bad_cwd, "not a directory").unwrap();
        let mut manifest = new_manifest("sub-fin-ok".into(), "research".into(), "m".into(), 1000);
        let events = Mutex::new(manifest.events.clone());
        let cancel = CancellationToken::new();
        let err = SubagentTool::finalize_subagent_run(
            &bad_cwd,
            &mut manifest,
            &events,
            Ok("done".to_string()),
            &cancel,
            None,
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&bad_cwd);
        assert!(
            err.contains("manifest write failed"),
            "a write failure after success must be an error, got: {err}"
        );
        assert!(
            err.contains("completed"),
            "the error must state the run itself completed, got: {err}"
        );
    }

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
