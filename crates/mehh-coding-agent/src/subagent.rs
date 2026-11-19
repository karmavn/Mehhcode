//! Depth-bounded, multi-mode subagent tool for delegated tasks and parallel execution.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use mehh_agent::{
    run_agent_loop, AgentEvent, AgentLoopConfig, AgentLoopTurnUpdate, AgentTool, AgentToolResult,
    AgentToolUpdateCallback, QueueMode,
};
use mehh_ai::{now_ms, Context, LlmProvider, Message, Model, StopReason, Usage, UserContent, UserMessage};

use crate::lead_worker::LeadWorkerManager;
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

/// Wall-clock budget for the forced finalization summary call in seconds.
pub const SUBAGENT_FINALIZE_TIMEOUT_SECS: u64 = 60;

/// Grace period after timeout cancellation for tools to kill process trees
/// and abort in-flight requests before the loop future is dropped.
const SUBAGENT_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Shorter grace for an explicit kill: the caller wants the child stopped
/// now, so cleanup is best-effort before the loop future is dropped.
const SUBAGENT_KILL_GRACE: Duration = Duration::from_millis(500);

/// RAII guard: cancels the child token whenever the run scope exits —
/// timeout, error, or the parent dropping this future mid-run.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Instruction injected when the turn budget ends without a concluding text answer.
const FINALIZE_PROMPT: &str = "Your turn budget is exhausted and you must not call any more tools. Based only on the evidence already gathered above, write the final answer now: a concise summary of findings or changes, with exact file paths. If nothing was completed, state what was attempted and what remains.";

/// Instruction injected when a Lead rescue takes over after a Worker failure.
const RESCUE_PROMPT: &str = "The previous model failed while working on this task. You are taking over with a fresh perspective. Review the evidence gathered so far, resolve the blocker, and complete the original task. Return a concise final answer with exact file paths.";

/// Structured result of one subagent run: summary plus budget/telemetry handoff.
struct ChildOutcome {
    summary: String,
    status: &'static str,
    turns_used: usize,
    usage: Usage,
    files_touched: Vec<String>,
}

impl ChildOutcome {
    fn metadata_json(&self) -> Value {
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

    fn result_text(&self) -> String {
        format!("{}\n\n{}", self.summary, self.status_line())
    }
}

fn accumulate_usage(total: &mut Usage, u: &Usage) {
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
            for tc in a.tool_calls() {
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
    files
}

/// Per-run inputs for one child subagent invocation.
struct ChildRunParams<'a> {
    prompt: &'a str,
    mode: &'a str,
    custom_sys: Option<&'a str>,
    max_turns: usize,
    task_id: Option<usize>,
    model_tier: Option<&'a str>,
    timeout: Duration,
    steering_queue: Option<Arc<Mutex<VecDeque<Message>>>>,
}

/// Shared in-memory notes for one batch fan-out.
type Scratchpad = Arc<RwLock<HashMap<String, String>>>;

const SCRATCHPAD_MAX_KEYS: usize = 64;
const SCRATCHPAD_MAX_VALUE_CHARS: usize = 4096;

/// Publishes a short note to the batch scratchpad. Memory-only: no workspace
/// side effects, hence annotated read-only so research/review children get it.
struct NoteWriteTool {
    pad: Scratchpad,
}

#[async_trait]
impl AgentTool for NoteWriteTool {
    fn name(&self) -> &str {
        "note_write"
    }
    fn description(&self) -> &str {
        "Save a short note (key, value) to the scratchpad shared with sibling tasks in this batch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Note identifier." },
                "value": { "type": "string", "description": "Short finding to share (truncated to 4096 chars)." }
            },
            "required": ["key", "value"]
        })
    }
    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'key' parameter".to_string())?;
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required 'value' parameter".to_string())?;
        let value: String = value.chars().take(SCRATCHPAD_MAX_VALUE_CHARS).collect();
        let mut pad = self.pad.write().await;
        if !pad.contains_key(key) && pad.len() >= SCRATCHPAD_MAX_KEYS {
            return Err(format!("Scratchpad full ({SCRATCHPAD_MAX_KEYS} keys)"));
        }
        pad.insert(key.to_string(), value);
        Ok(AgentToolResult::text(format!("note '{key}' saved")))
    }
}

/// Reads a note from the batch scratchpad.
struct NoteReadTool {
    pad: Scratchpad,
}

#[async_trait]
impl AgentTool for NoteReadTool {
    fn name(&self) -> &str {
        "note_read"
    }
    fn description(&self) -> &str {
        "Read a note by key from the scratchpad shared with sibling tasks in this batch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Note identifier." }
            },
            "required": ["key"]
        })
    }
    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'key' parameter".to_string())?;
        let pad = self.pad.read().await;
        match pad.get(key) {
            Some(v) => Ok(AgentToolResult::text(v.clone())),
            None => Ok(AgentToolResult::text(format!("(no note for key '{key}')"))),
        }
    }
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
            t.text = t.text.replacen("[subagent:", &format!("[subagent:{prefix}"), 1);
        }
    }
    upd
}

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
    /// Optional Lead/Worker routing template; each child run clones its own state.
    pub lead_worker: Option<Arc<Mutex<LeadWorkerManager>>>,
    /// Shared notes for batch siblings; set per fan-out, None for single runs.
    pub scratchpad: Option<Scratchpad>,
    /// Default wall-clock budget for one child run (loop + cleanup grace).
    pub timeout: Duration,
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
            lead_worker: None,
            scratchpad: None,
            timeout: Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
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
                    "description": "Optional list of independent tasks to execute concurrently (up to 4 in parallel; coding tasks run sequentially on the shared working tree). Batch tasks share a scratchpad via note_write/note_read."
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
    /// Coding excludes recursive tools ('subagent', 'codemode', 'todo').
    /// Research/review admit any tool annotated read-only (including read-only
    /// MCP tools), fail closed on unannotated tools, and always exclude
    /// recursive/stateful tools regardless of their annotation.
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
                .filter(|t| {
                    !matches!(t.name(), "subagent" | "codemode" | "todo")
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
    fn depth_corrected_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Vec<Arc<dyn AgentTool>> {
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
                Some(Arc::new(SubagentTool {
                    cwd: self.cwd.clone(),
                    model: self.model.clone(),
                    provider: self.provider.clone(),
                    api_key: self.api_key.clone(),
                    tools: self.tools.clone(),
                    depth: nested_depth,
                    inspection_manager: self.inspection_manager.clone(),
                    approval_handler: self.approval_handler.clone(),
                    control: self.control.clone(),
                    lead_worker: self.lead_worker.clone(),
                    scratchpad: self.scratchpad.clone(),
                    timeout: self.timeout,
                }) as Arc<dyn AgentTool>)
            })
            .collect()
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
        params: ChildRunParams<'_>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
        manifest: &mut SubagentManifest,
        events: Arc<Mutex<Vec<SubagentEvent>>>,
    ) -> Result<ChildOutcome, String> {
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
        let mut sys = custom_sys.map(str::to_string).unwrap_or(default_sys);
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
            cb(prefixed_update(&child_agent_id, mode, &started_event, &task_prefix));
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
                    events.lock().unwrap().push(ev.clone());
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
                    events.lock().unwrap().push(ev.clone());
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
                let mut m = template.lock().unwrap().clone();
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
            let m = lw.lock().unwrap();
            base_config.model = m.active_model();
            base_config.provider = m.active_provider();
            let active_key = m.active_api_key();
            base_config.stream_options.api_key=active_key;
        }

        if let Some(ref lw) = lead_worker {
            let lw = lw.clone();
            base_config.after_tool_call = Some(Arc::new(move |after_ctx, _cancel| {
                let lw = lw.clone();
                Box::pin(async move {
                    lw.lock().unwrap().record_tool_outcome(after_ctx.is_error);
                    mehh_agent::AfterToolCallResult::default()
                })
            }));
        }

        if let Some(ref mgr) = self.inspection_manager {
            let mgr = mgr.clone();
            let session_cwd = self.cwd.clone();
            let approval_h = self.approval_handler.clone();
            let origin_label = format!("subagent:{child_agent_id}:{mode}");
            base_config.before_tool_call = Some(Arc::new(move |before_ctx, _cancel| {
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
                        .and_then(|t| t.is_read_only());

                    let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                        tool_call_id,
                        tool_name,
                        args: before_ctx.args,
                        is_read_only,
                        cwd: session_cwd,
                        origin: Some(origin_label),
                    };
                    mgr.inspect_and_authorize(&inspect_ctx, approval_h.as_deref()).await
                })
            }));
        }

        if lead_worker.is_none() {
            if let Some(ref key) = self.api_key {
                base_config.stream_options.api_key=Some(key.clone());
            } else if self.model.provider != "faux" && self.model.api != "faux" {
                if let Ok(Some(key)) =
                    crate::auth::resolve_api_key_async(&self.model.provider, None, None).await
                {
                    base_config.stream_options.api_key=Some(key);
                }
            }
        } else if base_config.stream_options.api_key.is_none()
            && base_config.model.provider != "faux"
            && base_config.model.api != "faux"
        {
            if let Ok(Some(key)) =
                crate::auth::resolve_api_key_async(&base_config.model.provider, None, None).await
            {
                base_config.stream_options.api_key=Some(key);
            }
        }

        if let Some(ref queue) = steering_queue {
            base_config.steering_queue = Arc::clone(queue);
            base_config.steering_mode = QueueMode::OneAtATime;
        }

        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(task_prompt.to_string()),
            timestamp: now_ms(),
        });

        let outcome: Result<ChildOutcome, String> = async {
            let child_cancel = cancel.child_token();
            let _cancel_guard = CancelOnDrop(child_cancel.clone());
            base_config.cancel = child_cancel.clone();

            let deadline = tokio::time::Instant::now() + timeout;
            let mut all_messages: Vec<Message> = Vec::new();
            let mut prompts = vec![user_msg];
            let mut history: Vec<Message> = Vec::new();
            let mut turns_used = 0usize;
            let mut usage = Usage::default();

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(format!("Subagent timed out after {timeout:?}"));
                }
                let attempt_budget = max_turns.saturating_sub(turns_used).max(1);
                let mut config = base_config.clone();
                config.max_turns = Some(attempt_budget);

                // Soft landing (steer to finalize two turns before the cap)
                // plus per-turn Lead/Worker routing for this attempt.
                {
                    let budget = attempt_budget;
                    let lw_hook = lead_worker.clone();
                    config.prepare_next_turn = Some(Arc::new(move |turn_ctx, _cancel| {
                        let lw_hook = lw_hook.clone();
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
                                let mut m = lw.lock().unwrap();
                                let is_empty_assistant = turn_ctx.assistant_message.content.is_empty();
                                let repetition_detected = turn_ctx.tool_results.iter().any(|tr| {
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
                                        || next_model.provider != turn_ctx.assistant_message.provider
                                    {
                                        let next_key = m.active_api_key();
                                        update.model = Some(next_model);
                                        update.provider = Some(m.active_provider());
                                        update.api_key=Some(next_key);
                                    }
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
                    msgs = &mut loop_fut => msgs,
                    _ = &mut sleep => {
                        // Cancel BEFORE dropping the loop future so cancellation-aware
                        // tools (bash process trees, MCP requests) can clean up; only
                        // drop after the grace period if the loop fails to unwind.
                        child_cancel.cancel();
                        let _ = tokio::time::timeout(SUBAGENT_CANCEL_GRACE, &mut loop_fut).await;
                        return Err(format!("Subagent timed out after {timeout:?}"));
                    }
                    _ = cancel.cancelled() => {
                        child_cancel.cancel();
                        let _ = tokio::time::timeout(SUBAGENT_KILL_GRACE, &mut loop_fut).await;
                        return Err("Subagent execution was aborted".to_string());
                    }
                };

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
                            let mut m = lw.lock().unwrap();
                            m.record_turn_outcome(StopReason::Error, false);
                            if m.lead_rescue_remaining > 0 && turns_used < max_turns {
                                let rescue_key = m.active_api_key();
                                base_config.model = m.active_model();
                                base_config.provider = m.active_provider();
                                base_config.stream_options.api_key=rescue_key;
                                rescued = true;
                            }
                        }
                        if rescued {
                            history = all_messages.clone();
                            prompts = vec![Message::User(UserMessage {
                                content: UserContent::Text(RESCUE_PROMPT.to_string()),
                                timestamp: now_ms(),
                            })];
                            continue;
                        }
                        return Err(a
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "Subagent model call failed".to_string()));
                    }
                    Some(a) if a.stop_reason == StopReason::Aborted => {
                        return Err("Subagent execution was aborted".to_string());
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
                    let mut fin_messages = all_messages.clone();
                    fin_messages.push(Message::User(UserMessage {
                        content: UserContent::Text(FINALIZE_PROMPT.to_string()),
                        timestamp: now_ms(),
                    }));
                    let fin_ctx = Context {
                        system_prompt: base_config.system_prompt.clone(),
                        messages: fin_messages,
                        tools: None,
                    };
                    let mut stream = base_config.provider.stream(
                        &base_config.model,
                        &fin_ctx,
                        &base_config.stream_options,
                        child_cancel.clone(),
                    );
                    let mut summary = match tokio::time::timeout(
                        Duration::from_secs(SUBAGENT_FINALIZE_TIMEOUT_SECS),
                        stream.collect_final_message(),
                    )
                    .await
                    {
                        Ok(Ok(fin)) if !fin.text_content().trim().is_empty() => {
                            accumulate_usage(&mut usage, &fin.usage);
                            turns_used += 1;
                            fin.text_content()
                        }
                        _ => text,
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
                None => Err("Subagent produced no assistant response".to_string()),
            }
        }
        .await;

        let text_outcome = outcome
            .as_ref()
            .map(|o| o.result_text())
            .map_err(|e| e.clone());
        Self::finalize_subagent_run(
            &self.cwd,
            manifest,
            &events,
            text_outcome,
            &cancel,
            on_update,
        )?;
        outcome
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
            let task_prompts: Vec<String> = tasks_arr
                .iter()
                .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                .collect();

            if !task_prompts.is_empty() {
                let control = self.control();
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
                    let cancel_token = cancel.clone();
                    let on_update_child = on_update.clone();
                    let mode_str = mode.to_string();
                    let custom_sys_owned = custom_sys.map(str::to_string);
                    let model_tier_owned = model_tier.map(str::to_string);
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
                        lead_worker: self.lead_worker.clone(),
                        scratchpad: Some(batch_pad.clone()),
                        timeout: self.timeout,
                    };
                    let task_agent_id = new_agent_id();
                    task_ids.push(task_agent_id.clone());
                    let mut task_manifest_for_task =
                        new_manifest(task_agent_id.clone(), mode_str.clone(), self.model.id.clone(), now_ms());
                    child_manifests.push(task_manifest_for_task.clone());
                    child_statuses.push(SubagentStatus::Running);
                    // Shared with the spawned task so the parent can finalize
                    // the manifest from the child's live event log even when
                    // the child panics before finalizing it itself.
                    let task_events =
                        Arc::new(Mutex::new(task_manifest_for_task.events.clone()));
                    child_events.push(task_events.clone());

                    let task_control = control.clone();
                    let batch_sem = batch_semaphore.clone();
                    handles.push(tokio::spawn(async move {
                        let _local_permit = batch_sem.acquire().await.map_err(|e| e.to_string())?;
                        let _permit = task_control.acquire().await;
                        let handle = task_control.register(
                            task_agent_id,
                            mode_str.clone(),
                            cancel_token.child_token(),
                        );
                        if let Some(ref cb) = on_update_child {
                            cb(AgentToolResult::text(format!(
                                "[subagent:#{}:{mode_str}] Starting task",
                                idx + 1
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
                        task_control.unregister(&handle.agent_id);
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
                            task_meta.push(json!({"task": idx + 1, "status": "error", "error": e}));
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
                result.structured_content = Some(json!({"tasks": task_meta, "usage": total_usage}));
                result.usage = Some(total_usage);
                if let Some(child_manifest) = child_manifests.last() {
                    let path = agent_store_dir(&self.cwd).join(format!("{}.json", child_manifest.agent_id));
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
            lead_worker: self.lead_worker.clone(),
            scratchpad: self.scratchpad.clone(),
            timeout: self.timeout,
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
        let (result_text, is_error, handoff, usage) = match outcome {
            Ok(o) => (o.result_text(), false, Some(o.metadata_json()), Some(o.usage.clone())),
            Err(err) => (err, true, None, None),
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

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};

    #[tokio::test]
    async fn test_nested_subagent_tool_is_rebuilt_at_child_depth() {
        let cwd = std::env::temp_dir();
        let faux = Arc::new(FauxProvider::new());
        faux.add_response(faux_assistant_message("nested ok"));
        // The inherited arc sits at the depth ceiling and would refuse to run.
        let stale = Arc::new(SubagentTool::new(
            cwd.clone(),
            faux_model(),
            faux.clone(),
            None,
            mehh_tools::create_coding_tools(cwd.clone()),
            MAX_SUBAGENT_DEPTH,
        ));
        let mut tools: Vec<Arc<dyn AgentTool>> = mehh_tools::create_coding_tools(cwd.clone());
        tools.push(stale);

        let sub = SubagentTool::new(cwd.clone(), faux_model(), faux.clone(), None, tools.clone(), 0);
        let corrected = sub.depth_corrected_tools(tools.clone());
        let nested = corrected
            .iter()
            .find(|t| t.name() == "subagent")
            .expect("depth-0 child may expose one nesting level");
        let mut params = Map::new();
        params.insert("prompt".to_string(), Value::String("nested task".to_string()));
        let res = nested
            .execute("tc_nested", params, CancellationToken::new(), None)
            .await
            .expect("rebuilt nested tool must run at depth 1, not the stale ceiling depth");
        let text = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!("Expected text output"),
        };
        assert!(text.starts_with("nested ok"));

        // At the ceiling, the nested tool must be hidden entirely so the LLM
        // never wastes a turn calling it only to receive a depth error.
        let deep = SubagentTool::new(
            cwd,
            faux_model(),
            faux,
            None,
            tools.clone(),
            MAX_SUBAGENT_DEPTH - 1,
        );
        let corrected_deep = deep.depth_corrected_tools(tools);
        assert!(corrected_deep.iter().all(|t| t.name() != "subagent"));
    }

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
        assert!(text.starts_with("Found 3 matching files."));
        assert!(text.contains("[subagent-meta] status=completed"));

        // Depth at MAX_SUBAGENT_DEPTH fails closed
        let deep = SubagentTool::new(cwd, faux_model(), faux, None, tools, MAX_SUBAGENT_DEPTH);
        let err = deep
            .execute("tc_sub_2", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("Maximum subagent nesting depth"));
    }

    #[tokio::test]
    async fn batch_details_lists_every_child_with_its_own_terminal_status() {
        // Route by task text so each child's outcome is deterministic even
        // though batch children are spawned concurrently.
        fn route(
            _model: &mehh_ai::Model,
            context: &mehh_ai::Context,
            _options: &mehh_ai::StreamOptions,
        ) -> mehh_ai::AssistantMessage {
            let is_fail = context
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::User(u) => match &u.content {
                        UserContent::Text(t) => Some(t.contains("FAIL")),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or(false);
            if is_fail {
                let mut msg = faux_assistant_message("child failed");
                msg.stop_reason = StopReason::Error;
                msg.error_message = Some("simulated failure".to_string());
                msg
            } else {
                faux_assistant_message("child ok")
            }
        }

        let faux = Arc::new(FauxProvider::new());
        faux.add_handler(route);
        faux.add_handler(route);
        faux.add_handler(route);

        let cwd = std::env::temp_dir();
        let tools = mehh_tools::create_coding_tools(cwd.clone());
        let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);

        let mut params = Map::new();
        params.insert(
            "tasks".to_string(),
            Value::Array(vec![
                Value::String("OK one".into()),
                Value::String("FAIL two".into()),
                Value::String("OK three".into()),
            ]),
        );
        let res = sub
            .execute("tc_batch", params, CancellationToken::new(), None)
            .await
            .unwrap();

        let details = res.details.expect("batch result must carry details");
        let sub_details = details.get("subagent").expect("subagent details");
        assert_eq!(
            sub_details.get("status").and_then(|v| v.as_str()),
            Some("failed"),
            "aggregate status reflects any failure"
        );
        // Legacy keys still point at the last child.
        assert_eq!(
            sub_details.get("child_id").and_then(|v| v.as_str()),
            sub_details
                .get("children")
                .and_then(|v| v.as_array())
                .and_then(|a| a.last())
                .and_then(|c| c.get("child_id"))
                .and_then(|v| v.as_str()),
            "legacy child_id stays the last child"
        );

        let children = sub_details
            .get("children")
            .and_then(|v| v.as_array())
            .expect("children array");
        assert_eq!(children.len(), 3, "one entry per task");

        let mut statuses = std::collections::HashMap::new();
        for child in children {
            let id = child
                .get("child_id")
                .and_then(|v| v.as_str())
                .expect("child_id")
                .to_string();
            let status = child
                .get("status")
                .and_then(|v| v.as_str())
                .expect("status")
                .to_string();
            assert!(
                child.get("manifest").and_then(|v| v.as_str()).is_some(),
                "each child carries its own manifest path"
            );
            assert!(statuses.insert(id, status).is_none(), "child ids are unique");
        }
        let failed = statuses.iter().filter(|(_, s)| s.as_str() == "failed").count();
        let completed = statuses.iter().filter(|(_, s)| s.as_str() == "completed").count();
        assert_eq!(failed, 1, "the FAIL task is failed: {statuses:?}");
        assert_eq!(completed, 2, "both OK tasks are completed: {statuses:?}");
    }
}
