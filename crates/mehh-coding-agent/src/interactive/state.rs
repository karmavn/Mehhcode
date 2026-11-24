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

pub(super) type TranscriptCache = HashMap<
    (usize, usize, u8, usize, u64),
    (String, TranscriptItem, Arc<[String]>),
>;

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
            .send(AgentSignal::ToolApprovalRequest(pending))
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
    pub(super) transcript_cache: Mutex<TranscriptCache>,
    pub(super) cached_ctx_tokens: Mutex<Option<u64>>,
    /// Original theme saved while `/theme` picker previews choices live.
    pub(crate) theme_preview_restore: Option<Theme>,
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
            transcript_cache: Mutex::new(HashMap::new()),
            cached_ctx_tokens: Mutex::new(None),
            theme_preview_restore: None,
        }
    }

    pub(crate) fn invalidate_context_tokens(&self) {
        let mut guard = self
            .cached_ctx_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
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

/// Routes editor input submitted while an agent turn is running.
///
/// `/follow-up <text>` becomes a follow-up message; anything else is queued as a
/// steering message injected into the running turn.
pub fn route_busy_input(input: &str) -> BusyInput {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("/follow-up") {
        let text = rest.trim();
        // The command name must end at a boundary, so `/follow-upXYZ` is not a match.
        if !text.is_empty() && rest.starts_with(char::is_whitespace) {
            return BusyInput::FollowUp(text.to_string());
        }
    }
    BusyInput::Steer(trimmed.to_string())
}

