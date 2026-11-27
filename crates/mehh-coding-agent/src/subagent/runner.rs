use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use mehh_agent::{
    run_agent_loop, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate, AgentTool, AgentToolResult,
    AgentToolUpdateCallback, QueueMode,
};
use mehh_ai::{
    now_ms, Context, LlmProvider, Message, StopReason, Usage, UserContent, UserMessage,
};

use super::scratchpad::{NoteReadTool, NoteWriteTool};
use super::{SubagentTool, SUBAGENT_FINALIZE_TIMEOUT_SECS};
use crate::lead_worker::LeadWorkerManager;
use crate::subagent_control::SubagentControl;
use crate::subagent_lifecycle::{
    agent_store_dir, classify_failure, derive_state, write_manifest, SubagentEvent,
    SubagentEventKind, SubagentFailureClass, SubagentManifest, SubagentStatus,
};

/// Grace period after timeout cancellation for tools to kill process trees
/// and abort in-flight requests before the loop future is dropped.
const SUBAGENT_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Shorter grace for an explicit kill: the caller wants the child stopped
/// now, so cleanup is best-effort before the loop future is dropped.
const SUBAGENT_KILL_GRACE: Duration = Duration::from_millis(500);

/// The finalize call must not extend the caller's `timeout_secs` budget.
pub(super) fn finalize_wait_budget(remaining: Duration) -> Duration {
    remaining.min(Duration::from_secs(SUBAGENT_FINALIZE_TIMEOUT_SECS))
}

/// RAII guard: cancels the child token whenever the run scope exits —
/// timeout, error, or the parent dropping this future mid-run.
pub(super) struct CancelOnDrop(pub(super) CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Removes a child from the registry on every exit path, including a dropped
/// future that never reaches its explicit unregister call.
pub(super) struct RegistrationGuard {
    pub(super) control: Arc<SubagentControl>,
    pub(super) agent_id: String,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        self.control.unregister(&self.agent_id);
    }
}

/// Instruction injected when the turn budget ends without a concluding text answer.
const FINALIZE_PROMPT: &str = "Your turn budget is exhausted and you must not call any more tools. Based only on the evidence already gathered above, write the final answer now: a concise summary of findings or changes, with exact file paths. If nothing was completed, state what was attempted and what remains.";

/// Instruction injected when a Lead rescue takes over after a Worker failure.
const RESCUE_PROMPT: &str = "The previous model failed while working on this task. You are taking over with a fresh perspective. Review the evidence gathered so far, resolve the blocker, and complete the original task. Return a concise final answer with exact file paths.";

/// Structured result of one subagent run: summary plus budget/telemetry handoff.
pub(super) struct ChildOutcome {
    summary: String,
    status: &'static str,
    turns_used: usize,
    pub(super) usage: Usage,
    files_touched: Vec<String>,
}

impl ChildOutcome {
    pub(super) fn metadata_json(&self) -> Value {
        json!({
            "summary": self.summary,
            "status": self.status,
            "files_touched": self.files_touched,
            "turns_used": self.turns_used,
            "usage": self.usage,
        })
    }

    fn status_line(&self) -> String {
        let files = if self.files_touched.is_empty() {
            "-".to_string()
        } else {
            self.files_touched.join("; ")
        };
        format!(
            "[subagent-meta] status={} turns_used={} tokens_in={} tokens_out={} cost_usd={:.6} files_touched={}",
            self.status,
            self.turns_used,
            self.usage.input,
            self.usage.output,
            self.usage.cost.total,
            files
        )
    }

    pub(super) fn result_text(&self) -> String {
        format!("{}\n\n{}", self.summary, self.status_line())
    }
}

/// Typed failure of one child run. Keeping the cause in the variant (instead
/// of matching substrings later) prevents a child tool's "timed out" message
/// from being recorded as the subagent's own deadline expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChildRunError {
    TimedOut(std::time::Duration),
    Aborted,
    ModelError(String),
    ToolFailure(String),
    Panic(String),
    ManifestWrite(String),
    NoResponse,
}

impl ChildRunError {
    fn failure_class(&self) -> SubagentFailureClass {
        match self {
            Self::TimedOut(_) => SubagentFailureClass::Timeout,
            Self::Aborted => SubagentFailureClass::Aborted,
            Self::ToolFailure(_) => SubagentFailureClass::ToolRuntime,
            Self::Panic(_) | Self::ManifestWrite(_) => SubagentFailureClass::Infra,
            Self::ModelError(msg) => classify_failure(msg),
            Self::NoResponse => SubagentFailureClass::Infra,
        }
    }

    fn terminal_kind(&self) -> SubagentEventKind {
        match self {
            Self::TimedOut(_) => SubagentEventKind::TimedOut,
            _ => SubagentEventKind::Aborted,
        }
    }
}

impl std::fmt::Display for ChildRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(d) => write!(f, "Subagent timed out after {}s", d.as_secs()),
            Self::Aborted => write!(f, "Subagent execution was aborted"),
            Self::ModelError(msg)
            | Self::ToolFailure(msg)
            | Self::Panic(msg)
            | Self::ManifestWrite(msg) => write!(f, "{msg}"),
            Self::NoResponse => write!(f, "Subagent produced no assistant response"),
        }
    }
}

pub(super) fn accumulate_usage(total: &mut Usage, u: &Usage) {
    total.input += u.input;
    total.output += u.output;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
    total.total_tokens += u.total_tokens;
    total.cost.input += u.cost.input;
    total.cost.output += u.cost.output;
    total.cost.cache_read += u.cost.cache_read;
    total.cost.cache_write += u.cost.cache_write;
    total.cost.total += u.cost.total;
}

/// Collects file paths mutated by write/edit tool calls, in first-touch order.
fn collect_files_touched(messages: &[Message]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for m in messages {
        if let Message::Assistant(a) = m {
            for b in &a.content {
                if let mehh_ai::AssistantContentBlock::ToolCall(tc) = b {
                    if matches!(tc.name.as_str(), "write" | "edit" | "undo_edit") {
                        if let Some(path) = tc.arguments.get("path").and_then(|v| v.as_str()) {
                            if !files.iter().any(|f| f == path) {
                                files.push(path.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    files
}

/// Per-run inputs for one child subagent invocation.
pub(super) struct ChildRunParams<'a> {
    pub(super) prompt: &'a str,
    pub(super) mode: &'a str,
    pub(super) custom_sys: Option<&'a str>,
    pub(super) max_turns: usize,
    pub(super) task_id: Option<usize>,
    pub(super) model_tier: Option<&'a str>,
    pub(super) timeout: Duration,
    pub(super) steering_queue: Option<Arc<Mutex<VecDeque<Message>>>>,
}

/// Builds a lifecycle update, prefixing batch task identity into the text tag
/// so parallel siblings stay distinguishable in logs and the TUI.
fn prefixed_update(
    child_id: &str,
    mode: &str,
    event: &SubagentEvent,
    prefix: &str,
) -> AgentToolResult {
    let mut upd = SubagentTool::subagent_update(child_id, mode, event);
    if !prefix.is_empty() {
        if let Some(mehh_ai::ToolResultContentBlock::Text(t)) = upd.content.first_mut() {
            t.text = t
                .text
                .replacen("[subagent:", &format!("[subagent:{prefix}"), 1);
        }
    }
    upd
}

impl SubagentTool {
    /// Single finalization step: applies the terminal status, pushes the
    /// terminal lifecycle event, forwards it as a structured update, and
    /// persists the manifest. Always runs exactly once per child.
    ///
    /// Returns `Err(msg)` only when persisting the manifest itself fails;
    /// callers keep the typed `ChildRunError` from `outcome` whenever the
    /// manifest write succeeds.
    pub(super) fn finalize_subagent_manifest(
        cwd: &Path,
        manifest: &mut SubagentManifest,
        events: &Mutex<Vec<SubagentEvent>>,
        outcome: &Result<String, ChildRunError>,
        cancel: &CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
        prefix: &str,
    ) -> Result<(), String> {
        let (status, failure_class, error, result, terminal_kind, terminal_detail) = match outcome {
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
                } else {
                    e.failure_class()
                };
                let terminal_kind = if cancelled {
                    SubagentEventKind::Aborted
                } else {
                    e.terminal_kind()
                };
                let detail = e.to_string();
                (
                    SubagentStatus::Failed,
                    Some(failure_class),
                    Some(detail.clone()),
                    None,
                    terminal_kind,
                    Some(detail),
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
        {
            let mut event_log = events.lock().unwrap_or_else(PoisonError::into_inner);
            event_log.push(terminal_event.clone());
            manifest.events = event_log.clone();
        }

        if let Some(ref cb) = on_update {
            cb(prefixed_update(
                &manifest.agent_id,
                &manifest.mode,
                &terminal_event,
                prefix,
            ));
        }

        // Never discard the run's own outcome: a write failure is appended to
        // it so the caller sees both the real result and the persistence gap.
        write_manifest(&agent_store_dir(cwd), manifest)
            .map(|_| ())
            .map_err(|write_err| match outcome {
                Ok(out) => format!(
                    "{out}\n\nWarning: subagent run completed, but manifest write failed: {write_err}"
                ),
                Err(original) => format!("{original}; manifest write failed: {write_err}"),
            })
    }

    pub(super) fn finalize_subagent_run(
        cwd: &Path,
        manifest: &mut SubagentManifest,
        events: &Mutex<Vec<SubagentEvent>>,
        outcome: Result<String, ChildRunError>,
        cancel: &CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
        prefix: &str,
    ) -> Result<String, String> {
        Self::finalize_subagent_manifest(
            cwd, manifest, events, &outcome, cancel, on_update, prefix,
        )?;
        outcome.map_err(|e| e.to_string())
    }

    pub(super) async fn run_child_prompt(
        &self,
        params: ChildRunParams<'_>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
        manifest: &mut SubagentManifest,
        events: Arc<Mutex<Vec<SubagentEvent>>>,
    ) -> Result<ChildOutcome, ChildRunError> {
        let ChildRunParams {
            prompt: task_prompt,
            mode,
            custom_sys,
            max_turns,
            task_id,
            model_tier,
            timeout,
            steering_queue,
        } = params;
        let mut child_tools = self.depth_corrected_tools(self.filter_tools_for_mode(mode));
        if let Some(ref pad) = self.scratchpad {
            child_tools.push(Arc::new(NoteWriteTool { pad: pad.clone() }));
            child_tools.push(Arc::new(NoteReadTool { pad: pad.clone() }));
        }
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
        let mut sys = match custom_sys {
            Some(custom) => custom.to_string(),
            None => {
                let mut s = default_sys;
                append_web_tool_guidance(&mut s, &child_tools);
                s
            }
        };
        if self.scratchpad.is_some() {
            sys.push_str(" You share a scratchpad with sibling tasks in this batch: publish short findings with note_write(key, value) and consume theirs with note_read(key) instead of re-reading the same files.");
        }

        // The caller owns the shared event log so it can still finalize the
        // manifest (including in-flight tool events) if this task panics.
        let child_agent_id = manifest.agent_id.clone();
        let task_prefix = task_id.map(|i| format!("#{}:", i + 1)).unwrap_or_default();
        let started_event = SubagentEvent {
            kind: SubagentEventKind::Started,
            at_ms: now_ms(),
            detail: None,
        };
        if let Some(ref cb) = on_update {
            cb(prefixed_update(
                &child_agent_id,
                mode,
                &started_event,
                &task_prefix,
            ));
        }

        let mode_owned = mode.to_string();
        let listener_events = events.clone();
        let listener_on_update = on_update.clone();
        let listener_prefix = task_prefix.clone();
        let listener_agent_id = child_agent_id.clone();
        let listener: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
            let mode_str = mode_owned.clone();
            let agent_id = listener_agent_id.clone();
            let events = listener_events.clone();
            let on_update = listener_on_update.clone();
            let prefix = listener_prefix.clone();
            match event {
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    let ev = SubagentEvent {
                        kind: SubagentEventKind::ToolStarted,
                        at_ms: now_ms(),
                        detail: Some(tool_name),
                    };
                    events.lock().unwrap_or_else(PoisonError::into_inner).push(ev.clone());
                    if let Some(ref cb) = on_update {
                        cb(prefixed_update(&agent_id, &mode_str, &ev, &prefix));
                    }
                }
                AgentEvent::ToolExecutionEnd { is_error, .. } => {
                    let status = if is_error { "failed" } else { "completed" };
                    let ev = SubagentEvent {
                        kind: SubagentEventKind::ToolFinished,
                        at_ms: now_ms(),
                        detail: Some(status.to_string()),
                    };
                    events.lock().unwrap_or_else(PoisonError::into_inner).push(ev.clone());
                    if let Some(ref cb) = on_update {
                        cb(prefixed_update(&agent_id, &mode_str, &ev, &prefix));
                    }
                }
                _ => {}
            }
        });

        let mut base_config = AgentLoopConfig::new(self.model.clone(), Arc::clone(&self.provider));
        base_config.tools = child_tools;
        base_config.system_prompt = Some(sys);

        // Per-child Lead/Worker routing state, independent of parent and siblings.
        // Mode policy: research = worker-first, review = lead-only,
        // coding = template lead_turns; model_tier overrides the mode default.
        let lead_worker: Option<Arc<Mutex<LeadWorkerManager>>> =
            self.lead_worker.as_ref().map(|template| {
                let mut m = template.lock().unwrap_or_else(PoisonError::into_inner).clone();
                m.reset_for_new_prompt();
                match model_tier {
                    Some("lead") => {
                        m.worker_model = None;
                        m.worker_provider = None;
                    }
                    Some("worker") => {
                        m.lead_turns = 0;
                    }
                    _ => match mode {
                        "research" => m.lead_turns = 0,
                        "review" => {
                            m.worker_model = None;
                            m.worker_provider = None;
                        }
                        _ => {}
                    },
                }
                Arc::new(Mutex::new(m))
            });
        if let Some(ref lw) = lead_worker {
            let m = lw.lock().unwrap_or_else(PoisonError::into_inner);
            base_config.model = m.active_model();
            base_config.provider = m.active_provider();
            let active_key = m.active_api_key();
            base_config.stream_options.api_key = active_key;
            manifest.model_id = base_config.model.id.clone();
        }

        if let Some(ref lw) = lead_worker {
            let lw = lw.clone();
            base_config.after_tool_call = Some(Arc::new(move |after_ctx, _cancel| {
                let lw = lw.clone();
                Box::pin(async move {
                    lw.lock().unwrap_or_else(PoisonError::into_inner).record_tool_outcome(after_ctx.is_error);
                    mehh_agent::AfterToolCallResult::default()
                })
            }));
        }

        if let Some(ref mgr) = self.inspection_manager {
            let mgr = mgr.clone();
            let session_cwd = self.cwd.clone();
            let approval_h = self.approval_handler.clone();
            let origin_label = format!("subagent:{child_agent_id}:{mode}");
            base_config.before_tool_call = Some(Arc::new(move |before_ctx, cancel| {
                let mgr = mgr.clone();
                let session_cwd = session_cwd.clone();
                let approval_h = approval_h.clone();
                let origin_label = origin_label.clone();
                Box::pin(async move {
                    let tool_name = before_ctx.tool_call.name.clone();
                    let tool_call_id = before_ctx.tool_call.id.clone();
                    let is_read_only = before_ctx
                        .context
                        .tools
                        .as_ref()
                        .and_then(|tools| tools.iter().find(|t| t.name() == tool_name))
                        .and_then(|t| t.is_safe_for_auto_approve(&before_ctx.args));

                    let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                        tool_call_id,
                        tool_name,
                        args: before_ctx.args,
                        is_read_only,
                        cwd: session_cwd,
                        origin: Some(origin_label),
                    };
                    mgr.inspect_and_authorize_cancellable(
                        &inspect_ctx,
                        approval_h.as_deref(),
                        &cancel,
                    )
                    .await
                })
            }));
        }

        if let Some(ref queue) = steering_queue {
            base_config.steering_queue = Arc::clone(queue);
            base_config.steering_mode = QueueMode::OneAtATime;
        }

        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(task_prompt.to_string()),
            timestamp: now_ms(),
        });

        let outcome: Result<ChildOutcome, ChildRunError> = async {
            if lead_worker.is_none() {
                if let Some(ref key) = self.api_key {
                    base_config.stream_options.api_key = Some(key.clone());
                } else if self.model.provider != "faux" && self.model.api != "faux" {
                    if let Some(key) = self
                        .resolved_api_key(&self.model.provider)
                        .await
                        .map_err(ChildRunError::ModelError)?
                    {
                        base_config.stream_options.api_key = Some(key);
                    }
                }
            } else if base_config.stream_options.api_key.is_none()
                && base_config.model.provider != "faux"
                && base_config.model.api != "faux"
            {
                if let Some(key) = self
                    .resolved_api_key(&base_config.model.provider)
                    .await
                    .map_err(ChildRunError::ModelError)?
                {
                    base_config.stream_options.api_key = Some(key);
                }
            }

            let child_cancel = cancel.child_token();
            let _cancel_guard = CancelOnDrop(child_cancel.clone());
            base_config.cancel = child_cancel.clone();

            let deadline = tokio::time::Instant::now() + timeout;
            let mut all_messages: Vec<Message> = Vec::new();
            let mut prompts = vec![user_msg];
            let mut history: Vec<Message> = Vec::new();
            let mut turns_used = 0usize;
            let mut usage = Usage::default();
            let switch_auth_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(ChildRunError::TimedOut(timeout));
                }
                let attempt_budget = max_turns.saturating_sub(turns_used).max(1);
                let mut config = base_config.clone();
                config.max_turns = Some(attempt_budget);

                // Soft landing (steer to finalize two turns before the cap)
                // plus per-turn Lead/Worker routing for this attempt.
                {
                    let budget = attempt_budget;
                    let lw_hook = lead_worker.clone();
                    let api_key_cache_hook = self.api_key_cache.clone();
                    let switch_auth_err_hook = switch_auth_err.clone();
                    let hook_cancel = child_cancel.clone();
                    config.prepare_next_turn = Some(Arc::new(move |turn_ctx, _cancel| {
                        let lw_hook = lw_hook.clone();
                        let api_key_cache_hook = api_key_cache_hook.clone();
                        let switch_auth_err_hook = switch_auth_err_hook.clone();
                        let hook_cancel = hook_cancel.clone();
                        Box::pin(async move {
                            let mut update = AgentLoopTurnUpdate::default();
                            if budget >= 3 && turn_ctx.turn_index == budget - 2 {
                                update.messages = Some(vec![Message::User(UserMessage {
                                    content: UserContent::Text(
                                        "Budget notice: you have 2 assistant turns left. Stop expanding the search; finalize your answer now with exact file paths and concrete findings.".to_string(),
                                    ),
                                    timestamp: now_ms(),
                                })]);
                            }
                            if let Some(ref lw) = lw_hook {
                                let switch: Option<
                                    (mehh_ai::Model, Arc<dyn LlmProvider>, Option<String>),
                                > = {
                                    // The guard ends with this block so the
                                    // future stays `Send` across the credential
                                    // await below.
                                    let mut m = lw.lock().unwrap_or_else(PoisonError::into_inner);
                                    let is_empty_assistant =
                                        turn_ctx.assistant_message.content.is_empty();
                                    let repetition_detected =
                                        turn_ctx.tool_results.iter().any(|tr| {
                                            tr.is_error
                                                && tr.content.iter().any(|b| match b {
                                                    mehh_ai::ToolResultContentBlock::Text(t) => {
                                                        t.text.contains("Repeated identical tool call")
                                                    }
                                                    _ => false,
                                                })
                                        });
                                    m.record_turn_outcome(
                                        turn_ctx.assistant_message.stop_reason,
                                        is_empty_assistant || repetition_detected,
                                    );
                                    if m.is_active() {
                                        let next_model = m.active_model();
                                        if next_model.id != turn_ctx.assistant_message.model
                                            || next_model.provider
                                                != turn_ctx.assistant_message.provider
                                        {
                                            Some((
                                                next_model,
                                                m.active_provider(),
                                                m.active_api_key(),
                                            ))
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                };
                                if let Some((next_model, next_provider, active_key)) = switch {
                                    let next_key = match active_key {
                                        Some(key) => Some(key),
                                        None if next_model.provider == "faux" => None,
                                        None => match Self::resolve_cached_api_key(
                                            &api_key_cache_hook,
                                            &next_model.provider,
                                        )
                                        .await
                                        {
                                            Ok(key) => key,
                                            Err(e) => {
                                                *mehh_ai::lock_recover(&switch_auth_err_hook) =
                                                    Some(e);
                                                hook_cancel.cancel();
                                                return None;
                                            }
                                        },
                                    };
                                    update.model = Some(next_model);
                                    update.provider = Some(next_provider);
                                    update.api_key = Some(next_key);
                                }
                            }
                            if update.messages.is_some() || update.model.is_some() {
                                Some(update)
                            } else {
                                None
                            }
                        })
                    }));
                }

                let loop_fut = run_agent_loop(prompts, history, config, Some(listener.clone()));
                tokio::pin!(loop_fut);
                let sleep = tokio::time::sleep(remaining);
                tokio::pin!(sleep);
                let messages = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        child_cancel.cancel();
                        let _ = tokio::time::timeout(SUBAGENT_KILL_GRACE, &mut loop_fut).await;
                        return Err(ChildRunError::Aborted);
                    }
                    msgs = &mut loop_fut => msgs,
                    _ = &mut sleep => {
                        // Cancel BEFORE dropping the loop future so cancellation-aware
                        // tools (bash process trees, MCP requests) can clean up; only
                        // drop after the grace period if the loop fails to unwind.
                        child_cancel.cancel();
                        let _ = tokio::time::timeout(SUBAGENT_CANCEL_GRACE, &mut loop_fut).await;
                        return Err(ChildRunError::TimedOut(timeout));
                    }
                };
                // Cancellation can also arrive after the loop future became
                // ready. Do not finalize that result as Completed, and never
                // re-poll `loop_fut` after it already completed in `select!`.
                if cancel.is_cancelled() {
                    child_cancel.cancel();
                    return Err(ChildRunError::Aborted);
                }
                if let Some(auth_err) = mehh_ai::lock_recover(&switch_auth_err).take() {
                    return Err(ChildRunError::ModelError(auth_err));
                }

                for m in &messages {
                    if let Message::Assistant(a) = m {
                        turns_used += 1;
                        accumulate_usage(&mut usage, &a.usage);
                    }
                }
                all_messages.extend(messages);

                let last_ast = all_messages.iter().rev().find_map(|m| match m {
                    Message::Assistant(a) => Some(a),
                    _ => None,
                });
                match last_ast {
                    Some(a) if a.stop_reason == StopReason::Error => {
                        // Rescue: a failed Worker turn may hand the run to the Lead
                        // for a bounded window, continuing from the same evidence.
                        let mut rescued = false;
                        if let Some(ref lw) = lead_worker {
                            let rescue_target = {
                                let mut m = lw.lock().unwrap_or_else(PoisonError::into_inner);
                                // Capture the role of the turn that actually failed BEFORE
                                // recording its outcome: a failing Lead turn is not a Worker
                                // escalation and must never hand the run back to itself.
                                let was_worker = m.should_use_worker();
                                m.record_turn_outcome(StopReason::Error, false);
                                if was_worker
                                    && (m.lead_rescue_remaining > 0 || m.locked_to_lead)
                                    && turns_used < max_turns
                                {
                                    Some((m.active_model(), m.active_provider(), m.active_api_key()))
                                } else {
                                    None
                                }
                            };
                            if let Some((next_model, next_provider, active_key)) = rescue_target {
                                // A different provider may have no key on the manager
                                // (auth.json/OAuth only); resolve it before using it.
                                let rescue_key = match active_key {
                                    Some(key) => Some(key),
                                    None if next_model.provider == "faux" => None,
                                    None => self
                                        .resolved_api_key(&next_model.provider)
                                        .await
                                        .map_err(ChildRunError::ModelError)?,
                                };
                                base_config.model = next_model;
                                base_config.provider = next_provider;
                                base_config.stream_options.api_key = rescue_key;
                                manifest.model_id = base_config.model.id.clone();
                                rescued = true;
                            }
                        }
                        if rescued {
                            while let Some(Message::Assistant(last_a)) = all_messages.last() {
                                if last_a.stop_reason == StopReason::Error
                                    && last_a.text_content().trim().is_empty()
                                    && !last_a
                                        .content
                                        .iter()
                                        .any(|b| matches!(b, mehh_ai::AssistantContentBlock::ToolCall(_)))
                                {
                                    all_messages.pop();
                                } else {
                                    break;
                                }
                            }
                            history = all_messages.clone();
                            prompts = vec![Message::User(UserMessage {
                                content: UserContent::Text(RESCUE_PROMPT.to_string()),
                                timestamp: now_ms(),
                            })];
                            continue;
                        }
                        return Err(ChildRunError::ModelError(
                            a.error_message
                                .clone()
                                .unwrap_or_else(|| "Subagent model call failed".to_string()),
                        ));
                    }
                    Some(a) if a.stop_reason == StopReason::Aborted => {
                        return Err(ChildRunError::Aborted);
                    }
                    _ => break,
                }
            }

            let files_touched = collect_files_touched(&all_messages);
            let budget_reached = turns_used >= max_turns;
            let final_ast = all_messages.iter().rev().find_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            });

            match final_ast {
                Some(a) => {
                    let text = a.text_content();
                    if !text.trim().is_empty() {
                        return Ok(ChildOutcome {
                            summary: text,
                            status: if budget_reached { "budget_exhausted" } else { "completed" },
                            turns_used,
                            usage,
                            files_touched,
                        });
                    }
                    // Force-finalize: the budget ended on a tool-call-only message.
                    // One extra LLM call with no tools must produce a usable summary.
                    // The history is consumed here; nothing after this point needs it.
                    let mut fin_messages = all_messages;
                    fin_messages.push(Message::User(UserMessage {
                        content: UserContent::Text(FINALIZE_PROMPT.to_string()),
                        timestamp: now_ms(),
                    }));
                    let fin_ctx = Context {
                        system_prompt: base_config.system_prompt.clone(),
                        messages: fin_messages,
                        tools: None,
                    };
                    if cancel.is_cancelled() {
                        return Err(ChildRunError::Aborted);
                    }
                    let finalize_budget = finalize_wait_budget(
                        deadline.saturating_duration_since(tokio::time::Instant::now()),
                    );
                    if finalize_budget.is_zero() {
                        return Err(ChildRunError::TimedOut(timeout));
                    }
                    if let Some(ref lw) = lead_worker {
                        let (next_model, next_provider, active_key) = {
                            let m = lw.lock().unwrap_or_else(PoisonError::into_inner);
                            (m.active_model(), m.active_provider(), m.active_api_key())
                        };
                        let fin_key = match active_key {
                            Some(key) => Some(key),
                            None if next_model.provider == "faux" => None,
                            None => self
                                .resolved_api_key(&next_model.provider)
                                .await
                                .map_err(ChildRunError::ModelError)?,
                        };
                        base_config.model = next_model;
                        base_config.provider = next_provider;
                        base_config.stream_options.api_key = fin_key;
                        manifest.model_id = base_config.model.id.clone();
                    }
                    let mut stream = base_config.provider.stream(
                        &base_config.model,
                        &fin_ctx,
                        &base_config.stream_options,
                        child_cancel.clone(),
                    );
                    let mut summary = match tokio::time::timeout(
                        finalize_budget,
                        stream.collect_final_message(),
                    )
                    .await
                    {
                        Err(_) => {
                            if cancel.is_cancelled() {
                                return Err(ChildRunError::Aborted);
                            }
                            return Err(ChildRunError::TimedOut(timeout));
                        }
                        Ok(Ok(fin)) => {
                            if fin.stop_reason == StopReason::Aborted || cancel.is_cancelled() {
                                return Err(ChildRunError::Aborted);
                            }
                            if fin.stop_reason == StopReason::Error {
                                return Err(ChildRunError::ModelError(
                                    fin.error_message.clone().unwrap_or_else(|| {
                                        "Subagent finalization call failed".to_string()
                                    }),
                                ));
                            }
                            if !fin.text_content().trim().is_empty() {
                                accumulate_usage(&mut usage, &fin.usage);
                                turns_used += 1;
                                fin.text_content()
                            } else {
                                text
                            }
                        }
                        Ok(Err(e)) => {
                            if cancel.is_cancelled() {
                                return Err(ChildRunError::Aborted);
                            }
                            if deadline
                                .saturating_duration_since(tokio::time::Instant::now())
                                .is_zero()
                            {
                                return Err(ChildRunError::TimedOut(timeout));
                            }
                            return Err(ChildRunError::ModelError(e.to_string()));
                        }
                    };
                    if summary.trim().is_empty() {
                        summary = "(subagent produced no summary)".to_string();
                    }
                    Ok(ChildOutcome {
                        summary,
                        status: if budget_reached { "budget_exhausted" } else { "partial" },
                        turns_used,
                        usage,
                        files_touched,
                    })
                }
                None => Err(ChildRunError::NoResponse),
            }
        }
        .await;

        let text_outcome = outcome
            .as_ref()
            .map(|o| o.result_text())
            .map_err(|e| e.clone());
        Self::finalize_subagent_manifest(
            &self.cwd,
            manifest,
            &events,
            &text_outcome,
            &cancel,
            on_update,
            &task_prefix,
        )
        .map_err(ChildRunError::ManifestWrite)?;
        outcome
    }
}

pub(super) fn append_web_tool_guidance(sys: &mut String, tools: &[Arc<dyn AgentTool>]) {
    let has_search = tools.iter().any(|t| t.name() == "web_search");
    let has_fetch = tools.iter().any(|t| t.name() == "web_fetch");
    match (has_search, has_fetch) {
        (true, true) => sys.push_str(
            " You also have live web tools: use web_search to search external docs, projects, or current info, and web_fetch to read HTTP/HTTPS URLs.",
        ),
        (true, false) => sys.push_str(
            " You also have web_search available to search external docs, projects, or current info on the web.",
        ),
        (false, true) => sys.push_str(
            " You also have web_fetch available to fetch and read HTTP/HTTPS URLs.",
        ),
        (false, false) => {}
    }
}
