use super::*;

/// One child row inside a nested subagent group, keyed by `child_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentChildRow {
    pub child_id: String,
    pub label: String,
    pub status: String,
    pub detail: String,
}

/// Transcript display entry in the interactive session.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    User(String),
    Assistant {
        thinking: String,
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        status: String,
        is_error: bool,
        args_summary: String,
        output_preview: Vec<String>,
        diff: Option<String>,
        duration_ms: Option<u64>,
    },
    /// Nested subagent tool call with its per-child progress rows.
    Subagent {
        id: String,
        mode: String,
        status: String,
        is_error: bool,
        children: Vec<SubagentChildRow>,
    },
    System(String),
}

/// The kind of picker overlay currently open above the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind {
    Model,
    Mode,
    Thinking,
    Theme,
    Tree,
    Resume,
    Settings,
    Login,
    ConnectMethod,
    ConnectInput,
    ToolApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectInputKind {
    ApiKey {
        provider: String,
        display_name: String,
    },
    CustomProviderId,
    CustomBaseUrl {
        provider_id: String,
    },
    CustomApiKey {
        provider_id: String,
        base_url: String,
    },
}

#[derive(Debug, Clone)]
pub struct ConnectInputState {
    pub kind: ConnectInputKind,
    pub value: String,
    pub masked: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    OpenModelPicker,
    OpenConnectProvider,
    CycleThinking,
    OpenThinkingPicker,
    OpenModePicker,
    OpenSessionSwitcher,
    NewSession,
    OpenSettings,
    OpenHelp,
    ScrollToBottom,
    SelectModalRow(usize),
    ClickSettingsRow {
        filtered_idx: usize,
        setting_id: String,
    },
    ApproveToolChoice(String),
    SelectAutocompleteRow(usize),
    CloseModal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickRegion {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub action: ClickAction,
}

/// A picker overlay: the list of choices plus the context needed to apply one.
#[derive(Debug, Clone)]
pub struct Selector {
    pub kind: SelectorKind,
    pub list: SelectList,
    pub settings_list: Option<SettingsList>,
}

#[derive(Clone)]
pub(super) struct StreamingMarkdownPrefix {
    pub(super) prefix_byte_len: usize,
    pub(super) prefix_hash: [u8; 16],
    pub(super) prefix_lines: Vec<String>,
    pub(super) width: usize,
    pub(super) theme_name: String,
    pub(super) theme_generation: u64,
}

pub(super) struct CachedTranscriptEntry {
    pub(super) width: usize,
    pub(super) flags: u8,
    pub(super) frame: usize,
    pub(super) thinking_elapsed: u64,
    pub(super) theme_name: String,
    pub(super) theme_generation: u64,
    pub(super) item_hash: [u8; 16],
    pub(super) thinking_hash: [u8; 16],
    pub(super) thinking_lines: Arc<[String]>,
    pub(super) text_hash: [u8; 16],
    pub(super) text_lines: Arc<[String]>,
    pub(super) streaming_md: Option<StreamingMarkdownPrefix>,
    pub(super) lines: Arc<[String]>,
}

pub(super) type TranscriptCache = Vec<Option<CachedTranscriptEntry>>;

/// Extracts only `[start..end]` lines from cached transcript segments so off-screen
/// turns never allocate or clone strings during an 80ms frame tick.
pub(super) fn slice_transcript_segments(
    segments: &[Arc<[String]>],
    start: usize,
    end: usize,
) -> Vec<String> {
    if start >= end {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(end - start);
    let mut cursor = 0usize;
    for seg in segments {
        let seg_end = cursor + seg.len();
        if seg_end > start && cursor < end {
            let lo = start.saturating_sub(cursor);
            let hi = end.min(seg_end) - cursor;
            out.extend(seg[lo..hi].iter().cloned());
        }
        cursor = seg_end;
        if cursor >= end {
            break;
        }
    }
    out
}

pub(super) fn anchored_scroll_offset(anchor: usize, growth: usize, offset: usize) -> usize {
    if offset == 0 {
        0
    } else {
        anchor.saturating_add(growth)
    }
}

fn apply_anchored_scroll(state: &mut InteractiveState, anchor: usize, next_total: usize) {
    if next_total == 0 {
        state.scroll_offset = 0;
        state.scroll_anchor.store(0, Ordering::Relaxed);
    } else {
        // Transfer scroll budget from `scroll_anchor` while keeping
        // `scroll_offset >= 1` so `render_interactive_frame` preserves the anchor.
        let new_anchor = anchor.min(next_total.saturating_sub(1));
        state.scroll_anchor.store(new_anchor, Ordering::Relaxed);
        state.scroll_offset = next_total - new_anchor;
    }
}

/// Scrolls up while staying consistent with the combined `scroll_offset + scroll_anchor` distance.
pub(super) fn scroll_up_anchored(state: &mut InteractiveState, lines: usize, max_scroll: usize) {
    let anchor = state.scroll_anchor.load(Ordering::Relaxed);
    let next_total = state
        .scroll_offset
        .saturating_add(anchor)
        .saturating_add(lines)
        .min(max_scroll);
    apply_anchored_scroll(state, anchor, next_total);
}

/// Scrolls down smoothly without resetting `scroll_anchor` until the viewport reaches the bottom.
pub(super) fn scroll_down_anchored(state: &mut InteractiveState, lines: usize, max_scroll: usize) {
    let anchor = state.scroll_anchor.load(Ordering::Relaxed);
    let total = state.scroll_offset.saturating_add(anchor).min(max_scroll);
    let next_total = total.saturating_sub(lines);
    apply_anchored_scroll(state, anchor, next_total);
}


/// Context for an active tool approval request awaiting user decision.
pub struct PendingApproval {
    pub tool_name: String,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub reason: String,
    pub preview: Option<String>,
    pub reply_tx: tokio::sync::oneshot::Sender<bool>,
}

/// Interactive broker dispatching tool approval requests to the TUI event loop.
pub struct InteractiveApprovalBroker {
    tx: tokio::sync::mpsc::UnboundedSender<AgentSignal>,
    approval_lock: Arc<tokio::sync::Mutex<()>>,
}

impl InteractiveApprovalBroker {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<AgentSignal>) -> Self {
        Self {
            tx,
            approval_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait::async_trait]
impl crate::tool_inspection::ApprovalHandler for InteractiveApprovalBroker {
    async fn request_approval(
        &self,
        ctx: &crate::tool_inspection::ToolInspectionContext,
        reason: &str,
        preview: Option<&str>,
    ) -> bool {
        let _guard = self.approval_lock.lock().await;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let pending = PendingApproval {
            tool_name: ctx.tool_name.clone(),
            args: ctx.args.clone(),
            reason: reason.to_string(),
            preview: preview.map(str::to_string),
            reply_tx,
        };
        if self
            .tx
            .send(AgentSignal::ToolApprovalRequest(Box::new(pending)))
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }
}

/// State managed by the interactive mode session.
pub struct InteractiveState {
    pub items: Vec<TranscriptItem>,
    pub status: Option<String>,
    pub is_busy: bool,
    pub loader: Loader,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost: f64,
    pub selector: Option<Selector>,
    pub pending_approval: Option<PendingApproval>,
    pub mcp_summaries: Vec<McpConnectionSummary>,
    pub theme: Theme,
    pub keybindings: Keybindings,
    pub scroll_offset: usize,
    pub last_max_scroll: AtomicUsize,
    pub(crate) prev_transcript_len: AtomicUsize,
    pub(crate) scroll_anchor: AtomicUsize,
    pub(crate) turn_started_at: Mutex<Option<std::time::Instant>>,
    pub(crate) turn_completed_at: Mutex<Option<std::time::Instant>>,
    pub(crate) thinking_expanded: bool,
    pub(crate) thinking_elapsed: Mutex<std::collections::HashMap<usize, u64>>,
    pub last_prompt_text: Option<String>,
    /// Full expanded prompt of the current turn (expanded text + blocks from
    /// `@file` expansion). Kept as one value so the two cannot drift apart.
    /// Overflow auto-retries must resend this, not the raw submitted text.
    pub last_prompt_content: Option<(String, Vec<UserContentBlock>)>,
    pub overflow_retried: bool,
    pub turn_start_idx: Option<usize>,
    pub last_turn_error: Option<String>,
    /// File that settings changes are written to. `None` uses the global settings file.
    pub settings_file: Option<PathBuf>,
    /// File that credentials are written to. `None` uses the default auth file.
    pub auth_file: Option<PathBuf>,
    /// File that custom provider configs are loaded from/saved to. `None` uses default `models.json`.
    pub models_file: Option<PathBuf>,
    /// File that cached discovered provider models are stored in. `None` uses default `discovered-models.json`.
    pub discovered_models_file: Option<PathBuf>,
    /// Active inline modal input state (`SelectorKind::ConnectInput`).
    pub connect_input: Option<ConnectInputState>,
    /// Target provider ID selected in `/connect` when picking auth method (`SelectorKind::ConnectMethod`).
    pub connect_provider_target: Option<String>,
    /// Clickable UI regions collected during the most recent frame render.
    pub click_regions: Mutex<Vec<ClickRegion>>,
    /// Queued background model refresh provider triggered by slash commands.
    pub pending_model_refresh: Option<String>,
    /// Whether completing the pending model refresh should open `/model`.
    pub pending_open_model_picker: bool,
    /// Cached count of connected providers for frame rendering without disk polling.
    pub connected_provider_count: usize,
    /// Non-blocking background model-discovery banner shown even when idle.
    pub discovery_status: Option<String>,
    /// Monotonic generation ID for active model discovery batch; stale completions are ignored.
    pub discovery_batch_id: u64,
    /// Number of in-flight provider discovery tasks in the active batch.
    pub discovery_pending: usize,
    /// Whether the active discovery batch should open `/model` once all tasks complete.
    pub discovery_open_picker: bool,
    /// Abort handles for active model discovery tasks so cancelling/disconnecting aborts them.
    pub discovery_handles: Vec<tokio::task::AbortHandle>,
    /// Shared cancellation token for the active discovery batch (aborts spawned blocking work too).
    pub discovery_cancel: Option<CancellationToken>,
    /// Reusable hardened HTTP client for live model discovery.
    pub(super) discovery_client: Option<reqwest::Client>,
    pub(super) transcript_cache: Mutex<TranscriptCache>,
    pub(super) cached_ctx_tokens: Mutex<Option<u64>>,
    /// Original theme saved while `/theme` picker previews choices live.
    pub(crate) theme_preview_restore: Option<Theme>,
    /// Cached parsed themes during `/theme` live preview so arrow navigation avoids disk re-reads.
    pub(crate) theme_preview_cache: std::collections::HashMap<String, Theme>,
    /// Monotonic counter bumped whenever the active theme changes or reloads.
    pub(crate) theme_generation: u64,
    /// Timestamp when the active tool approval prompt was first painted on screen.
    pub(crate) approval_first_rendered_at: Option<std::time::Instant>,
}

impl InteractiveState {
    pub fn new(mcp_summaries: Vec<McpConnectionSummary>, settings: &Settings) -> Self {
        Self {
            items: Vec::new(),
            status: None,
            is_busy: false,
            loader: Loader::new("Thinking..."),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost: 0.0,
            selector: None,
            pending_approval: None,
            mcp_summaries,
            theme: Theme::load(settings.theme.as_deref()),
            keybindings: Keybindings::default_bindings(),
            scroll_offset: 0,
            last_max_scroll: AtomicUsize::new(0),
            prev_transcript_len: AtomicUsize::new(0),
            scroll_anchor: AtomicUsize::new(0),
            turn_started_at: Mutex::new(None),
            turn_completed_at: Mutex::new(None),
            thinking_expanded: false,
            thinking_elapsed: Mutex::new(std::collections::HashMap::new()),
            last_prompt_text: None,
            last_prompt_content: None,
            overflow_retried: false,
            turn_start_idx: None,
            last_turn_error: None,
            settings_file: None,
            auth_file: None,
            models_file: None,
            discovered_models_file: None,
            connect_input: None,
            connect_provider_target: None,
            click_regions: Mutex::new(Vec::new()),
            pending_model_refresh: None,
            pending_open_model_picker: false,
            connected_provider_count: 0,
            discovery_status: None,
            discovery_batch_id: 0,
            discovery_pending: 0,
            discovery_open_picker: false,
            discovery_handles: Vec::new(),
            discovery_cancel: None,
            discovery_client: None,
            transcript_cache: Mutex::new(Vec::new()),
            cached_ctx_tokens: Mutex::new(None),
            theme_preview_restore: None,
            theme_preview_cache: std::collections::HashMap::new(),
            theme_generation: 0,
            approval_first_rendered_at: None,
        }
    }

    pub fn has_tool_approval(&self) -> bool {
        self.pending_approval.is_some()
            || self
                .selector
                .as_ref()
                .is_some_and(|s| s.kind == SelectorKind::ToolApproval)
    }

    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.theme_generation = self.theme_generation.wrapping_add(1);
    }

    pub fn close_modal(&mut self) {
        self.selector = None;
        self.connect_input = None;
        self.connect_provider_target = None;
        self.approval_first_rendered_at = None;
        self.theme_preview_cache.clear();
        if let Some(orig) = self.theme_preview_restore.take() {
            self.set_theme(orig);
        }
    }

    /// Opens `selector` unless a mandatory tool-approval prompt is currently active.
    pub fn open_modal(&mut self, selector: Selector) -> bool {
        if self.has_tool_approval() {
            return false;
        }
        if selector.kind != SelectorKind::ConnectInput {
            self.connect_input = None;
        }
        if selector.kind != SelectorKind::ConnectMethod {
            self.connect_provider_target = None;
        }
        if selector.kind != SelectorKind::Theme {
            self.theme_preview_cache.clear();
            if let Some(orig) = self.theme_preview_restore.take() {
                self.set_theme(orig);
            }
        }
        self.selector = Some(selector);
        true
    }

    /// Resets transient transcript, cache, scroll, elapsed, and approval render metadata
    /// on session switch, new session, branch navigation, or transcript clear.
    pub(crate) fn reset_transcript_metadata(&mut self) {
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.total_cost = 0.0;
        self.scroll_offset = 0;
        self.last_max_scroll.store(0, Ordering::Relaxed);
        self.prev_transcript_len.store(0, Ordering::Relaxed);
        self.scroll_anchor.store(0, Ordering::Relaxed);
        *mehh_ai::lock_recover(&self.turn_started_at) = None;
        *mehh_ai::lock_recover(&self.turn_completed_at) = None;
        mehh_ai::lock_recover(&self.thinking_elapsed).clear();
        self.last_prompt_text = None;
        self.last_prompt_content = None;
        self.overflow_retried = false;
        self.turn_start_idx = None;
        self.last_turn_error = None;
        self.approval_first_rendered_at = None;
        mehh_ai::lock_recover(&self.transcript_cache).clear();
        self.invalidate_context_tokens();
    }

    /// Aborts any in-flight discovery batch and advances `discovery_batch_id` so late results are dropped.
    pub fn cancel_active_discovery(&mut self) {
        if let Some(tok) = self.discovery_cancel.take() {
            tok.cancel();
        }
        for handle in self.discovery_handles.drain(..) {
            handle.abort();
        }
        self.discovery_batch_id = self.discovery_batch_id.wrapping_add(1);
        self.discovery_pending = 0;
        self.discovery_open_picker = false;
        self.discovery_status = None;
    }

    pub(crate) fn invalidate_context_tokens(&self) {
        let mut guard = self
            .cached_ctx_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

/// Rebuilds transcript items and recomputes token/cost totals from the active session branch.
pub(super) fn sync_state_from_session(state: &mut InteractiveState, session: &AgentSession) {
    state.reset_transcript_metadata();
    state.items = rebuild_transcript_items(session);
    for entry in session.session().active_path_entries() {
        if let SessionEntryKind::Message {
            message: Message::Assistant(a),
        } = &entry.kind
        {
            state.total_input_tokens = state.total_input_tokens.saturating_add(a.usage.input);
            state.total_output_tokens = state.total_output_tokens.saturating_add(a.usage.output);
            state.total_cost += a.usage.cost.total;
        }
    }
}

impl Drop for InteractiveState {
    fn drop(&mut self) {
        if let Some(tok) = self.discovery_cancel.take() {
            tok.cancel();
        }
        for handle in self.discovery_handles.drain(..) {
            handle.abort();
        }
    }
}

/// Result of evaluating a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandOutcome {
    NotACommand,
    Handled,
    Compact(Option<String>),
    StartOAuthLogin(String),
    StartMcpAuth(String),
    Quit,
}

/// Routing of editor input submitted while an agent turn is already running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyInput {
    /// Text queued for steering into the running turn.
    Steer(String),
    /// Text queued to run once the current turn finishes.
    FollowUp(String),
}

pub(super) fn strip_follow_up_prefix(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    let prefix = trimmed.get(..10)?;
    if !prefix.eq_ignore_ascii_case("/follow-up") {
        return None;
    }
    let rest = &trimmed[10..];
    if rest.starts_with(char::is_whitespace) {
        let body = rest.trim();
        if !body.is_empty() {
            return Some(body);
        }
    }
    None
}

pub(super) fn peel_follow_up_prefixes(mut input: &str) -> &str {
    while let Some(next) = strip_follow_up_prefix(input) {
        input = next;
    }
    input.trim()
}

/// Routes editor input submitted while an agent turn is running.
///
/// `/follow-up <text>` becomes a follow-up message; anything else is queued as a
/// steering message injected into the running turn.
pub fn route_busy_input(input: &str) -> BusyInput {
    let trimmed = input.trim();
    if let Some(text) = strip_follow_up_prefix(trimmed) {
        return BusyInput::FollowUp(text.to_string());
    }
    BusyInput::Steer(trimmed.to_string())
}

