//! Interactive full-screen TUI mode for mehh coding agent.
//! Provides conversational transcript, live markdown streaming, slash commands,
//! editor with history and autocomplete, and async agent loop turns.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, MouseEventKind};
use futures::FutureExt;
use mehh_agent::{run_agent_loop, AgentEvent, AgentTool, QueueMode};
use mehh_ai::oauth::{self, OAuthProviderKind, StoredCredential};
use mehh_ai::{
    AssistantContentBlock, AssistantMessageEvent, Message, StopReason, ThinkingLevel, UserContent,
    UserContentBlock,
};
use mehh_tui::{
    normalize_crossterm_key, truncate_to_width, wrap_text_with_ansi, BorderStyle, BorderedBox,
    Component, CrosstermTerminal, Editor, EditorOutput, Keybindings, Loader, MarkdownView,
    SelectItem, SelectList, SettingItem, SettingsList, Terminal, Text, Theme, TuiAction,
    TuiRenderer, WheelScrollAccelerator,
};
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSession;
use crate::mcp_manager::{McpConnectionSummary, McpManager};
use crate::session::{SessionEntryKind, SessionManager, SessionSummary};
use crate::settings::{Settings, SettingsManager};

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
    Tree,
    Resume,
    Settings,
    Login,
    ToolApproval,
}

/// A picker overlay: the list of choices plus the context needed to apply one.
#[derive(Debug, Clone)]
pub struct Selector {
    pub kind: SelectorKind,
    pub list: SelectList,
    pub settings_list: Option<SettingsList>,
}

type TranscriptCache = HashMap<
    (usize, usize, u8, usize, u64),
    (String, TranscriptItem, Vec<String>),
>;

fn anchored_scroll_offset(anchor: usize, growth: usize, offset: usize) -> usize {
    if offset == 0 {
        0
    } else {
        anchor.saturating_add(growth)
    }
}

fn scrollbar_char(row: usize, visible: usize, total: usize, offset: usize) -> char {
    if visible == 0 || total <= visible {
        return '│';
    }
    let max_scroll = total - visible;
    let thumb_row = 1 + offset.min(max_scroll) * visible.saturating_sub(2) / max_scroll.max(1);
    if row == thumb_row {
        '┃'
    } else {
        '│'
    }
}

fn cap_preview_lines(preview: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = preview.lines().collect();
    if lines.len() <= max_lines {
        return preview.to_string();
    }
    let hidden = lines.len() - max_lines;
    let mut out = lines[..max_lines].join("\n");
    out.push_str(&format!("\n… (+{hidden} more lines)"));
    out
}

fn tool_duration_label(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) => format!(" {ms}ms"),
        None => String::new(),
    }
}

fn diff_visible_lines(diff: &str) -> (Vec<&str>, usize) {
    let all: Vec<&str> = diff.lines().collect();
    let hidden = all.len().saturating_sub(24);
    (all.into_iter().take(24).collect(), hidden)
}

fn thinking_preview(
    thinking: &str,
    expanded: bool,
    busy: bool,
    elapsed_ms: Option<u64>,
) -> String {
    let lines: Vec<&str> = thinking.lines().collect();
    if expanded {
        return thinking.to_string();
    }
    let count = lines.len();
    if busy {
        let tail = lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        format!("⠋ Thinking ({count} lines)...\n{tail}")
    } else {
        let secs = elapsed_ms.unwrap_or(0) as f64 / 1000.0;
        format!("▸ Thought for {secs:.1}s ({count} lines · Ctrl+T to expand)")
    }
}

fn compact_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    }
}

fn compact_path(path: &Path, max_len: usize) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.chars().count() <= max_len {
        return raw;
    }
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    let tail = parts
        .iter()
        .rev()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    let compact = format!("~/.../{tail}");
    if compact.chars().count() <= max_len {
        compact
    } else {
        let keep = max_len.saturating_sub(3);
        let tail: String = raw.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
        format!("...{tail}")
    }
}

fn left_right(left: String, right: String, width: usize) -> String {
    let right_w = mehh_tui::visible_width(&right);
    let left_w = width.saturating_sub(right_w);
    let left = truncate_to_width(&left, left_w, "...", false);
    let pad = width.saturating_sub(mehh_tui::visible_width(&left) + right_w);
    format!("{left}{}{right}", " ".repeat(pad))
}

fn ctx_bar(pct: u64, theme: &Theme) -> String {
    let filled = (pct * 8 / 100).min(8) as usize;
    let color = if pct >= 85 {
        &theme.error
    } else if pct >= 70 {
        &theme.warning
    } else {
        &theme.accent
    };
    let mut out = theme.fg(color, &"█".repeat(filled));
    out.push_str(&theme.fg(&theme.border_muted, &"░".repeat(8 - filled)));
    out
}

fn permission_badge(mode: Option<crate::tool_inspection::PermissionMode>, theme: &Theme) -> String {
    use crate::tool_inspection::PermissionMode;
    let (text, color) = match mode.unwrap_or_default() {
        PermissionMode::Auto => ("[auto]", &theme.warning),
        PermissionMode::SmartApprove => ("[smart]", &theme.accent),
        PermissionMode::Approve => ("[approve]", &theme.accent),
        PermissionMode::Chat => ("[chat]", &theme.dim),
    };
    theme.fg(color, text)
}

fn elapsed_label(started: Option<std::time::Instant>, now: std::time::Instant) -> String {
    match started {
        Some(s) => format!("{:.1}s", now.saturating_duration_since(s).as_secs_f64()),
        None => String::new(),
    }
}

fn render_user_turn(text: &str, width: usize, theme: &Theme) -> Vec<String> {
    let inner = width.saturating_sub(4);
    wrap_text_with_ansi(text, inner)
        .into_iter()
        .map(|line| {
            let body_w = mehh_tui::visible_width(&line);
            let padding = " ".repeat(width.saturating_sub(1 + body_w));
            format!(
                "{prefix}▌{reset}{bg}{color}{line}{padding}{reset}",
                prefix = theme.user_prefix,
                reset = "\x1b[0m",
                bg = theme.user_bg,
                color = theme.text,
            )
        })
        .collect()
}

fn status_icon(status: &str, is_error: bool, frame: usize, theme: &Theme) -> String {
    if is_error || status == "failed" {
        theme.fg(&theme.tool_error, "✗")
    } else if status == "completed" {
        theme.fg(&theme.tool_success, "✓")
    } else if status == "blocked" {
        theme.fg(&theme.tool_pending, "◌")
    } else {
        let frames = mehh_tui::BRAILLE_FRAMES;
        theme.fg(&theme.tool_pending, frames[frame % frames.len()])
    }
}

fn search_bar_text(filter: &str, shown: usize, total: usize, theme: &Theme) -> String {
    if filter.is_empty() {
        theme.fg(&theme.dim, &format!("  / Type to filter... ({shown}/{total})"))
    } else {
        theme.fg(&theme.accent, &format!("  Search: {filter} ({shown}/{total})"))
    }
}

fn scroll_pill_text(offset: usize, max_scroll: usize, new_lines: usize) -> String {
    let pct = if max_scroll == 0 {
        100
    } else {
        offset.min(max_scroll) * 100 / max_scroll
    };
    format!("↑ Paused at {pct}% · +{new_lines} new lines · End to jump")
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
    pub overflow_retried: bool,
    pub turn_start_idx: Option<usize>,
    pub last_turn_error: Option<String>,
    /// File that settings changes are written to. `None` uses the global settings file.
    pub settings_file: Option<PathBuf>,
    /// File that credentials are written to. `None` uses the default auth file.
    pub auth_file: Option<PathBuf>,
    transcript_cache: Mutex<TranscriptCache>,
    cached_ctx_tokens: Mutex<Option<u64>>,
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
            overflow_retried: false,
            turn_start_idx: None,
            last_turn_error: None,
            settings_file: None,
            auth_file: None,
            transcript_cache: Mutex::new(HashMap::new()),
            cached_ctx_tokens: Mutex::new(None),
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

/// Built-in slash commands offered for autocomplete and help.
pub fn builtin_slash_commands() -> Vec<SelectItem> {
    vec![
        SelectItem::new("/help", "/help").with_description("Show help and available commands"),
        SelectItem::new("/mode", "/mode")
            .with_description("Set permission mode (auto/smart_approve/approve/chat)"),
        SelectItem::new("/new", "/new").with_description("Start a fresh session"),
        SelectItem::new("/compact", "/compact").with_description("Compact session history"),
        SelectItem::new("/model", "/model").with_description("Switch active model"),
        SelectItem::new("/thinking", "/thinking")
            .with_description("Set thinking level (off/low/medium/high)"),
        SelectItem::new("/theme", "/theme")
            .with_description("Set color theme (dark/light or theme name)"),
        SelectItem::new("/settings", "/settings").with_description("Browse and toggle settings"),
        SelectItem::new("/tools", "/tools").with_description("List active tools"),
        SelectItem::new("/mcp", "/mcp")
            .with_description("MCP server status, reconnect, auth and resources"),
        SelectItem::new("/tree", "/tree").with_description("Browse and switch session branches"),
        SelectItem::new("/fork", "/fork").with_description("Fork the session at an entry"),
        SelectItem::new("/clone", "/clone").with_description("Clone the current session"),
        SelectItem::new("/resume", "/resume").with_description("Resume a saved session"),
        SelectItem::new("/login", "/login").with_description("Log in to a provider"),
        SelectItem::new("/logout", "/logout")
            .with_description("Remove a stored provider credential"),
        SelectItem::new("/follow-up", "/follow-up").with_description("Queue a follow-up message"),
        SelectItem::new("/copy", "/copy").with_description("Copy last agent message to clipboard"),
        SelectItem::new("/name", "/name").with_description("Set session display name"),
        SelectItem::new("/session", "/session").with_description("Show session info and stats"),
        SelectItem::new("/hotkeys", "/hotkeys").with_description("Show all keyboard shortcuts"),
        SelectItem::new("/reload", "/reload")
            .with_description("Reload keybindings, skills, prompts, themes, and context files"),
        SelectItem::new("/scoped-models", "/scoped-models")
            .with_description("Enable/disable models for cycling"),
        SelectItem::new("/export", "/export").with_description("Export session to a JSONL file"),
        SelectItem::new("/import", "/import").with_description("Import a JSONL session"),
        SelectItem::new("/clear", "/clear").with_description("Clear terminal transcript"),
        SelectItem::new("/exit", "/exit").with_description("Exit interactive mode"),
        SelectItem::new("/quit", "/quit").with_description("Quit interactive mode"),
    ]
}

/// Copies a single interactive toggle from one settings set into another.
///
/// The remaining keys (model, provider, limits) belong to the user's machine or
/// project and are never overwritten by an interactive toggle.
pub fn copy_toggle(target: &mut Settings, source: &Settings, key: &str) {
    match key {
        "theme" => target.theme = source.theme.clone(),
        "steering_mode" => target.steering_mode = source.steering_mode,
        "follow_up_mode" => target.follow_up_mode = source.follow_up_mode,
        "default_thinking_level" => target.default_thinking_level = source.default_thinking_level,
        "hide_thinking_block" => target.hide_thinking_block = source.hide_thinking_block,
        "auto_compact" => target.auto_compact = source.auto_compact,
        _ => {}
    }
}

/// Persists a single interactive setting.
///
/// The target file is loaded first, so a malformed file is reported instead of
/// overwritten, and only the changed key is written, so project-local values of
/// the other keys are not promoted into the global configuration.
pub fn persist_settings(settings: &Settings, key: &str, path: Option<&Path>) -> anyhow::Result<()> {
    let file_path = match path {
        Some(p) => p.to_path_buf(),
        None => SettingsManager::global_path()?,
    };
    let mut target = SettingsManager::load_file(&file_path)?;
    copy_toggle(&mut target, settings, key);
    SettingsManager::save_file(&file_path, &target)
}

/// Returns true when a theme name can be resolved to a theme.
///
/// Built-in names (`dark`, `light`) always resolve; other names resolve when they
/// point at a readable theme file.
pub fn theme_source_exists(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.eq_ignore_ascii_case("dark") || trimmed.eq_ignore_ascii_case("light") {
        return true;
    }
    if Path::new(trimmed).exists() {
        return true;
    }
    let home = match std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        Some(h) => PathBuf::from(h),
        None => return false,
    };
    let filename = if trimmed.ends_with(".json") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.json")
    };
    home.join(".mehh")
        .join("agent")
        .join("themes")
        .join(filename)
        .exists()
}

/// Logs into a provider and stores the resulting credential.
///
/// An explicit API key is stored verbatim; otherwise the provider's OAuth flow is
/// used (PKCE for Anthropic and ChatGPT, device code for GitHub Copilot).
pub async fn perform_login(
    provider: &str,
    api_key: Option<&str>,
    auth_file: Option<&Path>,
) -> anyhow::Result<String> {
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        let cred = StoredCredential::ApiKey {
            key: Some(key.to_string()),
            env: None,
        };
        crate::auth::save_credential(provider, &cred, auth_file)?;
        return Ok(format!("Saved API key for {provider}"));
    }

    let kind = crate::auth::oauth_kind_for_provider(provider).ok_or_else(|| {
        anyhow::anyhow!("Provider '{provider}' does not support OAuth. Provide an API key instead.")
    })?;

    let client = mehh_ai::default_http_client();
    let cred = match kind {
        OAuthProviderKind::Anthropic | OAuthProviderKind::OpenAiChatGpt => {
            login_via_pkce(&client, kind).await?
        }
        OAuthProviderKind::GitHubCopilot => login_via_device_flow(&client).await?,
    };

    crate::auth::save_credential(provider, &cred, auth_file)?;
    Ok(format!("Logged in to {provider}"))
}

/// Removes the stored credential for a provider. Returns true when one was removed.
pub fn perform_logout(provider: &str, auth_file: Option<&Path>) -> anyhow::Result<bool> {
    crate::auth::remove_credential(provider, auth_file)
}

/// Refreshes the session API key after a login so the running agent uses the new
/// credential without a restart.
///
/// Only the credential of the session's own provider can replace the session key:
/// a key stored for another provider would be sent to the wrong endpoint.
pub async fn refresh_session_api_key(
    session: &mut AgentSession,
    provider: &str,
    auth_file: Option<&Path>,
) -> anyhow::Result<()> {
    if !provider.eq_ignore_ascii_case(&session.model.provider) {
        return Ok(());
    }

    // The credential that was just stored wins over the environment variable.
    let entries = crate::auth::load_auth_entries(auth_file)?;
    let key = entries
        .get(provider)
        .or_else(|| {
            entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(provider))
                .map(|(_, v)| v)
        })
        .and_then(|cred| match cred {
            StoredCredential::ApiKey { key, .. } => key.clone(),
            StoredCredential::OAuth { access, .. } => Some(access.clone()),
        });

    if let Some(key) = key {
        session.set_api_key(Some(key));
    }
    Ok(())
}

/// Runs the OAuth PKCE flow against a loopback callback listener.
async fn login_via_pkce(
    client: &reqwest::Client,
    kind: OAuthProviderKind,
) -> anyhow::Result<StoredCredential> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("Failed to bind loopback listener for OAuth callback")?;
    let port = listener
        .local_addr()
        .context("Failed to read loopback listener address")?
        .port();
    drop(listener);

    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let pkce = oauth::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::build_authorize_url(kind, &pkce.challenge, &state, &redirect_uri)?;

    println!("Open the following URL to authenticate:\n{url}");

    let code = oauth::wait_for_loopback_callback(port, &state, Duration::from_secs(300)).await?;
    oauth::exchange_code_for_tokens(client, kind, &code, &pkce.verifier, &redirect_uri).await
}

/// Runs the GitHub Copilot device code flow until the user approves or the code expires.
async fn login_via_device_flow(client: &reqwest::Client) -> anyhow::Result<StoredCredential> {
    let challenge = oauth::start_copilot_device_flow(client).await?;
    println!(
        "Enter code {} at {}",
        challenge.user_code, challenge.verification_uri
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(challenge.expires_in_secs);
    let interval = Duration::from_secs(challenge.interval_secs.max(1));

    let mut poll_interval = interval;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        match oauth::poll_copilot_device_token(client, &challenge.device_code).await? {
            oauth::CopilotDevicePoll::Complete(cred) => return Ok(cred),
            oauth::CopilotDevicePoll::SlowDown => {
                poll_interval += Duration::from_secs(5);
            }
            oauth::CopilotDevicePoll::Pending => {}
        }
    }

    anyhow::bail!("Device code login timed out before it was approved")
}

/// Converts a persisted message into its transcript representation.
fn message_to_transcript_item(message: &Message) -> TranscriptItem {
    match message {
        Message::User(u) => TranscriptItem::User(u.content.text()),
        Message::Assistant(a) => TranscriptItem::Assistant {
            thinking: a
                .content
                .iter()
                .filter_map(|b| match b {
                    AssistantContentBlock::Thinking(t) => Some(t.thinking.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            text: a.text_content(),
        },
        Message::ToolResult(tr) => TranscriptItem::ToolCall {
            id: tr.tool_call_id.clone(),
            name: tr.tool_name.clone(),
            status: if tr.is_error {
                "failed".to_string()
            } else {
                "completed".to_string()
            },
            is_error: tr.is_error,
            args_summary: String::new(),
            output_preview: Vec::new(),
            diff: tr
                .details
                .as_ref()
                .and_then(|d| d.get("diff"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            duration_ms: tr.duration_ms,
        },
        Message::System(s) => TranscriptItem::System(s.content.clone()),
    }
}

/// Rebuilds the transcript entries from the session's active branch.
pub fn rebuild_transcript_items(session: &AgentSession) -> Vec<TranscriptItem> {
    session
        .session()
        .active_path_entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            SessionEntryKind::Message { message } => Some(message_to_transcript_item(message)),
            _ => None,
        })
        .collect()
}

/// Human readable label for a queue mode.
fn queue_mode_label(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::OneAtATime => "one-at-a-time",
        QueueMode::All => "all",
    }
}

/// Human readable label for a thinking level.
pub fn thinking_level_label(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// Returns the next thinking level in the cycle.
fn cycle_thinking_level(current: Option<ThinkingLevel>) -> ThinkingLevel {
    match current {
        None | Some(ThinkingLevel::Off) => ThinkingLevel::Low,
        Some(ThinkingLevel::Minimal) | Some(ThinkingLevel::Low) => ThinkingLevel::Medium,
        Some(ThinkingLevel::Medium) => ThinkingLevel::High,
        Some(ThinkingLevel::High) | Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => {
            ThinkingLevel::Off
        }
    }
}

/// Builds the picker entries for the settings screen.
fn settings_selector_items(settings: &Settings) -> Vec<SelectItem> {
    vec![
        SelectItem::new(
            "theme",
            format!(
                "theme: {}",
                settings.theme.clone().unwrap_or_else(|| "dark".to_string())
            ),
        ),
        SelectItem::new(
            "steering_mode",
            format!(
                "steering mode: {}",
                queue_mode_label(settings.steering_mode.unwrap_or_default())
            ),
        ),
        SelectItem::new(
            "follow_up_mode",
            format!(
                "follow-up mode: {}",
                queue_mode_label(settings.follow_up_mode.unwrap_or_default())
            ),
        ),
        SelectItem::new(
            "default_thinking_level",
            format!(
                "thinking level: {}",
                settings
                    .default_thinking_level
                    .map(thinking_level_label)
                    .unwrap_or("off")
            ),
        ),
        SelectItem::new(
            "hide_thinking_block",
            format!(
                "hide thinking block: {}",
                settings.hide_thinking_block.unwrap_or(false)
            ),
        ),
        SelectItem::new(
            "auto_compact",
            format!("auto compact: {}", settings.auto_compact.unwrap_or(true)),
        ),
    ]
}

/// Builds the two-column settings list component for the settings screen.
fn build_settings_list(settings: &Settings) -> SettingsList {
    let items = vec![
        SettingItem::new(
            "theme",
            "theme",
            settings.theme.clone().unwrap_or_else(|| "dark".to_string()),
        )
        .with_values(vec!["dark".to_string(), "light".to_string()])
        .with_description("Active color theme for the TUI"),
        SettingItem::new(
            "steering_mode",
            "steering mode",
            queue_mode_label(settings.steering_mode.unwrap_or_default()),
        )
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("Steering mode for tool execution and prompts"),
        SettingItem::new(
            "follow_up_mode",
            "follow-up mode",
            queue_mode_label(settings.follow_up_mode.unwrap_or_default()),
        )
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("Follow-up queue mode"),
        SettingItem::new(
            "default_thinking_level",
            "default thinking level",
            settings
                .default_thinking_level
                .map(|t| format!("{t:?}").to_lowercase())
                .unwrap_or_else(|| "off".to_string()),
        )
        .with_values(vec![
            "off".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ])
        .with_description("Default thinking budget level"),
        SettingItem::new(
            "hide_thinking_block",
            "hide thinking block",
            if settings.hide_thinking_block.unwrap_or(false) {
                "true"
            } else {
                "false"
            },
        )
        .with_values(vec!["false".to_string(), "true".to_string()])
        .with_description("Collapse or hide thinking blocks in output"),
        SettingItem::new(
            "auto_compact",
            "auto compact",
            if settings.auto_compact.unwrap_or(true) {
                "true"
            } else {
                "false"
            },
        )
        .with_values(vec!["true".to_string(), "false".to_string()])
        .with_description("Automatically compact context when nearing limit"),
    ];
    SettingsList::new(items, 8)
}

/// Rebuild the settings overlay after a toggle without moving the cursor.
/// `SelectList::set_items` resets selection to zero, so restore it explicitly.
fn refresh_settings_selector(state: &mut InteractiveState, settings: &Settings) {
    let prev = state
        .selector
        .as_ref()
        .map(|sel| sel.list.selected)
        .unwrap_or(0);
    if let Some(sel) = state.selector.as_mut() {
        sel.list.set_items(settings_selector_items(settings));
        sel.list.selected = prev.min(sel.list.items.len().saturating_sub(1));
        let filter = sel.list.filter.clone();
        let mut detail = build_settings_list(settings);
        detail.set_filter(&filter);
        detail.selected = sel.list.selected;
        sel.settings_list = Some(detail);
    }
}

/// Feed a typed character or Backspace into a picker's fuzzy filter.
/// Returns false for navigation/action keys so the caller keeps handling them.
fn update_selector_filter(selector: &mut Selector, key: &str) -> bool {
    let next = if key == "backspace" {
        let mut filter = selector.list.filter.clone();
        if filter.pop().is_none() {
            return true;
        }
        filter
    } else if key == " " && selector.kind != SelectorKind::Settings {
        format!("{}{}", selector.list.filter, " ")
    } else if key.chars().count() == 1
        && key.chars().all(|c| !c.is_control() && !c.is_whitespace())
    {
        format!("{}{}", selector.list.filter, key)
    } else {
        return false;
    };
    selector.list.set_filter(&next);
    if let Some(ref mut detail) = selector.settings_list {
        detail.set_filter(&next);
    }
    true
}

/// Cycles a single setting value and applies it to the running session.
fn apply_setting_cycle(
    key: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<String> {
    match key {
        "theme" => {
            let current = session
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "dark".to_string());
            let next = if current.eq_ignore_ascii_case("light") {
                "dark".to_string()
            } else {
                "light".to_string()
            };
            state.theme = Theme::load(Some(&next));
            session.settings.theme = Some(next.clone());
            Ok(format!("theme = {next}"))
        }
        "steering_mode" => {
            let current = session.settings.steering_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.steering_mode = Some(next);
            Ok(format!("steering_mode = {}", queue_mode_label(next)))
        }
        "follow_up_mode" => {
            let current = session.settings.follow_up_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.follow_up_mode = Some(next);
            Ok(format!("follow_up_mode = {}", queue_mode_label(next)))
        }
        "default_thinking_level" => {
            let next = cycle_thinking_level(session.settings.default_thinking_level);
            session.settings.default_thinking_level = Some(next);
            session.set_thinking_level(if next == ThinkingLevel::Off {
                None
            } else {
                Some(next)
            });
            Ok(format!(
                "default_thinking_level = {}",
                thinking_level_label(next)
            ))
        }
        "hide_thinking_block" => {
            let next = !session.settings.hide_thinking_block.unwrap_or(false);
            session.settings.hide_thinking_block = Some(next);
            Ok(format!("hide_thinking_block = {next}"))
        }
        "auto_compact" => {
            let next = !session.settings.auto_compact.unwrap_or(true);
            session.settings.auto_compact = Some(next);
            Ok(format!("auto_compact = {next}"))
        }
        other => anyhow::bail!("Unknown setting: '{other}'"),
    }
}

/// Splits a rendered system prompt into its preamble and its addendum.
fn split_system_prompt(prompt: &str) -> (Option<String>, Option<String>) {
    let mut sections = prompt.split("\n\n");
    let preamble = sections.next().unwrap_or("").trim().to_string();
    let custom = if preamble.is_empty() {
        None
    } else {
        Some(preamble)
    };

    let mut addendum = None;
    for section in sections {
        if let Some(inner) = section
            .strip_prefix("<addendum>\n")
            .and_then(|s| s.strip_suffix("\n</addendum>"))
        {
            addendum = Some(inner.to_string());
        }
    }

    (custom, addendum)
}

/// Replaces the MCP-derived tools on the session with a freshly synchronized set.
///
/// Preserves multi-paragraph custom/append system prompts when stored on `AgentSession`,
/// falling back to `split_system_prompt` only when no explicit overrides were recorded.
fn sync_mcp_tools(
    session: &mut AgentSession,
    mcp_tools: Vec<Arc<dyn AgentTool>>,
    codemode_enabled: bool,
) {
    // `AgentSession::sync_mcp_tools` preserves the CodemodeTool and deferred
    // catalog handles. Carry rendered prompt overrides into its explicit slots
    // so rebuilding the system prompt does not drop them.
    if session.custom_system_prompt.is_none() && session.append_system_prompt.is_none() {
        let (preamble, addendum) = split_system_prompt(&session.system_prompt);
        session.custom_system_prompt = preamble;
        session.append_system_prompt = addendum;
    }
    session.sync_mcp_tools(mcp_tools, codemode_enabled);
}

/// Switches the session's model via [`ModelRegistry`] and asynchronously refreshes credentials.
async fn apply_interactive_model_switch(
    target: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    let reg = match crate::models_config::ModelRegistry::load_default() {
        Ok(r) => r,
        Err(e) => {
            state.items.push(TranscriptItem::System(format!(
                "Failed to load models registry: {e}"
            )));
            return;
        }
    };
    let Some(m) = reg.resolve(target, None) else {
        state
            .items
            .push(TranscriptItem::System(format!("Unknown model: '{target}'")));
        return;
    };

    let resolved_key = if m.provider != "faux" && m.api != "faux" {
        match crate::auth::resolve_api_key_async(&m.provider, None, state.auth_file.as_deref())
            .await
        {
            Ok(key) => key.or_else(|| reg.provider_api_key(&m.provider).map(str::to_string)),
            Err(e) => {
                state.items.push(TranscriptItem::System(format!(
                    "Failed to resolve credentials for '{}': {e}",
                    m.provider
                )));
                return;
            }
        }
    } else {
        None
    };

    let provider_changed = !session.model.provider.eq_ignore_ascii_case(&m.provider);
    let prov = AgentSession::default_provider_for_model(&m);
    session.set_model(m.clone(), prov);
    if let Some(key) = resolved_key {
        if provider_changed || !session.has_explicit_api_key() {
            session.set_api_key(Some(key));
        }
    }
    state.items.push(TranscriptItem::System(format!(
        "Switched model to {} ({})",
        m.name, m.provider
    )));
}

/// Formats active keyboard shortcuts from Keybindings for display.
pub fn format_hotkeys(keybindings: &Keybindings) -> String {
    let mut out = String::from("Keyboard Shortcuts:\n");
    let actions = [
        (TuiAction::Submit, "Submit prompt"),
        (TuiAction::Newline, "Insert newline"),
        (TuiAction::Interrupt, "Cancel turn / Interrupt"),
        (TuiAction::ClearOrExit, "Exit interactive session"),
        (
            TuiAction::TabComplete,
            "Autocomplete slash commands / files",
        ),
        (TuiAction::ScrollUp, "Scroll up"),
        (TuiAction::ScrollDown, "Scroll down"),
        (TuiAction::HistoryPrev, "Previous history item"),
        (TuiAction::HistoryNext, "Next history item"),
        (TuiAction::Undo, "Undo"),
    ];

    for (action, desc) in actions {
        if let Some(keys) = keybindings.get_bindings(action) {
            if !keys.is_empty() {
                let keys_str = keys.join(", ");
                out.push_str(&format!("  {:<24} {}\n", keys_str, desc));
            }
        }
    }
    out.trim_end().to_string()
}

/// Dispatches interactive slash commands.
pub async fn handle_slash_command(
    input: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> SlashCommandOutcome {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return SlashCommandOutcome::NotACommand;
    }

    let mut parts = trimmed.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next();
    let rest = parts.collect::<Vec<_>>().join(" ");
    let full_arg = trimmed
        .strip_prefix(cmd)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match cmd {
        "/exit" | "/quit" => SlashCommandOutcome::Quit,
        "/clear" => {
            state.items.clear();
            state.scroll_offset = 0;
            state.status = None;
            SlashCommandOutcome::Handled
        }
        "/help" => {
            let help_text = "\
Commands:
  /help                    Show this help message
  /new                     Start a fresh session
  /compact [instructions]  Compact session history
  /model [model_id]        Switch active model (or open picker)
  /thinking [level]        Set thinking level (off, low, medium, high)
  /theme [name]            Set color theme (dark, light, or theme file name)
  /settings                Browse settings and cycle their values
  /tools                   List all active tools
  /mcp [status|reconnect [name]|auth <name>|resources]
                           Inspect and manage MCP servers
  /tree [entry_id]         Browse session branches and switch between them
  /fork [entry_id]         Fork the session into a new one
  /clone                   Clone the current session
  /resume                  Resume a saved session
  /login [provider] [key]  Log in to a provider (OAuth or API key)
  /logout [provider]       Remove a stored provider credential
  /follow-up <text>        Queue a message for after the current turn
  /clear                   Clear transcript
  /exit or /quit           Exit interactive mode

Keybindings:
  Enter        Submit prompt (while busy: queue steering message)
  Shift+Enter  Insert newline
  Alt+Enter    Queue follow-up message while busy
  Ctrl+C       Interrupt the turn, clear the input, or exit on an empty prompt
  Esc          Interrupt the turn, clear the input, or exit on an empty prompt
  Up / Down    Navigate history or autocomplete";
            state
                .items
                .push(TranscriptItem::System(help_text.to_string()));
            SlashCommandOutcome::Handled
        }
        "/mode" => {
            if let Some(mode_str) = full_arg {
                match mode_str.parse::<crate::tool_inspection::PermissionMode>() {
                    Ok(new_mode) => {
                        session.inspection_manager.set_mode(new_mode).await;
                        session.settings.permission_mode = Some(new_mode);
                        state.items.push(TranscriptItem::System(format!(
                            "Permission mode set to: {:?}",
                            new_mode
                        )));
                    }
                    Err(e) => {
                        state.items.push(TranscriptItem::System(e));
                    }
                }
            } else {
                let current = session.inspection_manager.mode().await;
                state.items.push(TranscriptItem::System(format!(
                    "Current permission mode: {:?} (options: auto, smart_approve, approve, chat)",
                    current
                )));
            }
            SlashCommandOutcome::Handled
        }
        "/new" => {
            state.invalidate_context_tokens();
            session.clear_queues();
            match session.session_mut().new_session() {
                Ok(()) => {
                    state.items.clear();
                    state.scroll_offset = 0;
                    state.items.push(TranscriptItem::System(format!(
                        "Started new session: {}",
                        session.session().session_id()
                    )));
                }
                Err(e) => state.items.push(TranscriptItem::System(format!(
                    "Failed to start a new session: {e}"
                ))),
            }
            SlashCommandOutcome::Handled
        }
        "/compact" => {
            state.invalidate_context_tokens();
            let custom_instructions = if let Some(first_arg) = arg {
                let rest_idx = trimmed.find(first_arg).unwrap_or(trimmed.len());
                let rest = trimmed[rest_idx..].trim();
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                }
            } else {
                None
            };
            SlashCommandOutcome::Compact(custom_instructions)
        }
        "/model" => {
            if let Some(target) = arg {
                apply_interactive_model_switch(target, session, state).await;
            } else {
                match crate::models_config::ModelRegistry::load_default() {
                    Ok(reg) => {
                        let items: Vec<SelectItem> = reg
                            .models()
                            .iter()
                            .map(|m| {
                                SelectItem::new(
                                    format!("{}/{}", m.provider, m.id),
                                    format!("{} ({})", m.name, m.provider),
                                )
                                .with_description(m.id.clone())
                            })
                            .collect();
                        state.selector = Some(Selector {
                            kind: SelectorKind::Model,
                            list: SelectList::new(items, 8),
                            settings_list: None,
                        });
                    }
                    Err(e) => {
                        state.items.push(TranscriptItem::System(format!(
                            "Failed to load models registry: {e}"
                        )));
                    }
                }
            }
            SlashCommandOutcome::Handled
        }
        "/thinking" => {
            if let Some(level_str) = arg {
                match level_str.to_lowercase().as_str() {
                    "off" | "none" => {
                        session.set_thinking_level(None);
                        state
                            .items
                            .push(TranscriptItem::System("Thinking disabled".to_string()));
                    }
                    "low" => {
                        session.set_thinking_level(Some(ThinkingLevel::Low));
                        state
                            .items
                            .push(TranscriptItem::System("Thinking set to low".to_string()));
                    }
                    "medium" | "med" => {
                        session.set_thinking_level(Some(ThinkingLevel::Medium));
                        state
                            .items
                            .push(TranscriptItem::System("Thinking set to medium".to_string()));
                    }
                    "high" => {
                        session.set_thinking_level(Some(ThinkingLevel::High));
                        state
                            .items
                            .push(TranscriptItem::System("Thinking set to high".to_string()));
                    }
                    other => {
                        state.items.push(TranscriptItem::System(format!(
                            "Unknown thinking level: '{other}'. Choose from: off, low, medium, high"
                        )));
                    }
                }
            } else {
                let current = session
                    .thinking_level()
                    .map(thinking_level_label)
                    .unwrap_or("off");
                state.items.push(TranscriptItem::System(format!(
                    "Current thinking level: {current}"
                )));
            }
            SlashCommandOutcome::Handled
        }
        "/theme" => {
            match arg {
                Some(name) => {
                    if !theme_source_exists(name) {
                        state.items.push(TranscriptItem::System(format!(
                            "Unknown theme: '{name}'. Use dark, light, or a theme file name."
                        )));
                    } else {
                        let theme = Theme::load(Some(name));
                        state.invalidate_context_tokens();
                        state.theme = theme.clone();
                        session.settings.theme = Some(name.to_string());
                        match persist_settings(
                            &session.settings,
                            "theme",
                            state.settings_file.as_deref(),
                        ) {
                            Ok(()) => state.items.push(TranscriptItem::System(format!(
                                "Theme set to '{name}' (active: {})",
                                theme.name
                            ))),
                            Err(e) => state.items.push(TranscriptItem::System(format!(
                                "Theme set to '{name}' but saving settings failed: {e}"
                            ))),
                        }
                    }
                }
                None => {
                    state.items.push(TranscriptItem::System(format!(
                        "Current theme: {}",
                        state.theme.name
                    )));
                }
            }
            SlashCommandOutcome::Handled
        }
        "/settings" => {
            let items = settings_selector_items(&session.settings);
            let sl = build_settings_list(&session.settings);
            state.selector = Some(Selector {
                kind: SelectorKind::Settings,
                list: SelectList::new(items, 8),
                settings_list: Some(sl),
            });
            SlashCommandOutcome::Handled
        }
        "/tools" => {
            let tool_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
            let summary = format!(
                "Active tools ({}): {}",
                tool_names.len(),
                tool_names.join(", ")
            );
            state.items.push(TranscriptItem::System(summary));
            SlashCommandOutcome::Handled
        }
        "/mcp" => {
            let sub = arg.map(|s| s.to_ascii_lowercase());
            match sub.as_deref() {
                None | Some("status") => {
                    if state.mcp_summaries.is_empty() {
                        state.items.push(TranscriptItem::System(
                            "No MCP servers configured or connected.".to_string(),
                        ));
                    } else {
                        let mut out = String::from("MCP Servers:\n");
                        for s in &state.mcp_summaries {
                            if let Some(ref err) = s.error {
                                out.push_str(&format!("  - {}: error: {}\n", s.server, err));
                            } else {
                                out.push_str(&format!(
                                    "  - {}: {} tools connected\n",
                                    s.server, s.tool_count
                                ));
                            }
                        }
                        state
                            .items
                            .push(TranscriptItem::System(out.trim_end().to_string()));
                    }
                }
                Some("reconnect") => {
                    let name = rest.trim().to_string();
                    match mcp_manager {
                        None => state.items.push(TranscriptItem::System(
                            "MCP is not enabled for this session.".to_string(),
                        )),
                        Some(mgr) => {
                            let mut count = 0usize;
                            let mut errors: Vec<String> = Vec::new();
                            if name.is_empty() {
                                for summary in mgr.reconnect_all().await {
                                    count += 1;
                                    if let Some(ref err) = summary.error {
                                        errors.push(format!("{}: {err}", summary.server));
                                    }
                                }
                            } else {
                                match mgr.reconnect_server(&name).await {
                                    Ok(summary) => {
                                        count = 1;
                                        if let Some(ref err) = summary.error {
                                            errors.push(format!("{name}: {err}"));
                                        }
                                    }
                                    Err(e) => errors.push(format!("{name}: {e}")),
                                }
                            }
                            state.mcp_summaries = mgr.summaries().to_vec();
                            let tools = mgr.tools().to_vec();
                            sync_mcp_tools(session, tools, codemode_enabled);
                            let mut msg = format!("Reconnected {count} MCP server(s)");
                            if !errors.is_empty() {
                                msg.push_str(&format!(" — errors: {}", errors.join(", ")));
                            }
                            state.items.push(TranscriptItem::System(msg));
                        }
                    }
                }
                Some("auth") => {
                    let name = rest.trim().to_string();
                    if name.is_empty() {
                        state.items.push(TranscriptItem::System(
                            "Usage: /mcp auth <server>".to_string(),
                        ));
                    } else {
                        match mcp_manager {
                            None => state.items.push(TranscriptItem::System(
                                "MCP is not enabled for this session.".to_string(),
                            )),
                            Some(mgr) => {
                                let res = mgr
                                    .authenticate_server_with_prompt(&name, |url| {
                                        println!("Open the following URL to authenticate:\n{url}");
                                    })
                                    .await;
                                match res {
                                    Ok(()) => state.items.push(TranscriptItem::System(format!(
                                        "Authenticated MCP server '{name}'"
                                    ))),
                                    Err(e) => state.items.push(TranscriptItem::System(format!(
                                        "MCP authentication failed for '{name}': {e}"
                                    ))),
                                }
                            }
                        }
                    }
                }
                Some("resources") => match mcp_manager {
                    None => state.items.push(TranscriptItem::System(
                        "MCP is not enabled for this session.".to_string(),
                    )),
                    Some(mgr) => {
                        let registry = mgr.registry();
                        let entries = registry.read().await.clone();
                        if entries.is_empty() {
                            state.items.push(TranscriptItem::System(
                                "No MCP servers are connected.".to_string(),
                            ));
                        } else {
                            let mut out = String::from("MCP Resources:\n");
                            for (server, client) in entries {
                                match client.list_resources().await {
                                    Ok(resources) if resources.is_empty() => {
                                        out.push_str(&format!("  - {server}: no resources\n"));
                                    }
                                    Ok(resources) => {
                                        for r in resources {
                                            out.push_str(&format!(
                                                "  - {server}: {} ({})\n",
                                                r.name, r.uri
                                            ));
                                        }
                                    }
                                    Err(e) => {
                                        out.push_str(&format!("  - {server}: error: {e}\n"));
                                    }
                                }
                            }
                            state
                                .items
                                .push(TranscriptItem::System(out.trim_end().to_string()));
                        }
                    }
                },
                Some(other) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Unknown /mcp subcommand: '{other}'. Use status, reconnect, auth or resources."
                    )));
                }
            }
            SlashCommandOutcome::Handled
        }
        "/tree" => {
            state.invalidate_context_tokens();
            match arg {
                Some(arg_str) => {
                    let parts = full_arg.unwrap_or(arg_str).split_whitespace();
                    let mut entry_id = None;
                    let mut summarize = false;
                    for p in parts {
                        if p == "--summarize" || p == "-s" {
                            summarize = true;
                        } else if entry_id.is_none() {
                            entry_id = Some(p);
                        }
                    }
                    if let Some(id) = entry_id {
                        match session.navigate_tree(id, summarize, None).await {
                            Ok(summary_opt) => {
                                state.items = rebuild_transcript_items(session);
                                state.scroll_offset = 0;
                                let msg = if let Some(s) = summary_opt {
                                    format!(
                                        "Switched to branch at {id} with branch summary ({})",
                                        s.len()
                                    )
                                } else {
                                    format!("Switched to branch at {id}")
                                };
                                state.items.push(TranscriptItem::System(msg));
                            }
                            Err(e) => state.items.push(TranscriptItem::System(format!(
                                "Failed to switch branch: {e}"
                            ))),
                        }
                    }
                }
                None => {
                    let tree = session.session().build_tree();
                    if tree.is_empty() {
                        state
                            .items
                            .push(TranscriptItem::System("Session tree is empty.".to_string()));
                    } else {
                        let items: Vec<SelectItem> = tree
                            .iter()
                            .map(|node| {
                                let indent = "  ".repeat(node.depth);
                                let marker = if node.is_active { "*" } else { " " };
                                SelectItem::new(
                                    node.entry_id.clone(),
                                    format!("{indent}{marker}{}", node.label),
                                )
                                .with_description(node.timestamp.clone())
                            })
                            .collect();
                        state.selector = Some(Selector {
                            kind: SelectorKind::Tree,
                            list: SelectList::new(items, 10),
                            settings_list: None,
                        });
                    }
                }
            }
            SlashCommandOutcome::Handled
        }
        "/fork" => {
            let target = arg.filter(|id| !id.trim().is_empty());
            // An unknown entry id would fork into a session with a dangling leaf,
            // so it is rejected before the active session is replaced.
            let unknown_target = target.filter(|id| {
                !session
                    .session()
                    .build_tree()
                    .iter()
                    .any(|node| node.entry_id == *id)
            });

            match (target, unknown_target) {
                (_, Some(id)) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Unknown entry id: '{id}'. Use /tree to pick a branch."
                    )));
                }
                (target, None) => match session.session().fork_at(target) {
                    Ok(new_session) => {
                        let new_id = new_session.session_id().to_string();
                        session.session = new_session;
                        state.items = rebuild_transcript_items(session);
                        state.scroll_offset = 0;
                        state.items.push(TranscriptItem::System(format!(
                            "Forked session into {new_id}"
                        )));
                    }
                    Err(e) => state.items.push(TranscriptItem::System(format!(
                        "Failed to fork session: {e}"
                    ))),
                },
            }
            SlashCommandOutcome::Handled
        }
        "/clone" => {
            match session.session().clone_at_leaf() {
                Ok(new_session) => {
                    let new_id = new_session.session_id().to_string();
                    session.session = new_session;
                    state.items = rebuild_transcript_items(session);
                    state.scroll_offset = 0;
                    state.items.push(TranscriptItem::System(format!(
                        "Cloned session into {new_id}"
                    )));
                }
                Err(e) => state.items.push(TranscriptItem::System(format!(
                    "Failed to clone session: {e}"
                ))),
            }
            SlashCommandOutcome::Handled
        }
        "/resume" => {
            state.invalidate_context_tokens();
            match crate::session::list_sessions(&session.cwd) {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        state.items.push(TranscriptItem::System(
                            "No saved sessions found for this directory.".to_string(),
                        ));
                    } else {
                        let items = resume_selector_items(&sessions);
                        state.selector = Some(Selector {
                            kind: SelectorKind::Resume,
                            list: SelectList::new(items, 10),
                            settings_list: None,
                        });
                    }
                }
                Err(e) => state.items.push(TranscriptItem::System(format!(
                    "Failed to list sessions: {e}"
                ))),
            }
            SlashCommandOutcome::Handled
        }
        "/login" => {
            match arg {
                Some(provider) => {
                    let api_key = if rest.trim().is_empty() {
                        None
                    } else {
                        Some(rest.trim())
                    };
                    match perform_login(provider, api_key, state.auth_file.as_deref()).await {
                        Ok(msg) => {
                            match refresh_session_api_key(
                                session,
                                provider,
                                state.auth_file.as_deref(),
                            )
                            .await
                            {
                                Ok(()) => state.items.push(TranscriptItem::System(msg)),
                                Err(e) => state.items.push(TranscriptItem::System(format!(
                                    "{msg} (could not refresh the session key: {e})"
                                ))),
                            }
                        }
                        Err(e) => state
                            .items
                            .push(TranscriptItem::System(format!("Login failed: {e}"))),
                    }
                }
                None => {
                    let items = vec![
                        SelectItem::new("anthropic", "anthropic")
                            .with_description("Claude (OAuth)"),
                        SelectItem::new("openai", "openai").with_description("ChatGPT (OAuth)"),
                        SelectItem::new("github-copilot", "github-copilot")
                            .with_description("GitHub Copilot (device code)"),
                    ];
                    state.selector = Some(Selector {
                        kind: SelectorKind::Login,
                        list: SelectList::new(items, 8),
                        settings_list: None,
                    });
                }
            }
            SlashCommandOutcome::Handled
        }
        "/logout" => {
            match arg {
                Some(provider) => match perform_logout(provider, state.auth_file.as_deref()) {
                    Ok(true) => state.items.push(TranscriptItem::System(format!(
                        "Removed stored credential for {provider}"
                    ))),
                    Ok(false) => state.items.push(TranscriptItem::System(format!(
                        "No stored credential for {provider}"
                    ))),
                    Err(e) => state
                        .items
                        .push(TranscriptItem::System(format!("Logout failed: {e}"))),
                },
                None => state.items.push(TranscriptItem::System(
                    "Usage: /logout <provider>".to_string(),
                )),
            }
            SlashCommandOutcome::Handled
        }
        "/follow-up" => {
            let text = full_arg.unwrap_or_default().trim();
            if text.is_empty() {
                state.items.push(TranscriptItem::System(
                    "Usage: /follow-up <text>".to_string(),
                ));
            } else {
                session.follow_up(text.to_string());
                state
                    .items
                    .push(TranscriptItem::System(format!("Queued follow-up: {text}")));
            }
            SlashCommandOutcome::Handled
        }
        "/copy" => {
            let messages = session.session().build_context_messages();
            let last_assistant = messages.iter().rev().find_map(|m| match m {
                Message::Assistant(a) => Some(a.text_content()),
                _ => None,
            });
            match last_assistant {
                Some(text) if !text.trim().is_empty() => {
                    match crate::clipboard::copy_to_clipboard(&text) {
                        Ok(()) => {
                            state.items.push(TranscriptItem::System(
                                "Copied message to clipboard.".to_string(),
                            ));
                        }
                        Err(err) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Failed to copy to clipboard: {err}"
                            )));
                        }
                    }
                }
                _ => {
                    state.items.push(TranscriptItem::System(
                        "No assistant message to copy.".to_string(),
                    ));
                }
            }
            SlashCommandOutcome::Handled
        }
        "/name" => {
            match full_arg {
                Some(name) => {
                    let trimmed = name.trim().to_string();
                    match session
                        .session_mut()
                        .set_session_name(Some(trimmed.clone()))
                    {
                        Ok(()) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Set session name to '{trimmed}'"
                            )));
                        }
                        Err(e) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Failed to set session name: {e}"
                            )));
                        }
                    }
                }
                None => {
                    let current_name = session.session().entries().iter().rev().find_map(|e| {
                        if let SessionEntryKind::SessionInfo { name: Some(n) } = &e.kind {
                            Some(n.as_str())
                        } else {
                            None
                        }
                    });
                    if let Some(n) = current_name {
                        state
                            .items
                            .push(TranscriptItem::System(format!("Session name: '{n}'")));
                    } else {
                        state.items.push(TranscriptItem::System(
                            "Session has no name set.".to_string(),
                        ));
                    }
                }
            }
            SlashCommandOutcome::Handled
        }
        "/session" => {
            let s = session.session();
            let entries_count = s.entries().len();
            let messages = s.build_context_messages();
            let msg_count = messages.len();
            let tokens = crate::compaction::estimate_context_tokens(&messages);
            let s_id = s.session_id();
            let s_file = s
                .file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "in-memory".to_string());
            let m_id = &session.model().id;
            let m_prov = &session.model().provider;
            let thinking = session.thinking_level();
            state.items.push(TranscriptItem::System(format!(
                "Session: {s_id}\nPath: {s_file}\nEntries: {entries_count} ({msg_count} active messages)\nEstimated Context Tokens: {tokens}\nModel: {m_prov}/{m_id}\nThinking Level: {thinking:?}"
            )));
            SlashCommandOutcome::Handled
        }
        "/hotkeys" => {
            state
                .items
                .push(TranscriptItem::System(format_hotkeys(&state.keybindings)));
            SlashCommandOutcome::Handled
        }
        "/reload" => {
            session.reload();
            state.theme = Theme::load(session.settings.theme.as_deref());
            state.keybindings = Keybindings::default_bindings();
            state.items.push(TranscriptItem::System(
                "Reloaded settings, skills, and prompt templates.".to_string(),
            ));
            SlashCommandOutcome::Handled
        }
        "/scoped-models" => {
            match full_arg {
                Some(arg_str) => {
                    if let Ok(reg) = crate::models_config::ModelRegistry::load_default() {
                        let mut resolved = Vec::new();
                        for token in arg_str
                            .split(|c: char| c == ',' || c.is_whitespace())
                            .filter(|t| !t.is_empty())
                        {
                            let parts: Vec<&str> = token.split('/').collect();
                            let model_opt = if parts.len() == 2 {
                                reg.resolve_exact(parts[0], parts[1])
                            } else {
                                reg.resolve(token, None)
                            };
                            if let Some(m) = model_opt {
                                resolved.push(m);
                            }
                        }
                        let count = resolved.len();
                        session.set_scoped_models(resolved);
                        state.items.push(TranscriptItem::System(format!(
                            "Configured {count} scoped models for cycling."
                        )));
                    } else {
                        state.items.push(TranscriptItem::System(
                            "Failed to load models registry.".to_string(),
                        ));
                    }
                }
                None => {
                    let current = session.scoped_models();
                    if current.is_empty() {
                        state.items.push(TranscriptItem::System(
                            "No scoped models configured (cycles through all models).".to_string(),
                        ));
                    } else {
                        let names: Vec<String> = current
                            .iter()
                            .map(|m| format!("{}/{}", m.provider, m.id))
                            .collect();
                        state.items.push(TranscriptItem::System(format!(
                            "Scoped models:\n  {}",
                            names.join("\n  ")
                        )));
                    }
                }
            }
            SlashCommandOutcome::Handled
        }
        "/export" => {
            let default_path =
                PathBuf::from(format!("session_{}.jsonl", session.session().session_id()));
            let out_path = full_arg.map(PathBuf::from).unwrap_or(default_path);
            match session.session().export_jsonl(&out_path) {
                Ok(()) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Exported session to {}",
                        out_path.display()
                    )));
                }
                Err(e) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Failed to export session: {e}"
                    )));
                }
            }
            SlashCommandOutcome::Handled
        }
        "/import" => {
            state.invalidate_context_tokens();
            match full_arg {
                Some(file_path_str) => {
                    let path = PathBuf::from(file_path_str.trim());
                    match SessionManager::open_file(&path, session.cwd.clone()) {
                        Ok(new_session) => {
                            let id = new_session.session_id().to_string();
                            session.session = new_session;
                            state.items = rebuild_transcript_items(session);
                            state.scroll_offset = 0;
                            state.items.push(TranscriptItem::System(format!(
                                "Imported session {id} from {}",
                                path.display()
                            )));
                        }
                        Err(e) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Failed to import session: {e}"
                            )));
                        }
                    }
                }
                None => {
                    state.items.push(TranscriptItem::System(
                        "Usage: /import <session_file.jsonl>".to_string(),
                    ));
                }
            }
            SlashCommandOutcome::Handled
        }
        unknown => {
            state.items.push(TranscriptItem::System(format!(
                "Unknown command: '{unknown}'. Type /help for available commands."
            )));
            SlashCommandOutcome::Handled
        }
    }
}

/// Builds the picker entries for the session resume screen.
fn resume_selector_items(sessions: &[SessionSummary]) -> Vec<SelectItem> {
    sessions
        .iter()
        .map(|s| {
            let label = s
                .name
                .clone()
                .or_else(|| s.first_prompt.clone())
                .unwrap_or_else(|| s.id.clone());
            SelectItem::new(s.path.display().to_string(), label)
                .with_description(format!("{} • {} entries", s.timestamp, s.entry_count))
        })
        .collect()
}

fn format_assistant_stop_notice(msg: &mehh_ai::AssistantMessage) -> String {
    let err_text = msg
        .error_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if msg.stop_reason == StopReason::Aborted {
                "Aborted"
            } else {
                "Provider stream error"
            }
        });
    if msg.stop_reason == StopReason::Aborted {
        format!("Turn aborted: {err_text}")
    } else {
        format!("Provider error: {err_text}")
    }
}

fn push_turn_error_once(state: &mut InteractiveState, notice: String) {
    if state.last_turn_error.as_deref() == Some(notice.as_str()) {
        return;
    }
    state.last_turn_error = Some(notice.clone());
    state.items.push(TranscriptItem::System(notice));
}

/// Applies agent lifecycle and stream events to update interactive state and transcript.
pub fn apply_agent_event(state: &mut InteractiveState, event: &AgentEvent) {
    match event {
        AgentEvent::AgentStart => {
            state.is_busy = true;
        }
        AgentEvent::AgentEnd { .. } => {
            state.is_busy = false;
            state.status = None;
        }
        AgentEvent::TurnStart => {
            state.is_busy = true;
        }
        AgentEvent::TurnEnd { .. } => {}
        AgentEvent::MessageStart { message } => {
            state.is_busy = true;
            if let Message::Assistant(_) = message {
                match state.items.last() {
                    Some(TranscriptItem::Assistant { .. }) => {}
                    _ => {
                        state.items.push(TranscriptItem::Assistant {
                            thinking: String::new(),
                            text: String::new(),
                        });
                    }
                }
            }
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                if let Some(TranscriptItem::Assistant { text, .. }) = state.items.last_mut() {
                    text.push_str(delta);
                } else {
                    state.items.push(TranscriptItem::Assistant {
                        thinking: String::new(),
                        text: delta.clone(),
                    });
                }
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                if let Some(TranscriptItem::Assistant { thinking, .. }) = state.items.last_mut() {
                    thinking.push_str(delta);
                } else {
                    state.items.push(TranscriptItem::Assistant {
                        thinking: delta.clone(),
                        text: String::new(),
                    });
                }
            }
            AssistantMessageEvent::Error { error, .. } => {
                let notice = format_assistant_stop_notice(error);
                push_turn_error_once(state, notice);
            }
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            state.invalidate_context_tokens();
            if let Message::Assistant(a) = message {
                state.total_input_tokens += a.usage.input;
                state.total_output_tokens += a.usage.output;
                state.total_cost += a.usage.cost.total;
                if let Some(TranscriptItem::Assistant { text, thinking }) = state.items.last_mut() {
                    if text.is_empty() && thinking.is_empty() {
                        *text = a.text_content();
                    }
                }
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    let notice = format_assistant_stop_notice(a);
                    push_turn_error_once(state, notice);
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            state.status = Some(format!("Executing {tool_name}..."));
            if tool_name == "subagent" {
                let mode = args
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("research")
                    .to_string();
                state.items.push(TranscriptItem::Subagent {
                    id: tool_call_id.clone(),
                    mode,
                    status: "running".to_string(),
                    is_error: false,
                    children: Vec::new(),
                });
                return;
            }
            let args_summary = if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                cmd.chars().take(40).collect::<String>()
            } else if let Some(q) = args
                .get("query")
                .or_else(|| args.get("pattern"))
                .and_then(|v| v.as_str())
            {
                q.to_string()
            } else {
                String::new()
            };
            state.items.push(TranscriptItem::ToolCall {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                status: "running".to_string(),
                is_error: false,
                args_summary,
                output_preview: Vec::new(),
                diff: None,
                duration_ms: None,
            });
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            // Structured subagent updates: upsert a child row keyed by child_id,
            // or move the group's status on terminal events.
            if let Some(sub) = partial_result
                .details
                .as_ref()
                .and_then(|d| d.get("subagent"))
            {
                if let (Some(child_id), Some(event_kind)) = (
                    sub.get("child_id").and_then(|v| v.as_str()),
                    sub.get("event_kind").and_then(|v| v.as_str()),
                ) {
                    let detail = sub
                        .get("detail")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    for item in state.items.iter_mut().rev() {
                        if let TranscriptItem::Subagent { id, children, .. } = item {
                            if id != tool_call_id {
                                continue;
                            }
                            match event_kind {
                                "started" => {
                                    if !children.iter().any(|r| r.child_id == child_id) {
                                        children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "started".to_string(),
                                            status: "running".to_string(),
                                            detail: detail.clone(),
                                        });
                                    }
                                }
                                "blocked" => {
                                    let blocker = detail.clone();
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            row.label = "blocked".to_string();
                                            row.status = "blocked".to_string();
                                            row.detail = blocker;
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "blocked".to_string(),
                                            status: "blocked".to_string(),
                                            detail: blocker,
                                        }),
                                    }
                                }
                                "tool_started" => {
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            // Replace a placeholder (started) row in place
                                            // rather than appending a duplicate.
                                            row.label = "tool".to_string();
                                            row.status = "running".to_string();
                                            row.detail = detail.clone();
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "tool".to_string(),
                                            status: "running".to_string(),
                                            detail: detail.clone(),
                                        }),
                                    }
                                }
                                "tool_finished" => {
                                    // Never insert on tool_finished; update the most recent
                                    // running row for this child_id, if any.
                                    let status_value = if detail.is_empty() {
                                        "completed".to_string()
                                    } else {
                                        detail
                                    };
                                    // Find the last running row for this child.
                                    let mut target_idx: Option<usize> = None;
                                    for (i, r) in children.iter().enumerate() {
                                        if r.child_id == child_id && r.status == "running" {
                                            target_idx = Some(i);
                                        }
                                    }
                                    if let Some(i) = target_idx {
                                        children[i].status = status_value;
                                        children[i].detail = children[i].status.clone();
                                    }
                                }
                                "finished" | "aborted" | "timed_out" => {
                                    // A child reaching a terminal state changes only its own
                                    // row. The card's aggregate status is finalized once, on
                                    // ToolExecutionEnd, so an early failure is not cleared by
                                    // a sibling finishing later.
                                    let (row_status, row_detail) = match event_kind {
                                        "finished" => ("completed", "completed"),
                                        "aborted" => ("failed", "aborted"),
                                        _ => ("failed", "timed_out"),
                                    };
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            row.status = row_status.to_string();
                                            row.detail = row_detail.to_string();
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "child".to_string(),
                                            status: row_status.to_string(),
                                            detail: row_detail.to_string(),
                                        }),
                                    }
                                }
                                _ => {}
                            }
                            // Same cap as the generic tool card's output preview.
                            while children.len() > 8 {
                                children.remove(0);
                            }
                            break;
                        }
                    }
                }
                // Subagent updates never target a generic tool card; stop here
                // instead of falling through into the ToolCall scan below.
                return;
            }
            for item in state.items.iter_mut().rev() {
                if let TranscriptItem::ToolCall {
                    id,
                    status,
                    output_preview,
                    ..
                } = item
                {
                    if id == tool_call_id {
                        *status = "running".to_string();
                        for block in &partial_result.content {
                            if let mehh_ai::ToolResultContentBlock::Text(ref tc) = block {
                                for line in tc.text.lines().filter(|l| !l.trim().is_empty()) {
                                    output_preview.push(line.to_string());
                                    if output_preview.len() > 8 {
                                        output_preview.remove(0);
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
            duration_ms,
            ..
        } => {
            state.status = None;
            if tool_name == "subagent" {
                for item in state.items.iter_mut().rev() {
                    if let TranscriptItem::Subagent {
                        id,
                        status,
                        is_error: err,
                        ..
                    } = item
                    {
                        if id == tool_call_id {
                            *status = if *is_error {
                                "failed".to_string()
                            } else {
                                "completed".to_string()
                            };
                            *err = *is_error;
                            break;
                        }
                    }
                }
                return;
            }
            for item in state.items.iter_mut().rev() {
                if let TranscriptItem::ToolCall {
                    id,
                    status,
                    is_error: err,
                    diff,
                    output_preview,
                    duration_ms: stored_duration,
                    ..
                } = item
                {
                    if id == tool_call_id {
                        *stored_duration = *duration_ms;
                        *status = if *is_error {
                            "failed".to_string()
                        } else {
                            "completed".to_string()
                        };
                        *err = *is_error;
                        if let Some(d) = result
                            .details
                            .as_ref()
                            .and_then(|m| m.get("diff"))
                            .and_then(|v| v.as_str())
                        {
                            *diff = Some(d.to_string());
                        }
                        if *is_error && output_preview.is_empty() {
                            for block in &result.content {
                                if let mehh_ai::ToolResultContentBlock::Text(ref tc) = block {
                                    for line in
                                        tc.text.lines().filter(|l| !l.trim().is_empty()).take(8)
                                    {
                                        output_preview.push(line.to_string());
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// Renders a full-screen frame with header, scrollable transcript tail, bottom controls/editor, and footer.
pub fn render_interactive_frame(
    state: &InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    width: usize,
    height: usize,
) -> Vec<String> {
    let width = width.max(20);
    let height = height.max(5);
    let theme = &state.theme;

    // 1. Header — a welcome card replaces the status strip on an empty session.
    let header_lines: Vec<String> = if state.items.is_empty() && editor.autocomplete.is_none() {
        let cwd_str = session.cwd.to_string_lossy();
        let rule = "─".repeat(width.saturating_sub(4).max(1));
        vec![
            String::new(),
            format!(
                "  {} v{}",
                theme.fg(&theme.accent, "mehh"),
                env!("CARGO_PKG_VERSION")
            ),
            format!(
                "  {} {} {}",
                theme.fg(&theme.text, &session.model.name),
                permission_badge(session.settings.permission_mode, theme),
                theme.fg(&theme.dim, &format!("· {cwd_str}"))
            ),
            String::new(),
            theme.fg(
                &theme.dim,
                "  /help commands   @file attach   Shift+Enter newline",
            ),
            theme.fg(&theme.border_muted, &format!("  {rule}")),
        ]
        .into_iter()
        .map(|line| truncate_to_width(&line, width, "...", false))
        .collect()
    } else {
        let short_id: String = session.session().session_id().chars().take(8).collect();
        let left = format!(
            "{} {} {} {}",
            theme.fg(&theme.accent, "mehh"),
            theme.fg(&theme.text, &session.model.name),
            theme.fg(&theme.dim, &format!("({})", session.model.provider)),
            permission_badge(session.settings.permission_mode, theme),
        );
        let right = format!(
            "session: {} · {}",
            theme.fg(&theme.warning, &short_id),
            compact_path(&session.cwd, 18)
        );
        let header_text = left_right(left, right, width);
        let sep_line = theme.fg(&theme.border_muted, &"─".repeat(width));
        vec![
            truncate_to_width(&header_text, width, "...", false),
            truncate_to_width(&sep_line, width, "", false),
        ]
    };

    // 2. Footer with a live context-window meter.
    let thinking_str = session
        .thinking_level()
        .map(thinking_level_label)
        .unwrap_or("off");
    let ctx_tokens = {
        let mut guard = state
            .cached_ctx_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = *guard {
            cached
        } else {
            let fresh = crate::compaction::estimate_context_tokens(
                &session.session().build_context_messages(),
            ) as u64;
            *guard = Some(fresh);
            fresh
        }
    };
    let ctx_pct = ctx_tokens
        .saturating_mul(100)
        .checked_div(session.model.context_window.max(1) as u64)
        .unwrap_or(0)
        .min(999);
    // The header already carries provider/model; the footer keeps live metrics only.
    let ctx_display_color = if ctx_pct >= 85 {
        &theme.error
    } else if ctx_pct >= 70 {
        &theme.warning
    } else {
        &theme.dim
    };
    let left = format!(
        "thinking: {} · ctx {} {}",
        thinking_str,
        theme.fg(ctx_display_color, &format!("{ctx_pct}%")),
        ctx_bar(ctx_pct, theme)
    );
    let right = format!(
        "{} in / {} out · ${:.4} · /help",
        compact_count(state.total_input_tokens),
        compact_count(state.total_output_tokens),
        state.total_cost
    );
    let footer_text = left_right(
        theme.fg(&theme.dim, &left),
        theme.fg(&theme.dim, &right),
        width,
    );
    let footer_lines = vec![truncate_to_width(&footer_text, width, "...", false)];

    // 3. Bottom controls
    let mut bottom_lines = Vec::new();
    if let Some(ref selector) = state.selector {
        let title = match selector.kind {
            SelectorKind::Model => "Select Model (Enter to confirm, Esc to cancel)",
            SelectorKind::Tree => "Select Branch (Enter to confirm, Esc to cancel)",
            SelectorKind::Resume => "Select Session (Enter to confirm, Esc to cancel)",
            SelectorKind::Settings => "Settings (Enter to cycle value, Esc to close)",
            SelectorKind::Login => "Select Provider (Enter to confirm, Esc to cancel)",
            SelectorKind::ToolApproval => "Tool Approval Required (y/Enter: allow once, a: allow session, A: always allow, n/Esc: deny)",
        };
        let selector_border = if selector.kind == SelectorKind::ToolApproval {
            &theme.warning
        } else {
            &theme.border_accent
        };
        let component: Box<dyn Component> = if selector.kind == SelectorKind::Settings {
            if let Some(ref sl) = selector.settings_list {
                Box::new(sl.clone().with_theme(theme.clone()))
            } else {
                Box::new(selector.list.clone().with_theme(theme.clone()))
            }
        } else {
            Box::new(selector.list.clone().with_theme(theme.clone()))
        };
        let search_text = search_bar_text(
            &selector.list.filter,
            selector.list.filtered_indices().len(),
            selector.list.items.len(),
            theme,
        );
        bottom_lines.push(truncate_to_width(&search_text, width, "...", false));
        if selector.kind == SelectorKind::ToolApproval {
            if let Some(ref pending) = state.pending_approval {
                let mut detail = format!(
                    "{}  {}\n{}\n",
                    theme.fg(&theme.accent, "tool"),
                    theme.fg(&theme.text, &pending.tool_name),
                    theme.fg(&theme.muted, &pending.reason)
                );
                if let Some(ref preview) = pending.preview {
                    let wrapped: Vec<String> = preview
                        .lines()
                        .flat_map(|line| wrap_text_with_ansi(line, width.saturating_sub(4)))
                        .collect();
                    let preview_budget = height.saturating_sub(18).clamp(1, 6);
                    detail.push_str(&cap_preview_lines(&wrapped.join("\n"), preview_budget));
                }
                let panel = BorderedBox::new(Box::new(Text::new(detail)))
                    .with_title("Command preview · verify before approving")
                    .with_style(BorderStyle::Rounded)
                    .with_border_color(selector_border)
                    .with_title_color(&theme.warning);
                bottom_lines.extend(panel.render(width));
            }
        }
        let boxed = BorderedBox::new(component)
            .with_title(title)
            .with_style(BorderStyle::Rounded)
            .with_border_color(selector_border)
            .with_title_color(&theme.text);
        bottom_lines.extend(boxed.render(width));
    }

    if state.is_busy {
        let spinner = mehh_tui::BRAILLE_FRAMES[state.loader.frame % mehh_tui::BRAILLE_FRAMES.len()];
        let now = std::time::Instant::now();
        let started = *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner());
        let completed = *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = elapsed_label(started.or(completed), completed.unwrap_or(now));
        let loader_line = format!(
            "{} {} {} · Esc to cancel",
            theme.fg(&theme.accent, spinner),
            theme.fg(&theme.text, &state.loader.label),
            theme.fg(&theme.dim, &elapsed)
        );
        let mut loader_lines = wrap_text_with_ansi(&loader_line, width);
        if let Some(ref st) = state.status {
            loader_lines.push(truncate_to_width(
                &format!("  {}", theme.fg(&theme.warning, st)),
                width,
                "...",
                false,
            ));
        }
        bottom_lines.extend(loader_lines);
    }

    // Queue counters are pills: print only the queues that actually hold messages.
    let steering = session.pending_steering_count();
    let follow_up = session.pending_follow_up_count();
    let mut queued = Vec::new();
    if steering > 0 {
        queued.push(format!("steering: {steering}"));
    }
    if follow_up > 0 {
        queued.push(format!("follow-up: {follow_up}"));
    }
    if !queued.is_empty() {
        bottom_lines.push(truncate_to_width(
            &format!("  {}", theme.fg(&theme.dim, &queued.join(" • "))),
            width,
            "...",
            false,
        ));
    }

    // Prompt dock: a thin two-line frame keeps the input visually anchored.
    // Re-theme the cloned view in both branches so `/theme` applies instantly,
    // including to autocomplete and paste chips, even while a picker is open.
    let mut themed_editor = editor.clone();
    themed_editor.theme = theme.clone();
    if let Some(ac) = themed_editor.autocomplete.as_mut() {
        ac.theme = theme.clone();
    }
    if state.selector.is_none() {
        let max_lines = (height / 3).clamp(3, 8);
        themed_editor.max_lines = max_lines;
        let ac_lines = themed_editor.render_autocomplete(width.saturating_sub(2));
        if !ac_lines.is_empty() {
            let panel = BorderedBox::new(Box::new(mehh_tui::RawLines { lines: ac_lines }))
                .with_title("Suggestions")
                .with_style(BorderStyle::Single)
                .with_border_color(theme.border_accent.as_str())
                .with_title_color(theme.text.as_str());
            bottom_lines.extend(panel.render(width));
        }
        let mut dock_title = if state.is_busy {
            "Steer (Enter) · Follow-up (Alt+Enter) · Cancel (Esc)".to_string()
        } else {
            "Prompt".to_string()
        };
        if themed_editor.lines.len() > 1 {
            dock_title.push_str(&format!(
                " · Ln {}/{}",
                themed_editor.cursor_row + 1,
                themed_editor.lines.len()
            ));
        }
        let dock_border = if state.is_busy {
            theme.warning.as_str()
        } else {
            theme.border.as_str()
        };
        let dock = BorderedBox::new(Box::new(themed_editor))
            .with_style(BorderStyle::Single)
            .with_border_color(dock_border)
            .with_title_color(theme.text.as_str())
            .with_title(dock_title);
        bottom_lines.extend(dock.render(width));
    } else {
        bottom_lines.extend(themed_editor.render(width));
    }

    // 4. Transcript
    let hide_thinking = session.settings.hide_thinking_block.unwrap_or(false);
    let mut transcript_lines = Vec::with_capacity(state.items.len().saturating_mul(4).max(64));
    let mut cache = state
        .transcript_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() > state.items.len().saturating_mul(2).saturating_add(16) {
        cache.clear();
    }
    for (item_idx, item) in state.items.iter().enumerate() {
        // Breathing room between turns; the transcript is a timeline, not a log dump.
        if item_idx > 0 {
            transcript_lines.push(String::new());
        }
        let needs_frame = match item {
            TranscriptItem::ToolCall { status, .. }
            | TranscriptItem::Subagent { status, .. } => status == "running",
            _ => false,
        };
        let thinking_elapsed_opt = state
            .thinking_elapsed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&item_idx)
            .copied();
        let thinking_elapsed = thinking_elapsed_opt.unwrap_or(0);
        let flags = (hide_thinking as u8)
            | ((state.thinking_expanded as u8) << 1)
            | (((state.is_busy && item_idx + 1 == state.items.len()) as u8) << 2);
        let key = (
            item_idx,
            width,
            flags,
            if needs_frame { state.loader.frame } else { 0 },
            thinking_elapsed,
        );
        if let Some((cached_theme, cached_item, cached_lines)) = cache.get(&key) {
            if cached_theme == &theme.name && cached_item == item {
                transcript_lines.extend(cached_lines.iter().cloned());
                continue;
            }
        }
        let render_start = transcript_lines.len();
        match item {
            TranscriptItem::User(text) => {
                transcript_lines.extend(render_user_turn(text, width, theme));
            }
            TranscriptItem::Assistant { thinking, text } => {
                if !thinking.is_empty() && !hide_thinking {
                    let started = *state
                        .turn_started_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let completed = *state
                        .turn_completed_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let elapsed_ms = match (started, completed) {
                        (Some(s), None) => Some(s.elapsed().as_millis() as u64),
                        (Some(s), Some(c)) => {
                            Some(c.saturating_duration_since(s).as_millis() as u64)
                        }
                        (None, _) => None,
                    };
                    let is_current = state.is_busy && item_idx + 1 == state.items.len();
                    let elapsed_ms = thinking_elapsed_opt.or(elapsed_ms);
                    let preview = thinking_preview(
                        thinking,
                        state.thinking_expanded,
                        is_current,
                        elapsed_ms,
                    );
                    let formatted = theme.fg(&theme.thinking_text, &preview);
                    transcript_lines.extend(wrap_text_with_ansi(&formatted, width));
                }
                if !text.is_empty() {
                    let rendered = MarkdownView::new(text)
                        .with_theme(theme.clone())
                        .render(width);
                    transcript_lines.extend(rendered);
                }
            }
            TranscriptItem::ToolCall {
                id: _id,
                name,
                status,
                is_error,
                args_summary,
                output_preview,
                diff,
                duration_ms,
            } => {
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let summary_part = if !args_summary.is_empty() {
                    format!(" {}", theme.fg(&theme.accent, args_summary))
                } else {
                    String::new()
                };
                let status_part = if status == "completed" {
                    String::new()
                } else {
                    format!(" {}", theme.fg(&theme.dim, status))
                };
                let duration_part = tool_duration_label(*duration_ms);
                let formatted = format!(
                    "  {sym} {}{summary_part}{status_part}{duration_part}",
                    theme.fg(&theme.text, name)
                );
                transcript_lines.extend(wrap_text_with_ansi(&formatted, width));

                for line in output_preview {
                    let preview_line = format!("    {}", theme.fg(&theme.dim, line));
                    transcript_lines.extend(wrap_text_with_ansi(&preview_line, width));
                }

                if let Some(diff_str) = diff {
                    let (visible, hidden) = diff_visible_lines(diff_str);
                    for line in visible {
                        let (color, bg): (&str, &str) =
                            if line.starts_with('+') && !line.starts_with("+++") {
                                (theme.tool_success.as_str(), theme.diff_add_bg.as_str())
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                (theme.tool_error.as_str(), theme.diff_del_bg.as_str())
                            } else if line.starts_with("@@") {
                                (theme.accent.as_str(), "")
                            } else {
                                (theme.dim.as_str(), "")
                            };
                        let colorized = format!("{color}{bg}{line}");
                        for wrapped in wrap_text_with_ansi(&format!("    {colorized}"), width) {
                            transcript_lines.push(format!("{wrapped}\x1b[0m"));
                        }
                    }
                    if hidden > 0 {
                        transcript_lines.push(format!(
                            "    {}",
                            theme.fg(
                                &theme.dim,
                                &format!("… (+{hidden} more lines)")
                            )
                        ));
                    }
                }
            }
            TranscriptItem::Subagent {
                id: _id,
                mode,
                status,
                is_error,
                children,
            } => {
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let formatted = format!(
                    "  {sym} subagent {} {}",
                    theme.fg(&theme.accent, mode),
                    theme.fg(&theme.dim, status)
                );
                transcript_lines.extend(wrap_text_with_ansi(&formatted, width));

                for row in children {
                    let row_sym =
                        status_icon(&row.status, false, state.loader.frame, theme);
                    let mut row_line =
                        format!("    {row_sym} {}", theme.fg(&theme.text, &row.label));
                    if row.label != row.status && !row.status.is_empty() {
                        row_line.push_str(&format!(": {}", theme.fg(&theme.dim, &row.status)));
                    }
                    // A finished tool row has detail == status ("completed");
                    // omit the redundant suffix instead of printing it twice.
                    if !row.detail.is_empty() && row.detail != row.status {
                        row_line.push_str(&format!(" {}", theme.fg(&theme.dim, &row.detail)));
                    }
                    transcript_lines.extend(wrap_text_with_ansi(&row_line, width));
                }
            }
            TranscriptItem::System(text) => {
                for sub in text.split('\n') {
                    let formatted = theme.fg(&theme.dim, &format!("• {sub}"));
                    transcript_lines.extend(wrap_text_with_ansi(&formatted, width));
                }
            }
        }
        let rendered: Vec<String> = transcript_lines[render_start..].to_vec();
        cache.insert(key, (theme.name.clone(), item.clone(), rendered));
    }

    let prev_len = state.prev_transcript_len.load(Ordering::Relaxed);
    let growth = transcript_lines.len().saturating_sub(prev_len);
    state
        .prev_transcript_len
        .store(transcript_lines.len(), Ordering::Relaxed);
    let anchor = anchored_scroll_offset(
        state.scroll_anchor.load(Ordering::Relaxed),
        growth,
        state.scroll_offset,
    );
    state.scroll_anchor.store(anchor, Ordering::Relaxed);
    let anchored = state.scroll_offset.saturating_add(anchor);

    if state.scroll_offset > 0 {
        let pill_height = 1usize;
        let pre_height = height.saturating_sub(
            header_lines.len() + footer_lines.len() + bottom_lines.len() + pill_height,
        );
        let max_scroll = transcript_lines.len().saturating_sub(pre_height);
        state.last_max_scroll.store(max_scroll, Ordering::Relaxed);
        let pill = scroll_pill_text(anchored.min(max_scroll), max_scroll, anchor);
        bottom_lines.push(truncate_to_width(
            &theme.fg(&theme.warning, &pill),
            width,
            "...",
            false,
        ));
    }

    // Keep the assembled frame within the terminal height: drop optional panel
    // rows from the front while preserving the dock at the end and approval actions.
    let bottom_budget = height
        .saturating_sub(header_lines.len() + footer_lines.len() + 1)
        .max(1);
    if bottom_lines.len() > bottom_budget {
        let mut excess = bottom_lines.len() - bottom_budget;
        let mut idx = 0;
        while excess > 0 && idx < bottom_lines.len().saturating_sub(3) {
            let line = &bottom_lines[idx];
            if line.contains("Allow") || line.contains("Deny") || line.contains("Suggestions") {
                idx += 1;
                continue;
            }
            bottom_lines.remove(idx);
            excess -= 1;
        }
    }

    // 5. Fit transcript into remaining vertical space
    let used_lines = header_lines.len() + footer_lines.len() + bottom_lines.len();
    let available_height = height.saturating_sub(used_lines);

    let mut visible_transcript = if available_height == 0 {
        state.last_max_scroll.store(0, Ordering::Relaxed);
        Vec::new()
    } else if transcript_lines.len() > available_height {
        let max_scroll = transcript_lines.len() - available_height;
        state.last_max_scroll.store(max_scroll, Ordering::Relaxed);
        let offset = anchored.min(max_scroll);
        let end = transcript_lines.len() - offset;
        let start = end.saturating_sub(available_height);
        transcript_lines[start..end].to_vec()
    } else {
        state.last_max_scroll.store(0, Ordering::Relaxed);
        transcript_lines.clone()
    };

    if transcript_lines.len() > available_height && width > 0 {
        let shown = visible_transcript.len();
        for (row, line) in visible_transcript.iter_mut().enumerate() {
            let base = truncate_to_width(line, width.saturating_sub(1), "...", false);
            let marker = scrollbar_char(row, shown, transcript_lines.len(), anchored);
            let marker_s = marker.to_string();
            let colored = if marker == '┃' {
                theme.fg(&theme.accent, &marker_s)
            } else {
                theme.fg(&theme.border_muted, &marker_s)
            };
            *line = format!("{base}{colored}");
        }
    }

    // Anchor the prompt dock and footer to the bottom row. Without this pad the
    // editor sits directly under the header and drifts down with every message.
    let pad = available_height.saturating_sub(visible_transcript.len());
    visible_transcript.extend(vec![String::new(); pad]);

    let mut frame = Vec::with_capacity(height);
    frame.extend(header_lines);
    frame.extend(visible_transcript);
    frame.extend(bottom_lines);
    frame.extend(footer_lines);

    frame
}

/// Signal delivered from a running agent turn or background prep/compaction task to the UI loop.
pub(crate) enum AgentSignal {
    ReadyForTurn {
        prompt_text: String,
        blocks: Vec<UserContentBlock>,
        new_api_key: Option<String>,
        compaction: Option<(String, String, u64)>,
        warning: Option<String>,
        cancel: CancellationToken,
    },
    PrepFailed(String),
    PrepAborted,
    OverflowCompacted {
        original_messages: Vec<Message>,
        prep: Option<(String, u64)>,
        result: Result<String, String>,
        cancelled: bool,
    },
    Event(Box<AgentEvent>),
    ToolApprovalRequest(PendingApproval),
    Finished(Vec<Message>),
    Failed(String),
}

/// Applies a signal from the agent task to the session and the interactive state.
///
/// Returns true when the signal ends the turn, so the caller can stop routing
/// user input into the finished turn.
fn apply_agent_signal(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    match signal {
        AgentSignal::ToolApprovalRequest(pending) => {
            // The reason and full preview live in their own panel; never hide them
            // inside a list row where truncate_to_width would cut the command.
            let allow_color = state.theme.success.clone();
            let deny_color = state.theme.error.clone();
            let items = vec![
                SelectItem::new("allow_once", "Allow once (y/Enter)")
                    .with_description("Approve this tool call once")
                    .with_label_color(allow_color.clone()),
                SelectItem::new("allow_session", "Allow for session (a)")
                    .with_description("Auto-approve this tool until session ends")
                    .with_label_color(allow_color.clone()),
                SelectItem::new("always_allow", "Always allow (A)")
                    .with_description("Persist always-allow rule to ~/.mehh/agent/permissions.json")
                    .with_label_color(allow_color),
                SelectItem::new("deny", "Deny (n/Esc)")
                    .with_description("Block this tool call")
                    .with_label_color(deny_color),
            ];
            state.pending_approval = Some(pending);
            state.selector = Some(Selector {
                kind: SelectorKind::ToolApproval,
                list: SelectList::new(items, 6),
                settings_list: None,
            });
            false
        }
        AgentSignal::Event(ev) => {
            apply_agent_event(state, &ev);
            if matches!(&*ev, AgentEvent::AgentEnd { .. }) {
                // Stay busy until the finished turn has been persisted.
                state.is_busy = true;
                state.status = None;
            }
            false
        }
        AgentSignal::Finished(messages) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *session.active_cancel_slot().lock().unwrap() = None;
            if let Some(Message::Assistant(a)) = messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    let notice = format_assistant_stop_notice(a);
                    push_turn_error_once(state, notice);
                }
            }
            if let Err(e) = session.finish_prompt(&messages) {
                state.items.push(TranscriptItem::System(format!(
                    "Failed to persist turn: {e}"
                )));
            }
            true
        }
        AgentSignal::Failed(message) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *session.active_cancel_slot().lock().unwrap() = None;
            state.items.push(TranscriptItem::System(format!(
                "Agent turn failed: {message}"
            )));
            true
        }
        AgentSignal::PrepFailed(message) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *session.active_cancel_slot().lock().unwrap() = None;
            state.items.push(TranscriptItem::System(message));
            true
        }
        AgentSignal::PrepAborted => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *session.active_cancel_slot().lock().unwrap() = None;
            state
                .items
                .push(TranscriptItem::System("Turn aborted.".to_string()));
            true
        }
        AgentSignal::ReadyForTurn { .. } | AgentSignal::OverflowCompacted { .. } => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalOutcome {
    Continuing,
    RetryStarted,
    TurnEnded,
}

async fn handle_agent_signal_async(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> SignalOutcome {
    match signal {
        AgentSignal::ReadyForTurn {
            prompt_text,
            blocks,
            new_api_key,
            compaction,
            warning,
            cancel,
        } => {
            if cancel.is_cancelled() {
                *session.active_cancel_slot().lock().unwrap() = None;
                state.is_busy = false;
                state.status = None;
                state
                    .items
                    .push(TranscriptItem::System("Turn aborted.".to_string()));
                return SignalOutcome::TurnEnded;
            }
            if let Some(key) = new_api_key {
                if !session.has_explicit_api_key()
                    && session.api_key.as_deref() != Some(key.as_str())
                {
                    session.set_api_key(Some(key));
                }
            }
            if let Some((summary, first_kept_id, tokens_before)) = compaction {
                if let Err(e) =
                    session.apply_prepared_compaction(summary, first_kept_id, tokens_before)
                {
                    state.items.push(TranscriptItem::System(format!(
                        "Warning: failed to persist proactive auto-compaction: {e}"
                    )));
                }
            }
            if let Some(w) = warning {
                state.items.push(TranscriptItem::System(format!(
                    "Warning: proactive auto-compaction failed: {w}"
                )));
            }
            start_turn(session, state, &prompt_text, blocks, agent_tx);
            SignalOutcome::RetryStarted
        }
        AgentSignal::Finished(messages) => {
            *session.active_cancel_slot().lock().unwrap() = None;
            if !state.overflow_retried && session.is_overflow_turn(&messages) {
                state.overflow_retried = true;
                if let Some(prep) = session.prepare_overflow_compaction() {
                    state.is_busy = true;
                    state.status = Some("Context overflow — compacting history...".to_string());
                    start_overflow_compaction(session, prep, messages, agent_tx);
                    return SignalOutcome::RetryStarted;
                }
            }
            if apply_agent_signal(AgentSignal::Finished(messages), session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
        AgentSignal::OverflowCompacted {
            original_messages,
            prep,
            result,
            cancelled,
        } => {
            *session.active_cancel_slot().lock().unwrap() = None;
            if !cancelled {
                if let (Ok(summary), Some((first_kept_id, tokens_before)), Some(retry_text)) =
                    (result.as_ref(), prep, state.last_prompt_text.clone())
                {
                    match session.apply_prepared_compaction(
                        summary.clone(),
                        first_kept_id,
                        tokens_before,
                    ) {
                        Ok(()) => {
                            if let Some(start_idx) = state.turn_start_idx {
                                state.items.truncate(start_idx);
                            }
                            state.last_turn_error = None;
                            state.items.push(TranscriptItem::System(
                                "Context overflow detected; auto-compacted history and retrying turn..."
                                    .to_string(),
                            ));
                            start_turn(session, state, &retry_text, Vec::new(), agent_tx);
                            return SignalOutcome::RetryStarted;
                        }
                        Err(e) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Failed to persist overflow auto-compaction: {e}"
                            )));
                        }
                    }
                } else if let Err(err) = result {
                    state.items.push(TranscriptItem::System(format!(
                        "Overflow auto-compaction failed: {err}"
                    )));
                }
            } else {
                state.items.push(TranscriptItem::System(
                    "Overflow auto-compaction aborted.".to_string(),
                ));
            }
            if apply_agent_signal(AgentSignal::Finished(original_messages), session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
        other => {
            if apply_agent_signal(other, session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
    }
}

/// Applies every signal that is already queued. Returns true when a turn ended without retry.
#[cfg(test)]
fn drain_agent_signals(
    agent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentSignal>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    let mut turn_ended = false;
    while let Ok(signal) = agent_rx.try_recv() {
        if apply_agent_signal(signal, session, state) {
            turn_ended = true;
        }
    }
    turn_ended
}

/// Applies every queued signal in interactive mode, handling automatic overflow retries.
/// Returns `Some(is_still_running)` when turn state changed.
async fn drain_agent_signals_async(
    agent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentSignal>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> Option<bool> {
    let mut next_running = None;
    while let Ok(signal) = agent_rx.try_recv() {
        match handle_agent_signal_async(signal, session, state, agent_tx).await {
            SignalOutcome::TurnEnded => next_running = Some(false),
            SignalOutcome::RetryStarted => next_running = Some(true),
            SignalOutcome::Continuing => {}
        }
    }
    next_running
}

/// Splits a submitted TUI prompt into arguments. Quotes group text so
/// `@"path with spaces"` stays one token, and newlines survive the round trip
/// through `process_file_arguments` (which rejoins args with spaces).
fn split_prompt_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            has_token = true;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                has_token = true;
            }
            '\n' | '\r' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
                args.push("\n".to_string());
            }
            c if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Expands `@path` tokens in a submitted TUI prompt into file/image blocks.
fn expand_prompt_with_files(
    cwd: &Path,
    text: &str,
) -> anyhow::Result<(String, Vec<UserContentBlock>)> {
    let args = split_prompt_args(text);
    if !args.iter().any(|token| token.starts_with('@')) {
        return Ok((text.to_string(), Vec::new()));
    }
    let processed = crate::process_file_arguments(cwd, &args)?;
    Ok((processed.text, processed.blocks))
}

/// Refreshes credentials and runs optional proactive compaction off the UI thread so the TUI and Ctrl+C remain responsive.
fn start_turn_preparation(
    session: &AgentSession,
    text: &str,
    blocks: Vec<UserContentBlock>,
    auth_file: Option<&Path>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let cancel = CancellationToken::new();
    *session.active_cancel_slot().lock().unwrap() = Some(cancel.clone());

    let prompt_text = text.to_string();
    let provider_name = session.model.provider.clone();
    let model_api = session.model.api.clone();
    let has_explicit_key = session.has_explicit_api_key();
    let auth_file = auth_file.map(Path::to_path_buf);
    let existing_key = session.api_key.clone();
    let model = session.model.clone();
    let provider = session.provider.clone();
    let proactive_prep = session.prepare_proactive_compaction();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();

    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(async {
            let key_res = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(AgentSignal::PrepAborted);
                    return;
                }
                res = AgentSession::refresh_resolved_api_key(
                    &provider_name,
                    &model_api,
                    has_explicit_key,
                    auth_file.as_deref(),
                ) => res,
            };
            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
                return;
            }
            let new_api_key = match key_res {
                Ok(k) => k,
                Err(e) => {
                    let _ = tx.send(AgentSignal::PrepFailed(format!(
                        "Failed to resolve provider credentials: {e}"
                    )));
                    return;
                }
            };

            let mut compaction = None;
            let mut warning = None;
            if let Some(prep) = proactive_prep {
                let effective_key = new_api_key.as_deref().or(existing_key.as_deref());
                match crate::compaction::compact_with_provider(
                    &prep,
                    &model,
                    provider.as_ref(),
                    effective_key,
                    None,
                    cancel.clone(),
                )
                .await
                {
                    Ok(summary) => {
                        compaction = Some((summary, prep.first_kept_entry_id, prep.tokens_before));
                    }
                    Err(e) => {
                        if cancel.is_cancelled() {
                            let _ = tx.send(AgentSignal::PrepAborted);
                            return;
                        }
                        warning = Some(e.to_string());
                    }
                }
            }

            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
            } else {
                let _ = tx.send(AgentSignal::ReadyForTurn {
                    prompt_text,
                    blocks,
                    new_api_key,
                    compaction,
                    warning,
                    cancel,
                });
            }
        })
        .catch_unwind()
        .await;

        if outcome.is_err() {
            let _ = tx_panic.send(AgentSignal::PrepFailed(
                "turn preparation panicked".to_string(),
            ));
        }
    });
}

fn start_overflow_compaction(
    session: &AgentSession,
    prep: crate::compaction::CompactionPreparation,
    original_messages: Vec<Message>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let cancel = CancellationToken::new();
    *session.active_cancel_slot().lock().unwrap() = Some(cancel.clone());

    let model = session.model.clone();
    let provider = session.provider.clone();
    let api_key = session.api_key.clone();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();
    let prep_meta = (prep.first_kept_entry_id.clone(), prep.tokens_before);

    tokio::spawn(async move {
        let res = AssertUnwindSafe(crate::compaction::compact_with_provider(
            &prep,
            &model,
            provider.as_ref(),
            api_key.as_deref(),
            None,
            cancel.clone(),
        ))
        .catch_unwind()
        .await;

        match res {
            Ok(compact_res) => {
                let cancelled = cancel.is_cancelled();
                let _ = tx.send(AgentSignal::OverflowCompacted {
                    original_messages,
                    prep: Some(prep_meta),
                    result: compact_res.map_err(|e| e.to_string()),
                    cancelled,
                });
            }
            Err(_) => {
                let _ = tx_panic.send(AgentSignal::Failed(
                    "overflow compaction panicked".to_string(),
                ));
            }
        }
    });
}

/// Starts an agent turn in a background task and streams its events to the UI loop.
fn start_turn(
    session: &AgentSession,
    state: &mut InteractiveState,
    text: &str,
    blocks: Vec<UserContentBlock>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.turn_start_idx = Some(state.items.len());
    state.last_turn_error = None;
    let mut content_blocks = blocks;
    if !content_blocks
        .iter()
        .any(|b| matches!(b, UserContentBlock::Text(_)))
    {
        content_blocks.insert(
            0,
            UserContentBlock::Text(mehh_ai::TextContent {
                text: text.to_string(),
                text_signature: None,
            }),
        );
    }
    let (user_msg, history, config) =
        session.prepare_prompt_content(UserContent::Blocks(content_blocks));

    let tx_events = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
        let _ = tx_events.send(AgentSignal::Event(Box::new(ev)));
    });

    tokio::spawn(async move {
        // A panic inside the agent loop must not wedge the UI: it is reported as a
        // failed turn so the loop can return to the prompt.
        let outcome = AssertUnwindSafe(run_agent_loop(
            vec![user_msg],
            history,
            config,
            Some(on_event),
        ))
        .catch_unwind()
        .await;

        match outcome {
            Ok(messages) => {
                let _ = tx_done.send(AgentSignal::Finished(messages));
            }
            Err(_) => {
                let _ = tx_done.send(AgentSignal::Failed("the agent loop panicked".to_string()));
            }
        }
    });

    state.is_busy = true;
    state.status = Some("Working...".to_string());
}

/// Refreshes the slash command autocomplete popup for the current editor text.
///
/// The popup displays the full command in the label while the id carries the text
/// inserted at the cursor, matching the file-completion path. A prefix that already
/// spells a command exactly leaves the popup closed, so `Enter` submits the command
/// instead of completing it a second time.
fn update_autocomplete_with_cwd(editor: &mut Editor, current_text: &str, cwd: &Path) {
    if current_text.starts_with('/') && !current_text.contains(' ') {
        let commands = builtin_slash_commands();
        if commands.iter().any(|cmd| cmd.id == current_text) {
            editor.clear_autocomplete();
            return;
        }

        let matches: Vec<SelectItem> = commands
            .into_iter()
            .filter(|cmd| cmd.id.starts_with(current_text))
            .map(|cmd| {
                let id = cmd.id;
                let description = cmd
                    .description
                    .unwrap_or_else(|| "Slash command".to_string());
                SelectItem::new(id.clone(), id).with_description(description)
            })
            .collect();

        if matches.is_empty() {
            editor.clear_autocomplete();
        } else {
            editor.set_autocomplete_replacing(matches, 0);
        }
        return;
    }

    let active_line = editor
        .lines
        .get(editor.cursor_row)
        .map(String::as_str)
        .unwrap_or("");
    if let Some(at_q) = crate::file_complete::extract_at_query(active_line, editor.cursor_col) {
        let matches = crate::file_complete::complete_file_paths(cwd, &at_q.query, 15);
        let items: Vec<SelectItem> = matches
            .into_iter()
            .map(|rel| SelectItem::new(format!("@{rel}"), rel))
            .collect();
        if items.is_empty() {
            editor.clear_autocomplete();
        } else {
            editor.set_autocomplete_replacing(items, at_q.replace_start);
        }
    } else {
        editor.clear_autocomplete();
    }
}

/// Renders the current frame to the terminal.
///
/// `TuiRenderer` positions changed rows absolutely, so only changed rows are
/// rewritten. `reset()` is reserved for a resize, where the previous frame's
/// geometry is no longer a valid baseline.
fn render_now(
    state: &InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    terminal: &mut dyn Terminal,
    renderer: &mut TuiRenderer,
) {
    let (c, r) = terminal.size();
    let lines = render_interactive_frame(state, editor, session, c as usize, r as usize);

    // The renderer positions changed rows absolutely, so no homing or reset is
    // needed here. A software cursor marks the input position.
    let _ = renderer.render_frame(&mut *terminal, lines, false);
}

/// Runs the full interactive TUI mode loop.
pub async fn run_interactive_mode(
    session: &mut AgentSession,
    mcp_summaries: &[McpConnectionSummary],
    mut mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> anyhow::Result<()> {
    let mut terminal = CrosstermTerminal::new()?;
    // CrosstermTerminal hides the hardware cursor and enters the alternate screen.
    let mut renderer = TuiRenderer::new();
    let mut state = InteractiveState::new(mcp_summaries.to_vec(), &session.settings);
    let mut editor = Editor::new()
        .with_prefix("> ")
        .with_placeholder("Type a message or /help...")
        .with_theme(state.theme.clone());
    let keybindings = state.keybindings.clone();

    // Background input reader thread
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<crossterm::event::Event>(128);
    std::thread::spawn(move || {
        while let Ok(evt) = crossterm::event::read() {
            if event_tx.blocking_send(evt).is_err() {
                break;
            }
        }
    });

    // Agent event channel. The extra sender keeps the channel open between turns.
    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let _agent_keepalive = agent_tx.clone();

    let approval_broker = Arc::new(InteractiveApprovalBroker::new(agent_tx.clone()));
    session.set_approval_handler(Some(approval_broker));

    let mut tick_interval = tokio::time::interval(Duration::from_millis(80));
    let mut turn_running = false;
    let mut wheel_accelerator = WheelScrollAccelerator::default();

    // Render initial screen
    render_now(&state, &editor, session, &mut terminal, &mut renderer);

    'outer: loop {
        tokio::select! {
            evt = event_rx.recv() => {
                let Some(evt) = evt else { break 'outer; };
                match evt {
                    Event::Resize(_new_cols, _new_rows) => {
                        renderer.reset();
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            let lines = wheel_accelerator.next(-1, now_ms);
                            let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                            state.scroll_offset =
                                state.scroll_offset.saturating_add(lines).min(max_scroll);
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                        MouseEventKind::ScrollDown => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            let lines = wheel_accelerator.next(1, now_ms);
                            let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                            state.scroll_offset =
                                state.scroll_offset.min(max_scroll).saturating_sub(lines);
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                        _ => {}
                    },
                    Event::Paste(pasted) => {
                        editor.insert_text(&pasted);
                        let text = editor.text();
                        update_autocomplete_with_cwd(&mut editor, &text, &session.cwd);
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    Event::Key(key_event) => {
                        // Apply agent signals that are already queued so a key press
                        // cannot start a new turn before the previous one was persisted.
                        if let Some(running) = drain_agent_signals_async(
                            &mut agent_rx,
                            session,
                            &mut state,
                            &agent_tx,
                        )
                        .await
                        {
                            turn_running = running;
                        }
                        if let Some(key_str) = normalize_crossterm_key(key_event) {
                            if key_str == "ctrl+t" {
                                state.thinking_expanded = !state.thinking_expanded;
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            // 1. Selector overlay handling
                            if state.selector.is_some() {
                                if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                                    let mut decision: Option<bool> = None;
                                    let mut persist_rule = false;
                                    let mut session_rule = false;
                                    match key_str.as_str() {
                                        "y" => decision = Some(true),
                                        "a" => {
                                            decision = Some(true);
                                            session_rule = true;
                                        }
                                        "A" => {
                                            decision = Some(true);
                                            persist_rule = true;
                                        }
                                        "n" | "escape" => decision = Some(false),
                                        "enter" => {
                                            let selected_id = state
                                                .selector
                                                .as_ref()
                                                .and_then(|s| s.list.selected_item())
                                                .map(|i| i.id.as_str())
                                                .unwrap_or("allow_once");
                                            match selected_id {
                                                "allow_once" => decision = Some(true),
                                                "allow_session" => {
                                                    decision = Some(true);
                                                    session_rule = true;
                                                }
                                                "always_allow" => {
                                                    decision = Some(true);
                                                    persist_rule = true;
                                                }
                                                _ => decision = Some(false),
                                            }
                                        }
                                        "up" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.move_up();
                                            }
                                        }
                                        "down" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.move_down();
                                            }
                                        }
                                        "pageup" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.page_up();
                                            }
                                        }
                                        "pagedown" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.page_down();
                                            }
                                        }
                                        _ => {}
                                    }

                                    if let Some(allowed) = decision {
                                        state.selector = None;
                                        if let Some(pending) = state.pending_approval.take() {
                                            if persist_rule || session_rule {
                                                let arg_prefix = pending
                                                    .args
                                                    .get("command")
                                                    .and_then(|v| v.as_str())
                                                    .and_then(mehh_execpolicy::safe_rule_prefix);
                                                if mehh_execpolicy::is_shell_tool(&pending.tool_name)
                                                    && arg_prefix.is_none()
                                                {
                                                    // An empty prefix would allow every command for
                                                    // this shell. Refuse instead of widening policy.
                                                    state.items.push(TranscriptItem::System(
                                                        "No rule saved: command is compound or too broad to allow safely".to_string(),
                                                    ));
                                                } else {
                                                    let rule = crate::permissions::PermissionRule {
                                                        tool_name: pending.tool_name.clone(),
                                                        arg_prefix,
                                                        path_glob: None,
                                                        action: crate::permissions::PermissionAction::AlwaysAllow,
                                                    };
                                                    if persist_rule {
                                                        let _ = session.inspection_manager.add_permission_rule(rule).await;
                                                    } else {
                                                        session.inspection_manager.add_session_permission_rule(rule).await;
                                                    }
                                                    let scope = if persist_rule { "Rule" } else { "Session rule" };
                                                    state.items.push(TranscriptItem::System(format!(
                                                        "{scope} added: always allow tool '{}'",
                                                        pending.tool_name
                                                    )));
                                                }
                                            }
                                            let _ = pending.reply_tx.send(allowed);
                                        }
                                    }
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }

                                if let Some(sel) = state.selector.as_mut() {
                                    if update_selector_filter(sel, &key_str) {
                                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        continue 'outer;
                                    }
                                }

                                match key_str.as_str() {
                                    "enter" | " " | "space"
                                        if key_str == "enter"
                                            || state.selector.as_ref().map(|s| s.kind)
                                                == Some(SelectorKind::Settings) =>
                                    {
                                        let kind = state.selector.as_ref().map(|s| s.kind);
                                        let selected = state
                                            .selector
                                            .as_ref()
                                            .and_then(|s| s.list.selected_item())
                                            .cloned();
                                        // The settings picker stays open so several
                                        // values can be cycled in a row.
                                        if kind != Some(SelectorKind::Settings) {
                                            state.selector = None;
                                        }
                                        match (kind, selected) {
                                            (Some(SelectorKind::Model), Some(item)) => {
                                                apply_interactive_model_switch(
                                                    &item.id,
                                                    session,
                                                    &mut state,
                                                )
                                                .await;
                                            }
                                            (Some(SelectorKind::Tree), Some(item)) => {
                                                match session.navigate_tree(&item.id, false, None).await {
                                                    Ok(_) => {
                                                        state.items = rebuild_transcript_items(session);
                                                        state.scroll_offset = 0;
                                                        state.items.push(TranscriptItem::System(format!(
                                                            "Switched to branch at {}", item.id
                                                        )));
                                                    }
                                                    Err(e) => state.items.push(TranscriptItem::System(format!(
                                                         "Failed to switch branch: {e}"
                                                    ))),
                                                }
                                            }
                                            (Some(SelectorKind::Resume), Some(item)) => {
                                                match SessionManager::open_file(
                                                    PathBuf::from(&item.id),
                                                    session.cwd.clone(),
                                                ) {
                                                    Ok(mgr) => {
                                                        session.session = mgr;
                                                        state.items = rebuild_transcript_items(session);
                                                        state.scroll_offset = 0;
                                                        state.items.push(TranscriptItem::System(format!(
                                                            "Resumed session {}",
                                                            session.session.session_id()
                                                        )));
                                                    }
                                                    Err(e) => state.items.push(TranscriptItem::System(format!(
                                                        "Failed to resume session: {e}"
                                                    ))),
                                                }
                                            }
                                            (Some(SelectorKind::Settings), Some(item)) => {
                                                match apply_setting_cycle(&item.id, session, &mut state) {
                                                    Ok(msg) => {
                                                        match persist_settings(
                                                            &session.settings,
                                                            &item.id,
                                                            state.settings_file.as_deref(),
                                                        ) {
                                                            Ok(()) => state.items.push(TranscriptItem::System(
                                                                format!("Updated {msg}")
                                                            )),
                                                            Err(e) => state.items.push(TranscriptItem::System(
                                                                format!("Updated {msg} but saving settings failed: {e}")
                                                            )),
                                                        }
                                                    }
                                                    Err(e) => state.items.push(TranscriptItem::System(
                                                        format!("Failed to update setting: {e}")
                                                    )),
                                                }
                                                // Refresh the labels so they show the new value.
                                                refresh_settings_selector(&mut state, &session.settings);
                                            }
                                            (Some(SelectorKind::Login), Some(item)) => {
                                                match perform_login(
                                                    &item.id,
                                                    None,
                                                    state.auth_file.as_deref(),
                                                ).await {
                                                    Ok(msg) => {
                                                        match refresh_session_api_key(
                                                            session,
                                                            &item.id,
                                                            state.auth_file.as_deref(),
                                                        ).await {
                                                            Ok(()) => state.items.push(TranscriptItem::System(msg)),
                                                            Err(e) => state.items.push(TranscriptItem::System(
                                                                format!("{msg} (could not refresh the session key: {e})")
                                                            )),
                                                        }
                                                    }
                                                    Err(e) => state.items.push(TranscriptItem::System(
                                                        format!("Login failed: {e}")
                                                    )),
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    "escape" => {
                                        state.selector = None;
                                    }
                                    "up" => {
                                        if let Some(sel) = state.selector.as_mut() {
                                            sel.list.move_up();
                                            if let Some(ref mut sl) = sel.settings_list {
                                                sl.move_up();
                                            }
                                        }
                                    }
                                    "down" => {
                                        if let Some(sel) = state.selector.as_mut() {
                                            sel.list.move_down();
                                            if let Some(ref mut sl) = sel.settings_list {
                                                sl.move_down();
                                            }
                                        }
                                    }
                                    "pageup" => {
                                        if let Some(sel) = state.selector.as_mut() {
                                            sel.list.page_up();
                                            if let Some(ref mut sl) = sel.settings_list {
                                                sl.page_up();
                                            }
                                        }
                                    }
                                    "pagedown" => {
                                        if let Some(sel) = state.selector.as_mut() {
                                            sel.list.page_down();
                                            if let Some(ref mut sl) = sel.settings_list {
                                                sl.page_down();
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }

                            // 2. Interrupt handling depends on whether a turn is running
                            if turn_running {
                                if keybindings.matches(&key_str, TuiAction::Interrupt) {
                                    let aborted = session.abort();
                                    state.items.push(TranscriptItem::System(
                                        if aborted {
                                            "Cancelling current turn...".to_string()
                                        } else {
                                            "No running turn to cancel.".to_string()
                                        },
                                    ));
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }

                                if key_str == "alt+enter" {
                                    let text = editor.text();
                                    let trimmed = text.trim().to_string();
                                    if !trimmed.is_empty() {
                                        session.follow_up(trimmed.clone());
                                        editor.clear();
                                        state.items.push(TranscriptItem::System(format!(
                                            "Queued follow-up: {trimmed}"
                                        )));
                                    }
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }
                            } else if keybindings.matches(&key_str, TuiAction::Interrupt) && editor.is_empty() {
                                break 'outer;
                            }

                            // Scroll keybindings operate regardless of turn state when selector is closed
                            if keybindings.matches(&key_str, TuiAction::ScrollUp) {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                state.scroll_offset =
                                    state.scroll_offset.saturating_add(5).min(max_scroll);
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            if keybindings.matches(&key_str, TuiAction::ScrollDown) {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                state.scroll_offset =
                                    state.scroll_offset.min(max_scroll).saturating_sub(5);
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            if keybindings.matches(&key_str, TuiAction::ScrollBottom) {
                                state.scroll_offset = 0;
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }

                            // 3. Normal editor input handling
                            let output = editor.handle_editor_key(&key_str, &keybindings);
                            match output {
                                EditorOutput::Submit(submitted) => {
                                    let trimmed = submitted.trim();
                                    if trimmed.is_empty() {
                                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        continue 'outer;
                                    }

                                    if turn_running {
                                        match route_busy_input(trimmed) {
                                            BusyInput::Steer(text) => {
                                                session.steer(text.clone());
                                                state.items.push(TranscriptItem::System(format!(
                                                    "Queued steering message: {text}"
                                                )));
                                            }
                                            BusyInput::FollowUp(text) => {
                                                session.follow_up(text.clone());
                                                state.items.push(TranscriptItem::System(format!(
                                                    "Queued follow-up: {text}"
                                                )));
                                            }
                                        }
                                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        continue 'outer;
                                    }

                                    match handle_slash_command(
                                        trimmed,
                                        session,
                                        &mut state,
                                        mcp_manager.as_deref_mut(),
                                        codemode_enabled,
                                    ).await {
                                        SlashCommandOutcome::Quit => {
                                            break 'outer;
                                        }
                                        SlashCommandOutcome::Handled => {
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::Compact(custom_instructions) => {
                                            state.is_busy = true;
                                            state.status = Some("Compacting session history...".to_string());
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);

                                            let compact_res = session.compact(custom_instructions.as_deref()).await;
                                            state.is_busy = false;
                                            state.status = None;

                                            match compact_res {
                                                Ok(Some(summary)) => {
                                                    state.items.push(TranscriptItem::System(format!(
                                                        "Compaction complete: {summary}"
                                                    )));
                                                }
                                                Ok(None) => {
                                                    state.items.push(TranscriptItem::System(
                                                        "History below compaction threshold; no compaction performed.".to_string(),
                                                    ));
                                                }
                                                Err(e) => {
                                                    state.items.push(TranscriptItem::System(format!(
                                                        "Compaction error: {e}"
                                                    )));
                                                }
                                            }

                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::NotACommand => {
                                            if let Some(ref mut mgr) = mcp_manager {
                                                if mgr.take_tools_dirty() {
                                                    if let Ok(true) = mgr.refresh_server_tools().await {
                                                        sync_mcp_tools(session, mgr.tools().to_vec(), codemode_enabled);
                                                        state.items.push(TranscriptItem::System(
                                                            "MCP tools updated via tools/list_changed.".to_string(),
                                                        ));
                                                    }
                                                }
                                            }
                                            let (expanded_text, prompt_blocks) =
                                                match expand_prompt_with_files(&session.cwd, trimmed) {
                                                    Ok(v) => v,
                                                    Err(e) => {
                                                        state.items.push(TranscriptItem::System(
                                                            format!("Failed to attach @file: {e}"),
                                                        ));
                                                        render_now(
                                                            &state,
                                                            &editor,
                                                            session,
                                                            &mut terminal,
                                                            &mut renderer,
                                                        );
                                                        continue 'outer;
                                                    }
                                                };
                                            state.scroll_offset = 0;
                                            state.items.push(TranscriptItem::User(trimmed.to_string()));
                                            state.last_prompt_text = Some(trimmed.to_string());
                                            state.overflow_retried = false;
                                            state.is_busy = true;
                                            state.status = Some("Preparing turn...".to_string());
                                            *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
                                            *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                            start_turn_preparation(
                                                session,
                                                &expanded_text,
                                                prompt_blocks,
                                                state.auth_file.as_deref(),
                                                &agent_tx,
                                            );
                                            turn_running = true;
                                        }
                                    }
                                }
                                EditorOutput::Changed(current_text) => {
                                    update_autocomplete_with_cwd(&mut editor, &current_text, &session.cwd);
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                }
                                EditorOutput::None => {
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some(sig) = agent_rx.recv() => {
                match handle_agent_signal_async(sig, session, &mut state, &agent_tx).await {
                    SignalOutcome::TurnEnded => {
                        turn_running = false;
                        let finished = std::time::Instant::now();
                        *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(finished);
                        if let (Some(started), Some(idx)) = (
                            *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()),
                            state.items.iter().rposition(|it| matches!(it, TranscriptItem::Assistant { .. })),
                        ) {
                            state.thinking_elapsed.lock().unwrap_or_else(|e| e.into_inner()).insert(
                                idx,
                                finished.saturating_duration_since(started).as_millis() as u64,
                            );
                        }
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    SignalOutcome::RetryStarted => {
                        turn_running = true;
                        *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
                        *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    SignalOutcome::Continuing => {
                        if state.selector.is_some() {
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                    }
                }
                // Streaming updates are painted by the 80 ms tick, so a burst of
                // token deltas does not rebuild the frame for every delta.
            }
            _ = tick_interval.tick(), if turn_running => {
                state.loader.tick();
                render_now(&state, &editor, session, &mut terminal, &mut renderer);
            }
        }
    }

    let _ = terminal.write("\n");
    let _ = terminal.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use crate::settings::Settings;
    use mehh_ai::{Model, ModelCost, Usage, UsageCost};

    fn make_test_session() -> AgentSession {
        let temp_dir = std::env::temp_dir().join(format!("mehh_test_int_{}", uuid::Uuid::now_v7()));
        let session_mgr = SessionManager::in_memory(&temp_dir);
        let faux = mehh_ai::FauxProvider::new();
        let model = Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };
        AgentSession::new(
            temp_dir,
            model,
            Arc::new(faux),
            session_mgr,
            Settings::default(),
        )
    }

    fn make_test_state() -> InteractiveState {
        InteractiveState::new(Vec::new(), &Settings::default())
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_handle_slash_command() {
        let mut session = make_test_session();
        let mut state = InteractiveState::new(
            vec![McpConnectionSummary {
                server: "test_mcp".to_string(),
                tool_count: 3,
                error: None,
            }],
            &Settings::default(),
        );

        // 1. Not a command
        assert_eq!(
            handle_slash_command("hello world", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::NotACommand
        );

        // 2. /help
        assert_eq!(
            handle_slash_command("/help", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("Commands:"))
        );

        // 3. /thinking high
        assert_eq!(
            handle_slash_command("/thinking high", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::High));
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("high"))
        );

        // 4. /tools
        assert_eq!(
            handle_slash_command("/tools", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("Active tools"))
        );

        // 5. /mcp
        assert_eq!(
            handle_slash_command("/mcp", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("test_mcp") && s.contains("3 tools"))
        );

        // 6. /clear
        assert!(!state.items.is_empty());
        assert_eq!(
            handle_slash_command("/clear", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(state.items.is_empty());

        // 7. /exit
        assert_eq!(
            handle_slash_command("/exit", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Quit
        );

        // 8. /quit
        assert_eq!(
            handle_slash_command("/quit", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Quit
        );
    }

    #[test]
    fn test_apply_agent_event() {
        let mut state = make_test_state();

        // AgentStart
        apply_agent_event(&mut state, &AgentEvent::AgentStart);
        assert!(state.is_busy);

        // MessageStart
        let dummy_assistant = mehh_ai::AssistantMessage {
            content: Vec::new(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "test-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: mehh_ai::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        apply_agent_event(
            &mut state,
            &AgentEvent::MessageStart {
                message: Message::Assistant(dummy_assistant.clone()),
            },
        );
        assert_eq!(state.items.len(), 1);

        // Thinking delta
        apply_agent_event(
            &mut state,
            &AgentEvent::MessageUpdate {
                message: dummy_assistant.clone(),
                assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "thinking part 1... ".to_string(),
                    partial: dummy_assistant.clone(),
                },
            },
        );
        assert_eq!(
            state.items[0],
            TranscriptItem::Assistant {
                thinking: "thinking part 1... ".to_string(),
                text: String::new(),
            }
        );

        // Text delta
        apply_agent_event(
            &mut state,
            &AgentEvent::MessageUpdate {
                message: dummy_assistant.clone(),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 1,
                    delta: "Hello, world!".to_string(),
                    partial: dummy_assistant.clone(),
                },
            },
        );
        assert_eq!(
            state.items[0],
            TranscriptItem::Assistant {
                thinking: "thinking part 1... ".to_string(),
                text: "Hello, world!".to_string(),
            }
        );

        // Tool execution start
        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionStart {
                tool_call_id: "call_1".to_string(),
                tool_name: "read_file".to_string(),
                args: serde_json::Map::new(),
            },
        );
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.status, Some("Executing read_file...".to_string()));
        assert_eq!(
            state.items[1],
            TranscriptItem::ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                status: "running".to_string(),
                is_error: false,
                args_summary: String::new(),
                output_preview: Vec::new(),
                diff: None,
                duration_ms: None,
            }
        );

        // Tool execution end
        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionEnd {
                tool_call_id: "call_1".to_string(),
                tool_name: "read_file".to_string(),
                result: mehh_agent::AgentToolResult::text("file content"),
                is_error: false,
                duration_ms: None,
            },
        );
        assert_eq!(state.status, None);
        assert_eq!(
            state.items[1],
            TranscriptItem::ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                status: "completed".to_string(),
                is_error: false,
                args_summary: String::new(),
                output_preview: Vec::new(),
                diff: None,
                duration_ms: None,
            }
        );

        // MessageEnd accumulates usage
        let mut finished_assistant = dummy_assistant;
        finished_assistant.usage = Usage {
            input: 120,
            output: 45,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 165,
            cost: UsageCost {
                input: 0.001,
                output: 0.002,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.003,
            },
        };
        apply_agent_event(
            &mut state,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(finished_assistant),
            },
        );
        assert_eq!(state.total_input_tokens, 120);
        assert_eq!(state.total_output_tokens, 45);
        assert!((state.total_cost - 0.003).abs() < 1e-6);

        // AgentEnd
        apply_agent_event(&mut state, &AgentEvent::AgentEnd { messages: vec![] });
        assert!(!state.is_busy);
    }

    #[test]
    fn test_subagent_group_renders_nested_child_rows_keyed_by_tool_call_id() {
        let mut state = make_test_state();
        let tool_id = "tc_sub_1".to_string();

        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionStart {
                tool_call_id: tool_id.clone(),
                tool_name: "subagent".to_string(),
                args: serde_json::json!({"mode": "coding"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        );
        assert!(
            matches!(state.items.last().unwrap(), TranscriptItem::Subagent { mode, .. } if mode == "coding")
        );

        let child_update = |id: &str, kind: &str, detail: &str| AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_id.clone(),
            tool_name: "subagent".to_string(),
            args: serde_json::Map::new(),
            partial_result: mehh_agent::AgentToolResult {
                details: Some(serde_json::json!({"subagent": {
                    "child_id": id, "mode": "coding", "event_kind": kind, "detail": detail
                }})),
                ..Default::default()
            },
        };
        apply_agent_event(&mut state, &child_update("sub-a", "tool_started", "read"));
        apply_agent_event(&mut state, &child_update("sub-b", "tool_started", "read"));
        apply_agent_event(
            &mut state,
            &child_update("sub-b", "tool_finished", "completed"),
        );

        match state.items.last().unwrap() {
            TranscriptItem::Subagent { children, .. } => {
                assert_eq!(children.len(), 2);
                let a = children.iter().find(|r| r.child_id == "sub-a").unwrap();
                let b = children.iter().find(|r| r.child_id == "sub-b").unwrap();
                assert_eq!(a.label, "tool");
                assert_eq!(a.detail, "read");
                assert_eq!(a.status, "running");
                assert_eq!(b.label, "tool");
                assert_eq!(b.detail, "completed");
                assert_eq!(b.status, "completed");
            }
            _ => panic!("expected subagent group"),
        }

        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionStart {
                tool_call_id: "tc_sub_2".to_string(),
                tool_name: "subagent".to_string(),
                args: serde_json::Map::new(),
            },
        );
        assert_eq!(
            state
                .items
                .iter()
                .filter(|i| matches!(i, TranscriptItem::Subagent { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn test_subagent_group_render_shows_child_rows() {
        let session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::Subagent {
            id: "tc_x".into(),
            mode: "coding".into(),
            status: "running".into(),
            is_error: false,
            children: vec![SubagentChildRow {
                child_id: "sub-a".into(),
                label: "tool".into(),
                status: "running".into(),
                detail: "read".into(),
            }],
        });
        let editor = Editor::new().with_prefix("> ");
        let frame = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        let plain = mehh_tui::strip_ansi(&frame);
        assert!(plain.contains("subagent"), "header must name the tool");
        assert!(
            plain.contains("tool: running read"),
            "child row must render label, status and detail: {plain}"
        );
    }

    #[test]
    fn test_subagent_finished_tool_row_does_not_repeat_status() {
        let session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::Subagent {
            id: "tc_done".into(),
            mode: "coding".into(),
            status: "completed".into(),
            is_error: false,
            // A finished tool row carries detail == status.
            children: vec![SubagentChildRow {
                child_id: "sub-done".into(),
                label: "tool".into(),
                status: "completed".into(),
                detail: "completed".into(),
            }],
        });
        let editor = Editor::new().with_prefix("> ");
        let frame = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        let plain = mehh_tui::strip_ansi(&frame);
        let row = plain
            .lines()
            .find(|line| line.contains("tool"))
            .expect("child row must render");
        assert!(
            row.contains("✓ tool: completed"),
            "expected completed row: {row}"
        );
        assert!(
            !row.contains("completed completed"),
            "status must not be duplicated as detail: {row}"
        );
    }

    #[test]
    fn test_subagent_child_lifecycle_events_populate_rows() {
        let mut state = make_test_state();
        let tool_id = "tc_sub_lc".to_string();
        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionStart {
                tool_call_id: tool_id.clone(),
                tool_name: "subagent".to_string(),
                args: serde_json::json!({"mode": "coding"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        );

        let child_update = |id: &str, kind: &str, detail: &str| AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_id.clone(),
            tool_name: "subagent".to_string(),
            args: serde_json::Map::new(),
            partial_result: mehh_agent::AgentToolResult {
                details: Some(serde_json::json!({"subagent": {
                    "child_id": id, "mode": "coding", "event_kind": kind, "detail": detail
                }})),
                ..Default::default()
            },
        };
        // started inserts a placeholder row before any tool runs.
        apply_agent_event(&mut state, &child_update("c1", "started", ""));
        match state.items.last().unwrap() {
            TranscriptItem::Subagent { children, .. } => {
                let c1 = children
                    .iter()
                    .find(|r| r.child_id == "c1")
                    .expect("started row");
                assert_eq!(c1.label, "started");
                assert_eq!(c1.status, "running");
            }
            _ => panic!("expected subagent group"),
        }
        // blocked inserts a row when the child has no placeholder yet.
        apply_agent_event(&mut state, &child_update("c2", "blocked", "needs approval"));
        // started -> blocked must update the existing row, not add a duplicate.
        apply_agent_event(&mut state, &child_update("c3", "started", ""));
        apply_agent_event(&mut state, &child_update("c3", "blocked", "needs review"));
        // tool_finished without prior tool_started is ignored.
        apply_agent_event(
            &mut state,
            &child_update("ghost", "tool_finished", "completed"),
        );
        // tool_started replaces the placeholder row.
        apply_agent_event(&mut state, &child_update("c1", "tool_started", "read"));
        // tool_finished updates the running row.
        apply_agent_event(
            &mut state,
            &child_update("c1", "tool_finished", "completed"),
        );

        match state.items.last().unwrap() {
            TranscriptItem::Subagent { children, .. } => {
                assert_eq!(children.len(), 3);
                let c1 = children.iter().find(|r| r.child_id == "c1").unwrap();
                assert_eq!(c1.label, "tool");
                assert_eq!(c1.detail, "completed");
                assert_eq!(c1.status, "completed");
                let c2 = children.iter().find(|r| r.child_id == "c2").unwrap();
                assert_eq!(c2.label, "blocked");
                assert_eq!(c2.status, "blocked");
                assert_eq!(c2.detail, "needs approval");
                let c3_rows: Vec<_> = children.iter().filter(|r| r.child_id == "c3").collect();
                assert_eq!(
                    c3_rows.len(),
                    1,
                    "blocked must update the started row in place"
                );
                assert_eq!(c3_rows[0].label, "blocked");
                assert_eq!(c3_rows[0].status, "blocked");
                assert_eq!(c3_rows[0].detail, "needs review");
                assert!(children.iter().all(|r| r.child_id != "ghost"));
            }
            _ => panic!("expected subagent group"),
        }
    }

    #[test]
    fn test_subagent_terminal_events_update_only_the_matching_child_row() {
        let mut state = make_test_state();
        let tool_id = "tc_sub_batch".to_string();
        apply_agent_event(
            &mut state,
            &AgentEvent::ToolExecutionStart {
                tool_call_id: tool_id.clone(),
                tool_name: "subagent".to_string(),
                args: serde_json::Map::new(),
            },
        );

        let child_update = |id: &str, kind: &str| AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_id.clone(),
            tool_name: "subagent".to_string(),
            args: serde_json::Map::new(),
            partial_result: mehh_agent::AgentToolResult {
                details: Some(serde_json::json!({"subagent": {
                    "child_id": id, "mode": "coding", "event_kind": kind, "detail": ""
                }})),
                ..Default::default()
            },
        };

        apply_agent_event(&mut state, &child_update("c1", "started"));
        apply_agent_event(&mut state, &child_update("c2", "started"));
        // c2 fails first; the group must stay running until ToolExecutionEnd.
        apply_agent_event(&mut state, &child_update("c2", "aborted"));
        // c1 finishing later must not clear c2's failure nor close the group.
        apply_agent_event(&mut state, &child_update("c1", "finished"));

        match state.items.last().unwrap() {
            TranscriptItem::Subagent {
                status,
                is_error,
                children,
                ..
            } => {
                assert_eq!(status, "running", "group closes only on ToolExecutionEnd");
                assert!(!is_error, "group error is final only on ToolExecutionEnd");
                let c1 = children.iter().find(|r| r.child_id == "c1").unwrap();
                let c2 = children.iter().find(|r| r.child_id == "c2").unwrap();
                assert_eq!(c1.status, "completed");
                assert_eq!(c2.status, "failed");
            }
            _ => panic!("expected subagent group"),
        }
    }

    #[test]
    fn test_render_interactive_frame() {
        let session = make_test_session();
        let mut state = make_test_state();
        state
            .items
            .push(TranscriptItem::User("How are you?".to_string()));
        state.items.push(TranscriptItem::Assistant {
            thinking: "Let me check".to_string(),
            text: "I am functioning normally.".to_string(),
        });
        state.items.push(TranscriptItem::ToolCall {
            id: "tc_1".to_string(),
            name: "list_dir".to_string(),
            status: "completed".to_string(),
            is_error: false,
            args_summary: String::new(),
            output_preview: Vec::new(),
            diff: None,
            duration_ms: None,
        });
        state
            .items
            .push(TranscriptItem::System("System notice: ready".to_string()));

        let editor = Editor::new().with_prefix("> ");

        let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
        assert!(!lines.is_empty());

        // Verify all rendered lines fit width
        for line in &lines {
            let clean = mehh_tui::strip_ansi(line);
            let w = mehh_tui::visible_width(&clean);
            assert!(w <= 80, "Line exceeds width 80: '{clean}' (len {w})");
        }

        // Verify frame contains header, transcript items, and footer
        let combined = lines.join("\n");
        assert!(combined.contains("mehh"));
        assert!(combined.contains("Test Model"));
        assert!(combined.contains("How are you?"));
        assert!(combined.contains("I am functioning normally."));
        assert!(combined.contains("list_dir"));
        assert!(combined.contains("System notice"));
        assert!(combined.contains("/help"));

        // Test with MockTerminal and TuiRenderer
        let mut term = mehh_tui::MockTerminal::new(80, 24);
        let mut renderer = TuiRenderer::new();
        renderer.render_frame(&mut term, lines, true).unwrap();
        let out = term.full_output();
        assert!(out.contains("mehh"));
        assert!(out.contains("Test Model"));

        // Incremental update test
        state
            .items
            .push(TranscriptItem::User("Next question".to_string()));
        let lines2 = render_interactive_frame(&state, &editor, &session, 80, 24);
        renderer.render_frame(&mut term, lines2, true).unwrap();
        let out2 = term.full_output();
        assert!(out2.contains("Next question"));
    }

    #[test]
    fn test_tool_approval_shows_full_reason_and_preview() {
        let session = make_test_session();
        let mut state = make_test_state();
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        let preview = "cargo test --all\ncargo clippy --all-targets\nrm -rf target";
        state.pending_approval = Some(PendingApproval {
            tool_name: "bash".to_string(),
            args: serde_json::Map::new(),
            reason: "Run the checks before deleting build output".to_string(),
            preview: Some(preview.to_string()),
            reply_tx,
        });
        state.selector = Some(Selector {
            kind: SelectorKind::ToolApproval,
            list: SelectList::new(
                vec![SelectItem::new("allow_once", "Allow once (y/Enter)")],
                6,
            ),
            settings_list: None,
        });

        let editor = Editor::new();
        let plain = mehh_tui::strip_ansi(
            &render_interactive_frame(&state, &editor, &session, 100, 30).join("\n"),
        );
        assert!(plain.contains("Run the checks before deleting build output"));
        for line in preview.lines() {
            assert!(plain.contains(line), "full command line was truncated: {line}");
        }
        assert!(plain.contains("Allow once"));
    }

    #[test]
    fn test_selector_filter_narrows_items_and_backspace_restores() {
        let mut sel = Selector {
            kind: SelectorKind::Model,
            list: SelectList::new(
                vec![
                    SelectItem::new("alpha", "Alpha"),
                    SelectItem::new("beta", "Beta"),
                ],
                8,
            ),
            settings_list: None,
        };
        for ch in ["b", "e", "t"] {
            assert!(update_selector_filter(&mut sel, ch));
        }
        assert_eq!(sel.list.filter, "bet");
        assert_eq!(sel.list.selected_item().map(|i| i.id.as_str()), Some("beta"));

        assert!(update_selector_filter(&mut sel, "backspace"));
        assert_eq!(sel.list.filter, "be");
        assert!(!update_selector_filter(&mut sel, "enter"));
    }

    #[test]
    fn test_slash_autocomplete_shows_full_command_and_replaces_text() {
        let mut editor = Editor::new();
        editor.set_text("/mc");
        update_autocomplete_with_cwd(&mut editor, "/mc", std::path::Path::new("."));

        let ac = editor.autocomplete.as_ref().expect("autocomplete popup");
        assert_eq!(ac.items[0].label, "/mcp", "popup must show the command, not its suffix");
        assert_eq!(ac.items[0].id, "/mcp");
        assert_eq!(editor.autocomplete_replace_start, Some(0));

        let kb = Keybindings::default_bindings();
        let _ = editor.handle_editor_key("enter", &kb);
        assert_eq!(editor.text(), "/mcp");
    }

    #[test]
    fn test_refresh_settings_selector_preserves_cursor() {
        let settings = Settings::default();
        let mut state = make_test_state();
        state.selector = Some(Selector {
            kind: SelectorKind::Settings,
            list: SelectList::new(settings_selector_items(&settings), 8),
            settings_list: Some(build_settings_list(&settings)),
        });
        state.selector.as_mut().unwrap().list.set_filter("the");
        state.selector.as_mut().unwrap().list.selected = 3;

        refresh_settings_selector(&mut state, &settings);

        let sel = state.selector.as_ref().unwrap();
        assert_eq!(sel.list.selected, 3, "list cursor must stay on the toggled row");
        assert_eq!(sel.list.filter, "the", "the active filter must survive a toggle");
        assert_eq!(
            sel.settings_list.as_ref().unwrap().selected,
            3,
            "detail pane must follow the same row"
        );
        assert_eq!(
            sel.settings_list.as_ref().unwrap().filter, "the",
            "detail pane must keep the same filter"
        );
    }

    #[test]
    fn test_render_interactive_frame_hides_empty_queue_counters() {
        let session = make_test_session();
        let state = make_test_state();
        let editor = Editor::new();
        let joined = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        assert!(!joined.contains("steering: 0"));
        assert!(!joined.contains("follow-up: 0"));
    }

    #[test]
    fn test_render_interactive_frame_tool_call_hides_internal_id() {
        let session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::ToolCall {
            id: "tc_secret_id".to_string(),
            name: "read".to_string(),
            status: "completed".to_string(),
            is_error: false,
            args_summary: String::new(),
            output_preview: Vec::new(),
            diff: None,
            duration_ms: None,
        });
        let editor = Editor::new();
        let plain = mehh_tui::strip_ansi(
            &render_interactive_frame(&state, &editor, &session, 80, 24).join("\n"),
        );
        assert!(plain.contains("read"));
        assert!(!plain.contains("tc_secret_id"));
        assert!(
            !plain.contains("completed"),
            "completed status is redundant noise"
        );
    }

    #[test]
    fn test_render_interactive_frame_anchors_dock_and_footer_to_bottom() {
        let session = make_test_session();
        let mut state = make_test_state();
        for i in 0..3 {
            state
                .items
                .push(TranscriptItem::User(format!("question {i}")));
        }
        let editor = Editor::new().with_placeholder("Type a message...");
        let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
        assert_eq!(lines.len(), 24, "frame must fill the terminal exactly");
        assert!(
            lines[23].contains("/help"),
            "footer must sit on the last row"
        );
        assert!(
            lines.iter().any(|l| l.contains("Type a message...")),
            "editor must stay visible even with a short transcript"
        );
    }

    #[test]
    fn test_render_interactive_frame_uses_theme_tokens() {
        let session = make_test_session();
        let mut state = make_test_state();

        let mut theme = Theme::dark();
        theme.accent = "\x1b[38;2;1;2;3m".to_string();
        theme.user_prefix = "\x1b[38;2;4;5;6m".to_string();
        theme.text = "\x1b[38;2;7;8;9m".to_string();
        theme.dim = "\x1b[38;2;10;11;12m".to_string();
        theme.thinking_text = "\x1b[38;2;13;14;15m".to_string();
        theme.tool_success = "\x1b[38;2;16;17;18m".to_string();
        state.theme = theme;

        state
            .items
            .push(TranscriptItem::User("Hello there".to_string()));
        state.items.push(TranscriptItem::Assistant {
            thinking: "pondering".to_string(),
            text: "Hi".to_string(),
        });
        state.items.push(TranscriptItem::ToolCall {
            id: "t1".to_string(),
            name: "read".to_string(),
            status: "completed".to_string(),
            is_error: false,
            args_summary: String::new(),
            output_preview: Vec::new(),
            diff: None,
            duration_ms: None,
        });
        state.items.push(TranscriptItem::System("note".to_string()));

        let editor = Editor::new().with_prefix("> ");
        let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
        let joined = lines.join("\n");

        assert!(joined.contains("\x1b[38;2;1;2;3m"), "accent token missing");
        assert!(
            joined.contains("\x1b[38;2;4;5;6m"),
            "user prefix token missing"
        );
        assert!(joined.contains("\x1b[38;2;7;8;9m"), "text token missing");
        assert!(joined.contains("\x1b[38;2;10;11;12m"), "dim token missing");
        assert!(
            joined.contains("\x1b[38;2;13;14;15m"),
            "thinking token missing"
        );
        assert!(
            joined.contains("\x1b[38;2;16;17;18m"),
            "tool success token missing"
        );

        // No hardcoded ANSI from the previous implementation remains.
        assert!(
            !joined.contains("\x1b[1;36m"),
            "hardcoded header accent remains"
        );
        assert!(
            !joined.contains("\x1b[1;32m"),
            "hardcoded user prefix remains"
        );
        assert!(
            !joined.contains("\x1b[2;3m"),
            "hardcoded thinking style remains"
        );
    }

    #[test]
    fn test_render_interactive_frame_uses_markdown_theme() {
        let session = make_test_session();
        let mut state = make_test_state();

        let mut theme = Theme::dark();
        theme.md_heading = "\x1b[38;2;200;100;50m".to_string();
        state.theme = theme;

        state.items.push(TranscriptItem::Assistant {
            thinking: String::new(),
            text: "# Custom Heading".to_string(),
        });

        let editor = Editor::new();
        let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
        let joined = lines.join("\n");
        assert!(
            joined.contains("\x1b[38;2;200;100;50m"),
            "markdown heading token missing"
        );
    }

    #[test]
    fn test_render_interactive_frame_respects_hide_thinking_block() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::Assistant {
            thinking: "secret reasoning".to_string(),
            text: "visible answer".to_string(),
        });

        let editor = Editor::new();
        let visible = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        assert!(visible.contains("Thought for") || visible.contains("Thinking"));
        assert!(visible.contains("visible answer"));

        session.settings.hide_thinking_block = Some(true);
        let hidden = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        assert!(!hidden.contains("Thought for") && !hidden.contains("Thinking"));
        assert!(hidden.contains("visible answer"));
    }

    #[tokio::test]
    async fn test_theme_slash_command_sets_and_persists_theme() {
        let dir = temp_dir("mehh_theme_cmd");
        let settings_file = dir.join("settings.json");

        let mut session = make_test_session();
        let mut state = make_test_state();
        state.settings_file = Some(settings_file.clone());

        assert_eq!(
            handle_slash_command("/theme light", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );

        assert_eq!(state.theme.name, "light");
        assert_eq!(session.settings.theme.as_deref(), Some("light"));

        let persisted = SettingsManager::load_file(&settings_file).unwrap();
        assert_eq!(persisted.theme.as_deref(), Some("light"));

        // Without an argument the current theme is reported.
        assert_eq!(
            handle_slash_command("/theme", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("light"))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A minimal terminal emulator: enough to check where a rendered frame lands.
    struct Screen {
        rows: usize,
        cols: usize,
        cells: Vec<Vec<char>>,
        row: usize,
        col: usize,
    }

    impl Screen {
        fn new(rows: usize, cols: usize) -> Self {
            Self {
                rows,
                cols,
                cells: vec![vec![' '; cols]; rows],
                row: 0,
                col: 0,
            }
        }

        fn feed(&mut self, data: &str) {
            let chars: Vec<char> = data.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '\x1b' if chars.get(i + 1) == Some(&'[') => {
                        let mut j = i + 2;
                        let mut params = String::new();
                        while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                            params.push(chars[j]);
                            j += 1;
                        }
                        let final_byte = chars.get(j).copied().unwrap_or('m');
                        self.apply_csi(&params, final_byte);
                        i = j + 1;
                    }
                    '\x1b' => {
                        let mut j = i + 2;
                        while j < chars.len() && chars[j] != '\u{7}' && chars[j] != '\x1b' {
                            j += 1;
                        }
                        i = j + 1;
                    }
                    '\r' => {
                        self.col = 0;
                        i += 1;
                    }
                    '\n' => {
                        self.row = (self.row + 1).min(self.rows - 1);
                        i += 1;
                    }
                    c => {
                        if self.row < self.rows && self.col < self.cols {
                            self.cells[self.row][self.col] = c;
                        }
                        self.col += 1;
                        i += 1;
                    }
                }
            }
        }

        fn apply_csi(&mut self, params: &str, final_byte: char) {
            let numbers: Vec<usize> = params.split(';').filter_map(|p| p.parse().ok()).collect();
            let first: usize = numbers.first().copied().unwrap_or(1);
            match final_byte {
                'A' => self.row = self.row.saturating_sub(first),
                'B' => self.row = (self.row + first).min(self.rows - 1),
                'C' => self.col = (self.col + first).min(self.cols - 1),
                'D' => self.col = self.col.saturating_sub(first),
                'H' | 'f' => {
                    let row = numbers.first().copied().unwrap_or(1);
                    let col = numbers.get(1).copied().unwrap_or(1);
                    self.row = row.saturating_sub(1).min(self.rows - 1);
                    self.col = col.saturating_sub(1).min(self.cols - 1);
                }
                'K' => {
                    let row = self.row;
                    if first == 2 {
                        for c in 0..self.cols {
                            self.cells[row][c] = ' ';
                        }
                    } else {
                        for c in self.col..self.cols {
                            self.cells[row][c] = ' ';
                        }
                    }
                }
                _ => {}
            }
        }

        fn line(&self, row: usize) -> String {
            self.cells[row]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        }
    }

    #[test]
    fn scroll_anchor_persists_across_frames() {
        let session = make_test_session();
        let mut state = make_test_state();
        for i in 0..20 {
            let text = if i == 18 {
                "ANCHOR_MARKER".to_string()
            } else {
                format!("line {i}")
            };
            state.items.push(TranscriptItem::System(text));
        }
        let editor = Editor::new();
        let _ = render_interactive_frame(&state, &editor, &session, 60, 15);
        state.scroll_offset = 1;
        let row_of = |frame: &[String]| -> Option<usize> {
            frame.iter().position(|l| l.contains("ANCHOR_MARKER"))
        };
        let a = render_interactive_frame(&state, &editor, &session, 60, 15);
        let r1 = row_of(&a);
        assert!(r1.is_some(), "marker must be visible initially: {a:?}");
        state.items.push(TranscriptItem::System("extra 1".into()));
        state.items.push(TranscriptItem::System("extra 2".into()));
        let b = render_interactive_frame(&state, &editor, &session, 60, 15);
        assert_eq!(r1, row_of(&b), "marker moved after a streaming burst");
        let c = render_interactive_frame(&state, &editor, &session, 60, 15);
        assert_eq!(r1, row_of(&c), "marker moved on the next tick");
        let pill = c.iter().find(|l| l.contains("Paused at")).unwrap();
        assert!(pill.contains("+4 new lines"), "cumulative count lost: {pill}");
    }

    #[test]
    fn stacked_panels_fit_60x20() {
        let session = make_test_session();
        let mut state = make_test_state();
        for i in 0..20 {
            state.items.push(TranscriptItem::System(format!("line {i}")));
        }
        state.is_busy = true;
        state.scroll_offset = 2;
        session.steer("queued steering message");
        let mut editor = Editor::new().with_placeholder("Type a message...");
        editor.set_text(&"long ".repeat(100));
        editor.set_autocomplete(
            (0..5)
                .map(|i| SelectItem::new(format!("f{i}"), format!("file {i}")))
                .collect(),
        );
        let frame = render_interactive_frame(&state, &editor, &session, 60, 20);
        assert!(frame.len() <= 20, "frame overflowed 20 rows: {}", frame.len());
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("/help"), "footer pushed off-screen: {plain}");
    }

    #[test]
    fn autocomplete_fits_60x20_when_empty() {
        let session = make_test_session();
        let state = make_test_state();
        let mut editor = Editor::new().with_placeholder("Type a message...");
        editor.set_text(&"long ".repeat(100));
        editor.set_autocomplete(
            (0..5)
                .map(|i| SelectItem::new(format!("f{i}"), format!("file {i}")))
                .collect(),
        );
        let frame = render_interactive_frame(&state, &editor, &session, 60, 20);
        assert!(frame.len() <= 20, "frame overflowed 20 rows: {}", frame.len());
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("/help"), "footer pushed off-screen: {plain}");
        assert!(plain.contains("Suggestions"), "{plain}");
    }

    #[test]
    fn completed_thinking_keeps_elapsed() {
        let session = make_test_session();
        let mut state = make_test_state();
        let now = std::time::Instant::now();
        *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(now - std::time::Duration::from_millis(4200));
        *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
        state.items.push(TranscriptItem::Assistant {
            thinking: "reasoning".to_string(),
            text: String::new(),
        });
        let frame = render_interactive_frame(&state, &Editor::new(), &session, 80, 24);
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("4.2s"), "elapsed lost: {plain}");
    }

    #[test]
    fn historical_thinking_keeps_its_own_elapsed() {
        let session = make_test_session();
        let mut state = make_test_state();
        for _ in 0..2 {
            state.items.push(TranscriptItem::Assistant {
                thinking: "reasoning".to_string(),
                text: String::new(),
            });
        }
        state
            .thinking_elapsed
            .lock()
            .unwrap()
            .insert(0, 2000);
        let frame = render_interactive_frame(&state, &Editor::new(), &session, 80, 24);
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert_eq!(plain.matches("2.0s").count(), 1, "{plain}");
    }

    #[test]
    fn only_active_thinking_shows_spinner() {
        let session = make_test_session();
        let mut state = make_test_state();
        for _ in 0..2 {
            state.items.push(TranscriptItem::Assistant {
                thinking: "alpha\nbeta\ngamma\ndelta".to_string(),
                text: String::new(),
            });
        }
        state.is_busy = true;
        let frame = render_interactive_frame(&state, &Editor::new(), &session, 80, 24);
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert_eq!(plain.matches("Thinking (").count(), 1, "{plain}");
        assert_eq!(plain.matches("Thought for").count(), 1, "{plain}");
    }

    #[test]
    fn transcript_cache_includes_render_time_flags() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::Assistant {
            thinking: "alpha\nbeta\ngamma\ndelta".to_string(),
            text: String::new(),
        });
        let editor = Editor::new();
        let collapsed = render_interactive_frame(&state, &editor, &session, 80, 24);
        state.thinking_expanded = true;
        let expanded = render_interactive_frame(&state, &editor, &session, 80, 24);
        assert_ne!(collapsed, expanded, "Ctrl+T must change the rendered frame");
        state.thinking_expanded = false;
        session.settings.hide_thinking_block = Some(true);
        let hidden = render_interactive_frame(&state, &editor, &session, 80, 24);
        assert_ne!(expanded, hidden, "hide_thinking must change the rendered frame");
        session.settings.hide_thinking_block = Some(false);
        state.is_busy = true;
        let busy = render_interactive_frame(&state, &editor, &session, 80, 24);
        assert_ne!(hidden, busy, "busy flag must change the rendered frame");
    }

    #[test]
    fn tool_approval_fits_60x20_with_long_preview() {
        let session = make_test_session();
        let mut state = make_test_state();
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        let preview = (0..300)
            .map(|i| format!("cargo test case {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.pending_approval = Some(PendingApproval {
            tool_name: "bash".to_string(),
            args: serde_json::Map::new(),
            reason: "Verify before approving this very long command preview".to_string(),
            preview: Some(preview),
            reply_tx,
        });
        state.selector = Some(Selector {
            kind: SelectorKind::ToolApproval,
            list: SelectList::new(
                vec![
                    SelectItem::new("allow_once", "Allow once (y/Enter)"),
                    SelectItem::new("allow_session", "Allow for session (a)"),
                    SelectItem::new("always_allow", "Always allow (A)"),
                    SelectItem::new("deny", "Deny (n/Esc)"),
                ],
                6,
            ),
            settings_list: None,
        });
        let editor = Editor::new();
        let frame = render_interactive_frame(&state, &editor, &session, 60, 20);
        assert!(frame.len() <= 20, "frame overflowed 20 rows: {}", frame.len());
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("Allow once"), "{plain}");
        assert!(plain.contains("Allow for session"), "{plain}");
        assert!(plain.contains("Always allow"), "{plain}");
        assert!(plain.contains("Deny"), "{plain}");
        assert!(plain.contains("/help"), "footer pushed off-screen: {plain}");
    }

    #[test]
    fn tool_duration_label_formats_ms() {
        assert_eq!(super::tool_duration_label(None), "");
        assert_eq!(super::tool_duration_label(Some(18)), " 18ms");
    }

    #[test]
    fn diff_truncation_reports_hidden_lines() {
        let diff = (0..30)
            .map(|i| format!("+line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (shown, hidden) = super::diff_visible_lines(&diff);
        assert_eq!(shown.len(), 24);
        assert_eq!(hidden, 6);
    }

    #[test]
    fn thinking_preview_shows_last_three_lines_when_busy() {
        let text = (1..=8)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = super::thinking_preview(&text, false, true, None);
        assert!(s.contains("line 8") && s.contains("line 6"));
        assert!(!s.contains("line 1"));
        assert!(s.contains("Thinking (8 lines)"));
    }

    #[test]
    fn context_formatting_helpers() {
        assert_eq!(super::compact_count(12_432), "12.4k");
        assert_eq!(super::compact_count(999), "999");
        assert!(
            super::ctx_bar(72, &mehh_tui::Theme::dark())
                .matches('█')
                .count()
                >= 5
        );
    }

    #[test]
    fn left_right_layout_aligns_right_edge() {
        let line = super::left_right("left".into(), "right".into(), 40);
        assert_eq!(mehh_tui::visible_width(&line), 40);
        assert!(line.ends_with("right"));
    }

    #[test]
    fn compact_path_keeps_final_folder() {
        let p = std::path::Path::new("C:/Users/Admin/Projects/mehh-code");
        let s = super::compact_path(p, 24);
        assert!(s.contains("mehh-code"), "{s}");
        assert!(s.contains("..."), "{s}");
    }

    #[test]
    fn compact_path_counts_characters_not_bytes() {
        let p = std::path::Path::new("C:/Users/Admin/项目/меhh-код");
        for max_len in [12, 18, 24] {
            let s = super::compact_path(p, max_len);
            assert!(s.chars().count() <= max_len, "{s} is longer than {max_len}");
        }
    }

    #[test]
    fn elapsed_label_formats_seconds() {
        let now = std::time::Instant::now();
        let s = super::elapsed_label(
            Some(now - std::time::Duration::from_millis(3400)),
            now,
        );
        assert!(s.contains("3.4s"), "{s}");
    }

    #[test]
    fn user_turn_paints_every_wrapped_line_full_width() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let lines = super::render_user_turn(text, 20, &mehh_tui::Theme::dark());
        assert!(lines.len() > 1, "must wrap: {lines:?}");
        assert!(
            lines
                .iter()
                .all(|l| mehh_tui::visible_width(l) == 20),
            "{lines:?}"
        );
        assert!(lines
            .iter()
            .all(|l| l.contains("\x1b[48;2;35;38;46m")));
    }

    #[test]
    fn status_icon_uses_monospace_glyphs() {
        let t = mehh_tui::Theme::dark();
        assert!(super::status_icon("running", false, 0, &t).contains('⠋'));
        assert!(super::status_icon("completed", false, 0, &t).contains('✓'));
        assert!(super::status_icon("failed", true, 0, &t).contains('✗'));
        assert!(super::status_icon("blocked", false, 0, &t).contains('◌'));
    }

    #[test]
    fn filter_accepts_space_except_for_settings() {
        let mut model = Selector {
            kind: SelectorKind::Model,
            list: mehh_tui::SelectList::new(vec![], 5),
            settings_list: None,
        };
        assert!(super::update_selector_filter(&mut model, " "));
        assert_eq!(model.list.filter, " ");
        let mut settings = Selector {
            kind: SelectorKind::Settings,
            list: mehh_tui::SelectList::new(vec![], 5),
            settings_list: None,
        };
        assert!(!super::update_selector_filter(&mut settings, " "));
    }

    #[test]
    fn queue_mode_values_match_enum() {
        let settings = crate::settings::Settings::default();
        let list = super::build_settings_list(&settings);
        let item = list.items.iter().find(|i| i.id == "steering_mode").unwrap();
        assert_eq!(
            item.values,
            vec!["one-at-a-time".to_string(), "all".to_string()]
        );
    }

    #[test]
    fn cap_preview_lines_preserves_action_rows() {
        let preview = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let capped = super::cap_preview_lines(&preview, 6);
        assert_eq!(capped.lines().count(), 7);
        assert!(capped.contains("+294 more lines"), "{capped}");
    }

    #[test]
    fn anchored_scroll_offset_accumulates_across_frames() {
        assert_eq!(super::anchored_scroll_offset(0, 12, 5), 12);
        assert_eq!(super::anchored_scroll_offset(12, 0, 5), 12);
        assert_eq!(super::anchored_scroll_offset(12, 3, 0), 0);
    }

    #[test]
    fn scrollbar_char_marks_thumb_and_track() {
        assert_eq!(super::scrollbar_char(1, 10, 100, 0), '┃');
        assert_eq!(super::scrollbar_char(5, 10, 100, 0), '│');
    }

    #[test]
    fn scroll_pill_reports_pct_and_new_lines() {
        assert!(super::scroll_pill_text(32, 100, 12).contains("+12 new lines"));
        assert!(super::scroll_pill_text(32, 100, 12).contains("32%"));
    }

    #[test]
    fn context_token_cache_invalidates_on_message_end() {
        let state = make_test_state();
        *state.cached_ctx_tokens.lock().unwrap() = Some(4242);
        state.invalidate_context_tokens();
        assert!(state.cached_ctx_tokens.lock().unwrap().is_none());
    }

    #[test]
    fn render_interactive_frame_is_deterministic_at_60x20() {
        let session = make_test_session();
        let state = make_test_state();
        let editor = Editor::new().with_placeholder("Type a message...");
        let a = render_interactive_frame(&state, &editor, &session, 60, 20);
        let b = render_interactive_frame(&state, &editor, &session, 60, 20);
        assert_eq!(a, b);
    }

    #[test]
    fn render_now_keeps_renderer_state_and_does_not_home() {
        let session = make_test_session();
        let state = make_test_state();
        let editor = Editor::new().with_placeholder("Type a message...");
        let mut term = mehh_tui::MockTerminal::new(80, 24);
        let mut renderer = TuiRenderer::new();

        render_now(&state, &editor, &session, &mut term, &mut renderer);
        term.clear_output();
        render_now(&state, &editor, &session, &mut term, &mut renderer);
        let out = term.full_output();
        assert!(!out.contains("\x1b[H"), "no home: {out:?}");
        assert!(!out.contains("\x1b[J"), "no clear-below: {out:?}");
        assert!(!renderer.previous_lines.is_empty(), "renderer keeps baseline");
    }

    #[test]
    fn test_frame_updates_stay_aligned_on_screen() {
        let session = make_test_session();
        let mut state = make_test_state();
        state.items.push(TranscriptItem::User("hello".to_string()));

        let mut editor = Editor::new().with_placeholder("Type a message...");
        let mut term = mehh_tui::MockTerminal::new(80, 24);
        let mut renderer = TuiRenderer::new();
        let mut screen = Screen::new(24, 80);

        fn row_of(screen: &Screen, needle: &str) -> Option<usize> {
            (0..24).find(|&i| screen.line(i).contains(needle))
        }

        render_now(&state, &editor, &session, &mut term, &mut renderer);
        screen.feed(&term.full_output());
        assert_eq!(row_of(&screen, "mehh"), Some(0));
        assert_eq!(row_of(&screen, "hello"), Some(2));
        let editor_row = row_of(&screen, "Type a message").expect("editor placeholder must render");
        let footer_row = row_of(&screen, "/help").expect("footer must render");
        // The prompt dock and footer are pinned to the bottom, not floating under the header.
        assert!(editor_row > 2 && footer_row > editor_row);
        assert!(footer_row >= 21, "footer row was {footer_row}");

        // Typing must rewrite the input line in place without moving the footer.
        term.clear_output();
        editor.set_text("/pro");
        render_now(&state, &editor, &session, &mut term, &mut renderer);
        screen.feed(&term.full_output());
        assert_eq!(row_of(&screen, "mehh"), Some(0));
        assert_eq!(row_of(&screen, "hello"), Some(2));
        assert_eq!(row_of(&screen, "/pro"), Some(editor_row));
        assert_eq!(row_of(&screen, "/help"), Some(footer_row));
    }

    #[tokio::test]
    async fn test_refresh_session_api_key_ignores_another_provider() {
        let dir = temp_dir("mehh_login_other_provider");
        let auth_file = dir.join("auth.json");

        let mut session = make_test_session();
        session.set_api_key(Some("original-key".to_string()));

        crate::auth::save_credential(
            "anthropic",
            &StoredCredential::ApiKey {
                key: Some("sk-other-provider".to_string()),
                env: None,
            },
            Some(&auth_file),
        )
        .unwrap();

        refresh_session_api_key(&mut session, "anthropic", Some(&auth_file))
            .await
            .unwrap();

        assert_eq!(
            session.api_key.as_deref(),
            Some("original-key"),
            "another provider's key must not replace the session key"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_sync_mcp_tools_keeps_the_custom_prompt_and_addendum() {
        let mut session = make_test_session();
        session.set_system_prompt(crate::system_prompt::build_system_prompt_with_skills(
            &session.cwd,
            Some("CUSTOM PREAMBLE"),
            Some("EXTRA RULES"),
            &["read", "write"],
            session.skills(),
        ));

        sync_mcp_tools(&mut session, Vec::new(), false);

        assert!(
            session.system_prompt.starts_with("CUSTOM PREAMBLE"),
            "the custom prompt must survive a tool sync: {}",
            session.system_prompt
        );
        assert!(
            session
                .system_prompt
                .contains("<addendum>\nEXTRA RULES\n</addendum>"),
            "the addendum must survive a tool sync: {}",
            session.system_prompt
        );
    }

    #[test]
    fn test_interactive_sync_preserves_codemode_handle() {
        let mut session = make_test_session();
        session.enable_mcp_and_codemode(Vec::new(), true);
        let before = session.codemode_tool().expect("codemode enabled");
        sync_mcp_tools(&mut session, Vec::new(), true);
        let after = session.codemode_tool().expect("codemode still enabled");
        assert!(
            Arc::ptr_eq(&before, &after),
            "interactive sync must reuse the codemode handle to keep its script store"
        );
    }

    #[test]
    fn test_split_system_prompt_recovers_the_preamble_and_addendum() {
        let prompt = crate::system_prompt::build_system_prompt_with_skills(
            std::path::Path::new("."),
            Some("PREAMBLE"),
            Some("APPENDUM"),
            &["read"],
            &[],
        );
        let (preamble, addendum) = split_system_prompt(&prompt);
        assert_eq!(preamble.as_deref(), Some("PREAMBLE"));
        assert_eq!(addendum.as_deref(), Some("APPENDUM"));
    }

    #[test]
    fn test_expand_prompt_with_files_reads_at_file() {
        let dir = temp_dir("mehh_tui_file_arg");
        std::fs::write(dir.join("notes.txt"), "notes body").unwrap();
        let (text, blocks) = expand_prompt_with_files(&dir, "summarize @notes.txt").unwrap();
        assert!(text.contains("<file name=\"notes.txt\">"));
        assert!(text.contains("notes body"));
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, UserContentBlock::Text(t) if t.text.contains("notes body"))),
            "the expanded file must reach the user content blocks"
        );
    }

    #[test]
    fn test_expand_prompt_with_files_supports_quoted_paths_and_line_breaks() {
        let dir = temp_dir("mehh_tui_quoted_file_arg");
        std::fs::write(dir.join("notes with spaces.txt"), "spaced body").unwrap();
        std::fs::write(dir.join("short.txt"), "short body").unwrap();

        let (text, blocks) =
            expand_prompt_with_files(&dir, "summarize @\"notes with spaces.txt\"").unwrap();
        assert!(
            text.contains("<file name=\"notes with spaces.txt\">"),
            "quoted paths with spaces must expand as one argument"
        );
        assert!(text.contains("spaced body"));
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, UserContentBlock::Text(t) if t.text.contains("spaced body"))),
            "the quoted file must reach the user content blocks"
        );

        let (text, _) = expand_prompt_with_files(&dir, "first line\nsecond @short.txt").unwrap();
        assert!(text.contains('\n'), "line breaks must survive expansion");
    }

    #[tokio::test]
    async fn test_follow_up_slash_keeps_full_text() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        handle_slash_command(
            "/follow-up check tests",
            &mut session,
            &mut state,
            None,
            false,
        )
        .await;
        assert_eq!(session.pending_follow_up_count(), 1);
        assert!(state.items.iter().any(|item| matches!(
            item,
            TranscriptItem::System(text) if text.contains("Queued follow-up: check tests")
        )));
    }

    #[tokio::test]
    async fn test_tree_slash_accepts_summarize_flag_from_rest() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        let first = session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("first".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
        session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("second".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
        handle_slash_command(
            &format!("/tree {first} --summarize"),
            &mut session,
            &mut state,
            None,
            false,
        )
        .await;
        assert!(state.items.iter().any(|item| matches!(
            item,
            TranscriptItem::System(text) if text.contains("with branch summary")
        )));
    }

    #[test]
    fn test_route_busy_input() {
        assert_eq!(
            route_busy_input("check the logs"),
            BusyInput::Steer("check the logs".to_string())
        );
        assert_eq!(
            route_busy_input("/follow-up now summarize"),
            BusyInput::FollowUp("now summarize".to_string())
        );
        // An empty follow-up falls back to steering.
        assert_eq!(
            route_busy_input("/follow-up   "),
            BusyInput::Steer("/follow-up".to_string())
        );
        // The command name must end at a boundary.
        assert_eq!(
            route_busy_input("/follow-upXYZ"),
            BusyInput::Steer("/follow-upXYZ".to_string())
        );
    }

    #[test]
    fn test_update_autocomplete_offers_full_commands() {
        let mut editor = Editor::new();
        editor.set_text("/t");
        update_autocomplete_with_cwd(&mut editor, "/t", Path::new("."));

        let items = editor.autocomplete.as_ref().expect("popup").items.clone();
        let theme = items
            .iter()
            .find(|i| i.id == "/theme")
            .expect("/theme offered");
        assert_eq!(
            theme.label, "/theme",
            "the popup must show the full command, not its suffix"
        );

        // Applying the completion replaces the typed prefix with the full command.
        editor.set_autocomplete_replacing(vec![theme.clone()], 0);
        let kb = Keybindings::default_bindings();
        let _ = editor.handle_editor_key("enter", &kb);
        assert_eq!(editor.text(), "/theme");
    }

    #[test]
    fn test_update_autocomplete_leaves_exact_commands_submittable() {
        let mut editor = Editor::new();
        editor.set_text("/theme");
        let typed = editor.text();
        update_autocomplete_with_cwd(&mut editor, &typed, Path::new("."));
        assert!(
            editor.autocomplete.is_none(),
            "an exact command must not keep the popup open"
        );

        let kb = Keybindings::default_bindings();
        let out = editor.handle_editor_key("enter", &kb);
        assert!(
            matches!(out, EditorOutput::Submit(s) if s == "/theme"),
            "Enter must submit the typed command"
        );
    }

    #[tokio::test]
    async fn test_fork_with_unknown_entry_id_keeps_the_current_session() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("first prompt".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
        let original_id = session.session().session_id().to_string();

        assert_eq!(
            handle_slash_command(
                "/fork not-a-real-entry-id",
                &mut session,
                &mut state,
                None,
                false
            )
            .await,
            SlashCommandOutcome::Handled
        );

        assert_eq!(
            session.session().session_id(),
            original_id,
            "an unknown entry id must not replace the session"
        );
        assert!(matches!(
            state.items.last(),
            Some(TranscriptItem::System(s)) if s.contains("Unknown entry id")
        ));
    }

    #[tokio::test]
    async fn test_theme_command_rejects_unknown_theme_name() {
        let dir = temp_dir("mehh_theme_unknown");
        let settings_file = dir.join("settings.json");

        let mut session = make_test_session();
        let mut state = make_test_state();
        state.settings_file = Some(settings_file.clone());

        assert_eq!(
            handle_slash_command(
                "/theme definitely-not-a-theme",
                &mut session,
                &mut state,
                None,
                false
            )
            .await,
            SlashCommandOutcome::Handled
        );

        assert!(
            session.settings.theme.is_none(),
            "an unknown theme must not be persisted"
        );
        assert!(matches!(
            state.items.last(),
            Some(TranscriptItem::System(s)) if s.contains("Unknown theme")
        ));
        assert!(
            !settings_file.exists(),
            "nothing must be written for an unknown theme"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_copy_toggle_writes_only_the_changed_key() {
        let mut global = Settings {
            default_model: Some("gpt-4o".to_string()),
            default_provider: Some("openai".to_string()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            theme: Some("dark".to_string()),
            ..Default::default()
        };
        let session_settings = Settings {
            theme: Some("light".to_string()),
            steering_mode: Some(QueueMode::All),
            default_model: Some("claude-sonnet-4-5".to_string()),
            ..Default::default()
        };

        copy_toggle(&mut global, &session_settings, "steering_mode");

        assert_eq!(global.steering_mode, Some(QueueMode::All));
        assert_eq!(
            global.theme.as_deref(),
            Some("dark"),
            "an unrelated toggle must not be promoted"
        );
        assert_eq!(
            global.default_model.as_deref(),
            Some("gpt-4o"),
            "the global model must survive a toggle"
        );
        assert_eq!(global.default_provider.as_deref(), Some("openai"));
        assert_eq!(global.max_tokens, Some(4096));
        assert_eq!(global.temperature, Some(0.5));
    }

    #[test]
    fn test_persist_settings_reports_a_malformed_file_instead_of_overwriting_it() {
        let dir = temp_dir("mehh_settings_malformed");
        let settings_file = dir.join("settings.json");
        std::fs::write(&settings_file, b"{not valid json").unwrap();

        let settings = Settings {
            theme: Some("light".to_string()),
            ..Default::default()
        };
        let err = persist_settings(&settings, "theme", Some(&settings_file)).unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse settings JSON"),
            "unexpected error: {err}"
        );

        // The broken file must be left untouched.
        assert_eq!(
            std::fs::read(&settings_file).unwrap(),
            b"{not valid json".to_vec()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_failed_agent_signal_ends_the_turn_and_reports() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        state.is_busy = true;

        let ended = apply_agent_signal(
            AgentSignal::Failed("the agent loop panicked".to_string()),
            &mut session,
            &mut state,
        );

        assert!(ended, "a failed turn must end the turn");
        assert!(!state.is_busy);
        assert!(matches!(
            state.items.last(),
            Some(TranscriptItem::System(s)) if s.contains("panicked")
        ));
    }

    #[test]
    fn test_drain_agent_signals_applies_a_queued_finished_turn() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
        let _keepalive = tx.clone();

        tx.send(AgentSignal::Finished(vec![Message::User(
            mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("queued".to_string()),
                timestamp: mehh_ai::now_ms(),
            },
        )]))
        .unwrap();

        let ended = drain_agent_signals(&mut rx, &mut session, &mut state);
        assert!(ended, "a queued finished turn must be reported");
        assert_eq!(session.session().entries().len(), 1);
    }

    #[test]
    fn test_render_interactive_frame_themes_the_loader() {
        let session = make_test_session();
        let mut state = make_test_state();
        state.is_busy = true;

        let mut theme = Theme::dark();
        theme.accent = "\x1b[38;2;31;32;33m".to_string();
        state.theme = theme;

        let editor = Editor::new();
        let joined = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
        assert!(
            joined.contains("\x1b[38;2;31;32;33m"),
            "the spinner must use the theme accent"
        );
    }

    #[test]
    fn test_render_interactive_frame_shows_editor_and_queue_counters_when_busy() {
        let session = make_test_session();
        session.steer("first steer");
        session.steer("second steer");
        session.follow_up("later");

        let mut state = make_test_state();
        state.is_busy = true;
        state.status = Some("Executing read...".to_string());

        let editor = Editor::new().with_placeholder("Type a message or /help...");
        let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
        let joined = lines.join("\n");

        // The editor stays visible while the agent is running.
        assert!(joined.contains("Type a message or /help..."));
        assert!(joined.contains("steering: 2"));
        assert!(joined.contains("follow-up: 1"));
        assert!(joined.contains("Executing read..."));
    }

    #[tokio::test]
    async fn test_slash_command_settings_opens_selector_and_cycles_values() {
        let dir = temp_dir("mehh_settings_cmd");
        let settings_file = dir.join("settings.json");

        let mut session = make_test_session();
        let mut state = make_test_state();
        state.settings_file = Some(settings_file.clone());

        assert_eq!(
            handle_slash_command("/settings", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        let selector = state.selector.as_ref().expect("settings selector");
        assert_eq!(selector.kind, SelectorKind::Settings);
        let ids: Vec<&str> = selector.list.items.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"theme"));
        assert!(ids.contains(&"steering_mode"));
        assert!(ids.contains(&"follow_up_mode"));
        assert!(ids.contains(&"default_thinking_level"));
        assert!(ids.contains(&"hide_thinking_block"));
        assert!(ids.contains(&"auto_compact"));

        // Cycling a setting updates the runtime settings and persists them.
        apply_setting_cycle("steering_mode", &mut session, &mut state).unwrap();
        assert_eq!(session.settings.steering_mode, Some(QueueMode::All));
        apply_setting_cycle("hide_thinking_block", &mut session, &mut state).unwrap();
        assert_eq!(session.settings.hide_thinking_block, Some(true));
        apply_setting_cycle("default_thinking_level", &mut session, &mut state).unwrap();
        assert_eq!(
            session.settings.default_thinking_level,
            Some(ThinkingLevel::Low)
        );
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::Low));

        persist_settings(
            &session.settings,
            "steering_mode",
            state.settings_file.as_deref(),
        )
        .unwrap();
        persist_settings(
            &session.settings,
            "hide_thinking_block",
            state.settings_file.as_deref(),
        )
        .unwrap();
        let persisted = SettingsManager::load_file(&settings_file).unwrap();
        assert_eq!(persisted.steering_mode, Some(QueueMode::All));
        assert_eq!(persisted.hide_thinking_block, Some(true));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_slash_command_tree_fork_and_clone() {
        let mut session = make_test_session();
        let mut state = make_test_state();

        let first = session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("first prompt".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
        let second = session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("second prompt".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();

        // /tree with an explicit entry switches the active branch.
        assert_eq!(
            handle_slash_command(
                &format!("/tree {first}"),
                &mut session,
                &mut state,
                None,
                false
            )
            .await,
            SlashCommandOutcome::Handled
        );
        assert_eq!(session.session().leaf_id(), Some(first.as_str()));
        assert_eq!(
            state.items[0],
            TranscriptItem::User("first prompt".to_string())
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("Switched to branch")),
            "expected a branch switch notice, got {:?}",
            state.items.last()
        );

        // /tree without an argument opens the branch selector.
        assert_eq!(
            handle_slash_command("/tree", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        let selector = state.selector.as_ref().expect("tree selector");
        assert_eq!(selector.kind, SelectorKind::Tree);
        assert!(selector.list.items.iter().any(|i| i.id == second));

        // /fork creates a new session and switches to it.
        let original_id = session.session().session_id().to_string();
        assert_eq!(
            handle_slash_command(
                &format!("/fork {first}"),
                &mut session,
                &mut state,
                None,
                false
            )
            .await,
            SlashCommandOutcome::Handled
        );
        assert_ne!(session.session().session_id(), original_id);

        // /clone copies the active path into a fresh session.
        let forked_id = session.session().session_id().to_string();
        assert_eq!(
            handle_slash_command("/clone", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert_ne!(session.session().session_id(), forked_id);
    }

    #[tokio::test]
    async fn test_slash_command_resume_opens_session_selector() {
        let mut session = make_test_session();
        let mut state = make_test_state();

        assert_eq!(
            handle_slash_command("/resume", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );

        // Either a selector opened or an explanatory message was produced.
        let opened = state
            .selector
            .as_ref()
            .map(|s| s.kind == SelectorKind::Resume)
            .unwrap_or(false);
        let reported = state
            .items
            .iter()
            .any(|i| matches!(i, TranscriptItem::System(s) if s.contains("session")));
        assert!(opened || reported);
    }

    #[tokio::test]
    async fn test_login_and_logout_with_api_key() {
        let dir = temp_dir("mehh_login_cmd");
        let auth_file = dir.join("auth.json");

        let mut session = make_test_session();
        let mut state = make_test_state();
        state.auth_file = Some(auth_file.clone());

        assert_eq!(
            handle_slash_command(
                "/login anthropic sk-test-12345",
                &mut session,
                &mut state,
                None,
                false
            )
            .await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("anthropic"))
        );

        let entries = crate::auth::load_auth_entries(Some(&auth_file)).unwrap();
        assert!(entries.contains_key("anthropic"));

        assert_eq!(
            handle_slash_command("/logout anthropic", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        let entries = crate::auth::load_auth_entries(Some(&auth_file)).unwrap();
        assert!(!entries.contains_key("anthropic"));
    }

    #[tokio::test]
    async fn test_perform_login_rejects_oauthless_provider() {
        let dir = temp_dir("mehh_login_reject");
        let auth_file = dir.join("auth.json");

        let err = perform_login("mistral", None, Some(&auth_file))
            .await
            .expect_err("mistral has no OAuth flow");
        assert!(err.to_string().contains("does not support OAuth"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_mcp_command_without_manager_reports_disabled() {
        let mut session = make_test_session();
        let mut state = make_test_state();

        assert_eq!(
            handle_slash_command("/mcp reconnect", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("not enabled"))
        );

        assert_eq!(
            handle_slash_command("/mcp resources", &mut session, &mut state, None, false).await,
            SlashCommandOutcome::Handled
        );
        assert!(
            matches!(state.items.last(), Some(TranscriptItem::System(s)) if s.contains("not enabled"))
        );
    }

    #[tokio::test]
    async fn test_started_turn_streams_events_and_persists_messages() {
        let mut session = make_test_session();
        let mut state = make_test_state();

        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
        let _keepalive = agent_tx.clone();

        start_turn(&session, &mut state, "hello there", Vec::new(), &agent_tx);
        assert!(state.is_busy);

        // The UI loop keeps receiving events while the turn runs in the background.
        let mut finished: Option<Vec<Message>> = None;
        while let Some(sig) = agent_rx.recv().await {
            match sig {
                AgentSignal::Event(ev) => apply_agent_event(&mut state, &ev),
                AgentSignal::Finished(messages) => {
                    finished = Some(messages);
                    break;
                }
                AgentSignal::Failed(message) => {
                    panic!("the agent turn failed unexpectedly: {message}");
                }
                _ => {}
            }
        }

        let messages = finished.expect("the turn finished");
        session.finish_prompt(&messages).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(session.session().entries().len(), 2);
        assert!(matches!(
            state.items.last(),
            Some(TranscriptItem::Assistant { text, .. }) if text.contains("faux default response")
        ));
    }

    #[test]
    fn test_rebuild_transcript_items_maps_messages() {
        let mut session = make_test_session();
        session
            .session_mut()
            .append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text("hello".to_string()),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
        session
            .session_mut()
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "hi there",
            )))
            .unwrap();

        let items = rebuild_transcript_items(&session);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], TranscriptItem::User("hello".to_string()));
        assert_eq!(
            items[1],
            TranscriptItem::Assistant {
                thinking: String::new(),
                text: "hi there".to_string(),
            }
        );
    }

    #[test]
    fn test_sync_mcp_tools_preserves_multi_paragraph_custom_prompt() {
        let mut session = make_test_session();
        session.set_custom_system_prompt(
            Some("First paragraph.\n\nSecond paragraph.".to_string()),
            Some("Append line.".to_string()),
        );
        sync_mcp_tools(&mut session, Vec::new(), true);
        assert!(session
            .system_prompt
            .contains("First paragraph.\n\nSecond paragraph."));
        assert!(session.system_prompt.contains("Append line."));
        assert!(session.system_prompt.contains("codemode"));
    }

    #[test]
    fn test_update_autocomplete_with_cwd_completes_and_replaces_at_file() {
        let dir = std::env::temp_dir().join(format!("mehh_ic_at_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("alpha.rs"), "fn a() {}").unwrap();

        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();
        for ch in "inspect @src/al".chars() {
            let _ = editor.handle_editor_key(&ch.to_string(), &kb);
        }
        let text = editor.text();
        update_autocomplete_with_cwd(&mut editor, &text, &dir);

        let popup = editor.autocomplete.as_ref().expect("@file popup");
        assert!(popup.items.iter().any(|it| it.id == "@src/alpha.rs"));

        let _ = editor.handle_editor_key("tab", &kb);
        assert_eq!(editor.text(), "inspect @src/alpha.rs");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_interactive_surfaces_errors_and_retries_context_overflow() {
        let dir = crate::TempTestDir::new("ic_overflow");
        let mut sm = SessionManager::in_memory(dir.path());
        for i in 0..4 {
            sm.append_message(Message::User(mehh_ai::UserMessage {
                content: mehh_ai::UserContent::Text(format!(
                    "prior prompt {i}: {}",
                    "data ".repeat(800)
                )),
                timestamp: mehh_ai::now_ms(),
            }))
            .unwrap();
            sm.append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                format!("prior reply {i}: {}", "reply ".repeat(800)),
            )))
            .unwrap();
        }

        let faux = Arc::new(mehh_ai::FauxProvider::with_responses(vec![
            mehh_ai::AssistantMessage {
                content: Vec::new(),
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("prompt is too long: exceeded context length".to_string()),
                timestamp: mehh_ai::now_ms(),
            },
            mehh_ai::faux_assistant_message("## Goal\nCompacted summary."),
            mehh_ai::faux_assistant_message("Recovered in interactive!"),
        ]));

        let mut model = mehh_ai::faux_model();
        model.context_window = 200_000;
        let mut session = AgentSession::new_with_trust(
            dir.path().to_path_buf(),
            model,
            faux,
            sm,
            Settings::default(),
            false,
        );
        let mut state = make_test_state();

        // Direct MessageEnd error event surfaces as TranscriptItem::System
        let err_ast = mehh_ai::AssistantMessage {
            content: Vec::new(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("invalid api key".to_string()),
            timestamp: mehh_ai::now_ms(),
        };
        apply_agent_event(
            &mut state,
            &AgentEvent::MessageEnd {
                message: Message::Assistant(err_ast),
            },
        );
        assert!(matches!(
            state.items.last(),
            Some(TranscriptItem::System(s)) if s.contains("invalid api key")
        ));

        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
        let _keepalive = agent_tx.clone();
        state.last_prompt_text = Some("continue".to_string());
        state.overflow_retried = false;
        start_turn(&session, &mut state, "continue", Vec::new(), &agent_tx);

        while let Some(sig) = agent_rx.recv().await {
            if matches!(
                handle_agent_signal_async(sig, &mut session, &mut state, &agent_tx).await,
                SignalOutcome::TurnEnded
            ) {
                break;
            }
        }

        assert!(session
            .session()
            .entries()
            .iter()
            .any(|e| { matches!(e.kind, SessionEntryKind::Compaction { .. }) }));
        assert!(state.items.iter().any(|it| matches!(
            it,
            TranscriptItem::Assistant { text, .. } if text.contains("Recovered in interactive!")
        )));
    }

    #[tokio::test]
    async fn slash_name_session_hotkeys_scoped_models_export_import() {
        let temp_dir =
            std::env::temp_dir().join(format!("interactive_slash_{}", uuid::Uuid::now_v7()));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            mehh_ai::faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );
        let mut state = InteractiveState::new(vec![], &session.settings);

        // Test /name Demo
        handle_slash_command("/name Demo Session", &mut session, &mut state, None, false).await;
        assert!(state
            .items
            .iter()
            .any(|it| matches!(it, TranscriptItem::System(s) if s.contains("Demo Session"))));
        assert!(session.session().entries().iter().any(|e| matches!(&e.kind, SessionEntryKind::SessionInfo { name: Some(n) } if n == "Demo Session")));

        // Test /name without args reads current name
        handle_slash_command("/name", &mut session, &mut state, None, false).await;
        assert!(state.items.iter().any(|it| matches!(it, TranscriptItem::System(s) if s.contains("Session name: 'Demo Session'"))));

        // Test /session
        handle_slash_command("/session", &mut session, &mut state, None, false).await;
        assert!(state.items.iter().any(
            |it| matches!(it, TranscriptItem::System(s) if s.contains("Estimated Context Tokens:"))
        ));

        // Test /hotkeys
        handle_slash_command("/hotkeys", &mut session, &mut state, None, false).await;
        assert!(state
            .items
            .iter()
            .any(|it| matches!(it, TranscriptItem::System(s) if s.contains("Keyboard Shortcuts"))));

        // Test /scoped-models
        handle_slash_command("/scoped-models", &mut session, &mut state, None, false).await;
        handle_slash_command(
            "/scoped-models faux/faux-model",
            &mut session,
            &mut state,
            None,
            false,
        )
        .await;

        // Test /copy with assistant message
        session
            .session_mut()
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Hello from assistant",
            )))
            .unwrap();
        handle_slash_command("/copy", &mut session, &mut state, None, false).await;
        assert!(state.items.iter().any(|it| matches!(it, TranscriptItem::System(s) if s.contains("Copied message to clipboard"))));

        // Test /reload
        handle_slash_command("/reload", &mut session, &mut state, None, false).await;
        assert!(state
            .items
            .iter()
            .any(|it| matches!(it, TranscriptItem::System(s) if s.contains("Reloaded settings"))));

        // Test /export and /import
        let export_file = temp_dir.join("exported_session.jsonl");
        let export_arg = export_file.to_string_lossy().to_string();
        handle_slash_command(
            &format!("/export {export_arg}"),
            &mut session,
            &mut state,
            None,
            false,
        )
        .await;
        assert!(export_file.exists());

        // Create a new session and import the exported file
        session.session_mut().new_session().unwrap();
        assert_eq!(session.session().entries().len(), 0);

        handle_slash_command(
            &format!("/import {export_arg}"),
            &mut session,
            &mut state,
            None,
            false,
        )
        .await;
        assert!(!session.session().entries().is_empty());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_slash_command_hotkeys_reflects_configured_keybindings() {
        let mut session = make_test_session();
        let mut state = make_test_state();
        state
            .keybindings
            .set_bindings(TuiAction::Submit, vec!["ctrl+enter".to_string()]);

        handle_slash_command("/hotkeys", &mut session, &mut state, None, false).await;

        let last_system_msg = state
            .items
            .iter()
            .rev()
            .find_map(|it| match it {
                TranscriptItem::System(s) => Some(s.as_str()),
                _ => None,
            })
            .expect("must have system message from /hotkeys");

        assert!(
            last_system_msg.contains("ctrl+enter"),
            "hotkeys must reflect configured bindings: {last_system_msg}"
        );
        assert!(last_system_msg.contains("Submit prompt"));
    }
}
