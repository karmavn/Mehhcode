//! Depth-bounded, multi-mode subagent tool for delegated tasks and parallel execution.

mod runner;
mod scratchpad;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use mehh_ai::{now_ms, LlmProvider, Model, Usage};

use runner::{
    accumulate_usage, CancelOnDrop, ChildRunError, ChildRunParams, RegistrationGuard,
};
#[cfg(test)]
use runner::{append_web_tool_guidance, finalize_wait_budget};
use crate::lead_worker::LeadWorkerManager;
use crate::subagent_control::{global_control, SubagentControl};
use crate::subagent_lifecycle::{
    agent_store_dir, new_agent_id, new_manifest, SubagentEvent, SubagentEventKind,
    SubagentManifest, SubagentStatus,
};
use crate::tool_inspection::ToolInspectionManager;

/// Maximum nested subagent recursion depth.
pub const MAX_SUBAGENT_DEPTH: usize = 2;

/// Maximum assistant turns allowed per subagent invocation.
pub const SUBAGENT_MAX_TURNS: usize = 20;

/// Maximum number of tasks accepted in one parallel batch. The batch
/// semaphore only limits concurrency; this bounds the total spawned work.
pub const MAX_BATCH_TASKS: usize = 16;

/// Maximum wall-clock timeout for a subagent invocation in seconds.
pub const SUBAGENT_TIMEOUT_SECS: u64 = 180;

/// Wall-clock budget for the forced finalization summary call in seconds.
pub const SUBAGENT_FINALIZE_TIMEOUT_SECS: u64 = 60;

/// Shared in-memory notes for one batch fan-out.
type Scratchpad = Arc<RwLock<HashMap<String, String>>>;

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
    /// Live approval-handler slot shared with `AgentSession` so `set_approval_handler`
    /// takes effect without rebuilding `SubagentTool`.
    pub approval_handler_slot: Option<crate::tool_inspection::ApprovalHandlerSlot>,
    pub control: Option<Arc<SubagentControl>>,
    /// Optional Lead/Worker routing template; each child run clones its own state.
    pub lead_worker: Option<Arc<Mutex<LeadWorkerManager>>>,
    /// Shared notes for batch siblings; set per fan-out, None for single runs.
    pub scratchpad: Option<Scratchpad>,
    /// Default wall-clock budget for one child run (loop + cleanup grace).
    pub timeout: Duration,
    /// Credential cache shared by every child in one call tree, so a batch
    /// performs one credential-file read / OAuth refresh instead of one per child.
    pub api_key_cache: Arc<Mutex<HashMap<String, Option<String>>>>,
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
            approval_handler_slot: None,
            control: None,
            lead_worker: None,
            scratchpad: None,
            timeout: Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
            api_key_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attaches a Lead/Worker routing template for mode-aware model selection.
    pub fn with_lead_worker(mut self, mgr: Arc<Mutex<LeadWorkerManager>>) -> Self {
        self.lead_worker = Some(mgr);
        self
    }

    /// Overrides the default wall-clock budget for one child run.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
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

    pub fn with_approval_handler_slot(
        mut self,
        slot: crate::tool_inspection::ApprovalHandlerSlot,
    ) -> Self {
        self.approval_handler_slot = Some(slot);
        self
    }

    pub fn with_api_key_cache(
        mut self,
        cache: Arc<Mutex<HashMap<String, Option<String>>>>,
    ) -> Self {
        self.api_key_cache = cache;
        self
    }

    fn initial_model_id(&self) -> String {
        if let Some(ref lw) = self.lead_worker {
            let m = mehh_ai::lock_recover(lw);
            m.worker_model()
                .map(|w| w.id.clone())
                .unwrap_or_else(|| m.lead_model.id.clone())
        } else {
            self.model.id.clone()
        }
    }

    fn clone_for_child(&self, depth: usize, scratchpad: Option<Scratchpad>) -> Self {
        Self {
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            api_key: self.api_key.clone(),
            tools: self.tools.clone(),
            depth,
            inspection_manager: self.inspection_manager.clone(),
            approval_handler: self.approval_handler.clone(),
            approval_handler_slot: self.approval_handler_slot.clone(),
            control: self.control.clone(),
            lead_worker: self.lead_worker.clone(),
            scratchpad,
            timeout: self.timeout,
            api_key_cache: self.api_key_cache.clone(),
        }
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

    fn normalize_provider_key(provider: &str) -> String {
        provider.trim().to_ascii_lowercase()
    }

    /// Resolves one provider credential per call tree. Batch children share
    /// the cache, so a fan-out performs one credential read / OAuth refresh
    /// instead of one per child. Failures stay uncached and are propagated so
    /// credential/refresh errors fail closed rather than silently falling back
    /// to unauthenticated requests.
    pub(super) async fn resolved_api_key(&self, provider: &str) -> Result<Option<String>, String> {
        Self::resolve_cached_api_key(&self.api_key_cache, provider).await
    }

    pub(super) async fn resolve_cached_api_key(
        cache: &Arc<Mutex<HashMap<String, Option<String>>>>,
        provider: &str,
    ) -> Result<Option<String>, String> {
        let cache_key = Self::normalize_provider_key(provider);
        if let Some(cached) = cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&cache_key)
        {
            return Ok(cached.clone());
        }
        let key = crate::auth::resolve_api_key_async(provider, None, None)
            .await
            .map_err(|e| format!("Failed to resolve credentials for provider '{provider}': {e}"))?;
        if key.is_some() {
            mehh_ai::lock_recover(cache).insert(cache_key, key.clone());
        }
        Ok(key)
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
                    "maxItems": MAX_BATCH_TASKS,
                    "description": "Optional list of independent tasks to execute concurrently (up to 16 tasks, 4 in parallel; coding tasks run sequentially on the shared working tree). Batch tasks share a scratchpad via note_write/note_read."
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
                },
                "model_tier": {
                    "type": "string",
                    "enum": ["worker", "lead", "auto"],
                    "description": "Optional model tier override: 'worker' (cheap, worker-first), 'lead' (strongest model only), 'auto' (mode default: research=worker-first, coding=adaptive, review=lead)."
                }
            }
        })
    }

    /// Filters available tools for the specified execution mode.
    /// Coding excludes recursive and deferred-discovery tools
    /// (`subagent`, `codemode`, `todo`, `tool_search`).
    /// Research/review admit any tool annotated read-only (including read-only
    /// MCP tools), fail closed on unannotated tools, and always exclude
    /// recursive/stateful/deferred-discovery tools regardless of their annotation.
    pub fn filter_tools_for_mode(&self, mode: &str) -> Vec<Arc<dyn AgentTool>> {
        match mode {
            "coding" => self
                .tools
                .iter()
                .filter(|t| !matches!(t.name(), "subagent" | "codemode" | "todo" | "tool_search"))
                .cloned()
                .collect(),
            "review" | "research" => self
                .tools
                .iter()
                .filter(|t| {
                    !matches!(t.name(), "subagent" | "codemode" | "todo" | "tool_search")
                        && t.is_read_only() == Some(true)
                })
                .cloned()
                .collect(),
            _ => self.filter_tools_for_mode("research"),
        }
    }

    /// Rebuilds recursive tools at the correct nesting depth for this child.
    /// The inherited tool list still holds the parent's `SubagentTool` arc;
    /// expose a fresh one at `self.depth + 1`, or hide it entirely when the
    /// next level would reach the ceiling. Recursion is disabled for all
    /// modes today; this keeps the depth contract correct if a mode ever
    /// admits the subagent tool again.
    pub(super) fn depth_corrected_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Vec<Arc<dyn AgentTool>> {
        let nested_depth = self.depth + 1;
        tools
            .into_iter()
            .filter_map(|t| {
                if t.name() != "subagent" {
                    return Some(t);
                }
                if nested_depth >= MAX_SUBAGENT_DEPTH {
                    return None;
                }
                Some(Arc::new(self.clone_for_child(nested_depth, self.scratchpad.clone()))
                    as Arc<dyn AgentTool>)
            })
            .collect()
    }

    /// Builds the structured subagent update result for a lifecycle event.
    pub fn subagent_update(child_id: &str, mode: &str, event: &SubagentEvent) -> AgentToolResult {
        let event_kind = serde_json::to_value(event.kind).unwrap_or_else(|_| json!("unknown"));
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
            SubagentEventKind::ToolStarted => format!(
                "[subagent:{mode}] Executing tool {}",
                event.detail.as_deref().unwrap_or("")
            ),
            SubagentEventKind::ToolFinished => format!(
                "[subagent:{mode}] Tool {}",
                event.detail.as_deref().unwrap_or("completed")
            ),
            SubagentEventKind::Blocked => format!("[subagent:{mode}] blocked"),
            SubagentEventKind::Finished => format!("[subagent:{mode}] finished"),
            SubagentEventKind::Aborted => format!("[subagent:{mode}] aborted"),
            SubagentEventKind::TimedOut => format!("[subagent:{mode}] timed out"),
        };
        let mut result = AgentToolResult::text(text);
        result.details = Some(details);
        result
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
        let model_tier = params.get("model_tier").and_then(|v| v.as_str());

        let max_turns = params
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, SUBAGENT_MAX_TURNS))
            .unwrap_or(SUBAGENT_MAX_TURNS);

        let timeout = params
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|n| Duration::from_secs(n.clamp(1, 180)))
            .unwrap_or(self.timeout);

        // Check if parallel batch of tasks was provided
        if let Some(tasks_arr) = params.get("tasks").and_then(|v| v.as_array()) {
            if tasks_arr.len() > MAX_BATCH_TASKS {
                return Err(format!(
                    "Too many batch tasks: {} supplied, maximum is {MAX_BATCH_TASKS}",
                    tasks_arr.len()
                ));
            }
            let task_prompts: Vec<String> = tasks_arr
                .iter()
                .filter_map(|v| {
                    v.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .collect();

            if !task_prompts.is_empty() {
                let control = self.control();
                // Detached batch tasks must die with the caller's future, not
                // keep running after the tool call has been cancelled or dropped.
                let batch_cancel = cancel.child_token();
                let _batch_cancel_guard = CancelOnDrop(batch_cancel.clone());
                // Coding tasks share one working tree: run them one at a time
                // to avoid file/build-lock conflicts. Read-only modes fan out.
                let local_permits = if mode == "coding" { 1 } else { 4 };
                let batch_semaphore = Arc::new(tokio::sync::Semaphore::new(local_permits));
                let batch_pad: Scratchpad = Arc::new(RwLock::new(HashMap::new()));
                let mut handles = Vec::new();
                let mut child_manifests: Vec<SubagentManifest> = Vec::new();
                // Each child's own terminal status, tracked independently of
                // the aggregate so `details.subagent.children` can report it.
                let mut child_statuses: Vec<SubagentStatus> = Vec::new();
                let mut child_events: Vec<Arc<Mutex<Vec<SubagentEvent>>>> = Vec::new();
                let mut task_ids: Vec<String> = Vec::new();
                let mut any_failed = false;
                let mut all_completed = true;

                for (idx, task_p) in task_prompts.iter().enumerate() {
                    let task_p = task_p.clone();
                    let cancel_token = batch_cancel.clone();
                    let on_update_child = on_update.clone();
                    let mode_str = mode.to_string();
                    let custom_sys_owned = custom_sys.map(str::to_string);
                    let model_tier_owned = model_tier.map(str::to_string);
                    let sub = self.clone_for_child(self.depth + 1, Some(batch_pad.clone()));
                    let task_agent_id = new_agent_id();
                    task_ids.push(task_agent_id.clone());
                    let mut task_manifest_for_task = new_manifest(
                        task_agent_id.clone(),
                        mode_str.clone(),
                        self.initial_model_id(),
                        now_ms(),
                    );
                    child_manifests.push(task_manifest_for_task.clone());
                    child_statuses.push(SubagentStatus::Running);
                    // Shared with the spawned task so the parent can finalize
                    // the manifest from the child's live event log even when
                    // the child panics before finalizing it itself.
                    let task_events = Arc::new(Mutex::new(task_manifest_for_task.events.clone()));
                    child_events.push(task_events.clone());

                    let task_control = control.clone();
                    let batch_sem = batch_semaphore.clone();
                    handles.push(tokio::spawn(async move {
                        let task_prefix = format!("#{}:", idx + 1);
                        let _local_permit = tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => {
                                let err = ChildRunError::Aborted;
                                let _ = SubagentTool::finalize_subagent_run(
                                    &sub.cwd,
                                    &mut task_manifest_for_task,
                                    &task_events,
                                    Err(err.clone()),
                                    &cancel_token,
                                    on_update_child.clone(),
                                    &task_prefix,
                                );
                                return Err(err);
                            }
                            permit = batch_sem.acquire() => match permit {
                                Ok(p) => p,
                                Err(e) => {
                                    let err = ChildRunError::ToolFailure(e.to_string());
                                    let _ = SubagentTool::finalize_subagent_run(
                                        &sub.cwd,
                                        &mut task_manifest_for_task,
                                        &task_events,
                                        Err(err.clone()),
                                        &cancel_token,
                                        on_update_child.clone(),
                                        &task_prefix,
                                    );
                                    return Err(err);
                                }
                            }
                        };
                        let _permit = tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => {
                                let err = ChildRunError::Aborted;
                                let _ = SubagentTool::finalize_subagent_run(
                                    &sub.cwd,
                                    &mut task_manifest_for_task,
                                    &task_events,
                                    Err(err.clone()),
                                    &cancel_token,
                                    on_update_child.clone(),
                                    &task_prefix,
                                );
                                return Err(err);
                            }
                            permit = task_control.acquire() => permit,
                        };
                        let handle = task_control.register(
                            task_agent_id.clone(),
                            mode_str.clone(),
                            cancel_token.child_token(),
                        );
                        let _registration = RegistrationGuard {
                            control: task_control.clone(),
                            agent_id: task_agent_id,
                        };
                        if let Some(ref cb) = on_update_child {
                            cb(AgentToolResult::text(format!(
                                "[subagent:{task_prefix}{mode_str}] Starting task"
                            )));
                        }
                        let res = sub
                            .run_child_prompt(
                                ChildRunParams {
                                    prompt: &task_p,
                                    mode: &mode_str,
                                    custom_sys: custom_sys_owned.as_deref(),
                                    max_turns,
                                    task_id: Some(idx),
                                    model_tier: model_tier_owned.as_deref(),
                                    timeout,
                                    steering_queue: Some(handle.steering_queue.clone()),
                                },
                                handle.cancel.clone(),
                                on_update_child,
                                &mut task_manifest_for_task,
                                task_events,
                            )
                            .await;
                        res
                    }));
                }

                let mut report = String::new();
                let mut total_usage = Usage::default();
                let mut task_meta: Vec<Value> = Vec::new();
                for (idx, h) in handles.into_iter().enumerate() {
                    let task_name = &task_prompts[idx];
                    report.push_str(&format!("=== Task #{}: {} ===\n", idx + 1, task_name));
                    match h.await {
                        Ok(Ok(out)) => {
                            child_statuses[idx] = SubagentStatus::Completed;
                            accumulate_usage(&mut total_usage, &out.usage);
                            report.push_str(&out.result_text());
                            let mut meta = out.metadata_json();
                            meta["task"] = json!(idx + 1);
                            task_meta.push(meta);
                        }
                        Ok(Err(e)) => {
                            child_statuses[idx] = SubagentStatus::Failed;
                            report.push_str(&format!("Error: {}", e));
                            task_meta.push(json!({"task": idx + 1, "status": "error", "error": e.to_string()}));
                            any_failed = true;
                            all_completed = false;
                        }
                        Err(join_err) => {
                            child_statuses[idx] = SubagentStatus::Failed;
                            report.push_str(&format!("Join error: {}", join_err));
                            task_meta.push(json!({"task": idx + 1, "status": "error", "error": join_err.to_string()}));
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
                                Err(ChildRunError::Panic(join_err.to_string())),
                                &batch_cancel,
                                on_update.clone(),
                                &format!("#{}:", idx + 1),
                            ) {
                                if e.contains("manifest write failed") {
                                    report.push_str(&format!("\nError: {}", e));
                                }
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
                result.structured_content = Some(json!({"tasks": task_meta, "usage": total_usage}));
                result.usage = Some(total_usage);
                if let Some(child_manifest) = child_manifests.last() {
                    let path = agent_store_dir(&self.cwd)
                        .join(format!("{}.json", child_manifest.agent_id));
                    // `child_id`/`manifest` above keep pointing at the last
                    // child for compatibility; `children` carries every child
                    // with its own terminal status and manifest path.
                    let children: Vec<Value> = child_manifests
                        .iter()
                        .zip(child_statuses.iter())
                        .map(|(m, status)| {
                            let manifest =
                                agent_store_dir(&self.cwd).join(format!("{}.json", m.agent_id));
                            json!({
                                "child_id": m.agent_id,
                                "status": status,
                                "manifest": manifest,
                            })
                        })
                        .collect();
                    result.details = Some(json!({
                        "subagent": {
                            "child_id": child_manifest.agent_id,
                            "mode": mode,
                            "status": aggregate_status,
                            "manifest": path,
                            "children": children,
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

        let child_sub = self.clone_for_child(self.depth + 1, self.scratchpad.clone());

        let control = self.control();
        let _permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("Subagent execution was aborted".to_string()),
            permit = control.acquire() => permit,
        };
        let handle = control.register(new_agent_id(), mode.to_string(), cancel.child_token());
        let _registration = RegistrationGuard {
            control: control.clone(),
            agent_id: handle.agent_id.clone(),
        };
        let mut manifest = new_manifest(
            handle.agent_id.clone(),
            mode.to_string(),
            self.initial_model_id(),
            now_ms(),
        );
        // Shared event log: finalize copies it back into the manifest, and the
        // parent can read it even if the child task dies unexpectedly.
        let events = Arc::new(Mutex::new(manifest.events.clone()));
        let outcome = child_sub
            .run_child_prompt(
                ChildRunParams {
                    prompt,
                    mode,
                    custom_sys,
                    max_turns,
                    task_id: None,
                    model_tier,
                    timeout,
                    steering_queue: Some(handle.steering_queue.clone()),
                },
                handle.cancel.clone(),
                on_update,
                &mut manifest,
                events,
            )
            .await;

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
        let (result_text, is_error, handoff, usage) = match outcome {
            Ok(o) => (
                o.result_text(),
                false,
                Some(o.metadata_json()),
                Some(o.usage.clone()),
            ),
            Err(err) => (err.to_string(), true, None, None),
        };
        let mut result = AgentToolResult::text(result_text);
        if is_error {
            result.is_error = Some(true);
        }
        result.details = Some(details);
        result.structured_content = handoff;
        result.usage = usage;
        Ok(result)
    }
}
