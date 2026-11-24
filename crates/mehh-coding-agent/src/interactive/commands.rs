use super::*;

/// Built-in slash commands offered for autocomplete and help.
pub fn builtin_slash_commands() -> Vec<SelectItem> {
    vec![
        SelectItem::new("/connect", "/connect")
            .with_description("Connect a provider (OAuth, API key, or custom endpoint)"),
        SelectItem::new("/settings", "/settings")
            .with_description("Open settings panel (or filter/set: /settings <query|key val>)"),
        SelectItem::new("/mode", "/mode")
            .with_description("Pick permission mode (chat / approve / smart_approve / auto)"),
        SelectItem::new("/model", "/model").with_description("Switch active model"),
        SelectItem::new("/thinking", "/thinking")
            .with_description("Set thinking effort (off / low / medium / high)"),
        SelectItem::new("/theme", "/theme")
            .with_description("Pick color theme with live preview"),
        SelectItem::new("/scoped-models", "/scoped-models")
            .with_description("Configure models for quick cycling"),
        SelectItem::new("/help", "/help").with_description("Show commands and shortcuts"),
        SelectItem::new("/new", "/new").with_description("Start a fresh session"),
        SelectItem::new("/sessions", "/sessions")
            .with_description("Switch session or start a new session"),
        SelectItem::new("/resume", "/resume").with_description("Resume a saved session"),
        SelectItem::new("/compact", "/compact").with_description("Summarize older history"),
        SelectItem::new("/tree", "/tree").with_description("Browse and switch session branches"),
        SelectItem::new("/fork", "/fork").with_description("Fork session into a new one"),
        SelectItem::new("/clone", "/clone").with_description("Clone the current session"),
        SelectItem::new("/name", "/name").with_description("Set or view session name"),
        SelectItem::new("/session", "/session").with_description("Show session info and context stats"),
        SelectItem::new("/export", "/export").with_description("Export session to JSONL file"),
        SelectItem::new("/import", "/import").with_description("Import session from JSONL file"),
        SelectItem::new("/tools", "/tools").with_description("List active tools"),
        SelectItem::new("/mcp", "/mcp")
            .with_description("MCP server status, reconnect, auth, and resources"),
        SelectItem::new("/login", "/login").with_description("Log in to a provider (OAuth or API key)"),
        SelectItem::new("/logout", "/logout").with_description("Remove stored provider credential"),
        SelectItem::new("/follow-up", "/follow-up")
            .with_description("Queue a follow-up message after current turn"),
        SelectItem::new("/copy", "/copy").with_description("Copy last assistant reply to clipboard"),
        SelectItem::new("/hotkeys", "/hotkeys").with_description("Show all keyboard shortcuts"),
        SelectItem::new("/reload", "/reload")
            .with_description("Reload settings, skills, prompts, and theme"),
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
        "default_provider" => target.default_provider = source.default_provider.clone(),
        "default_model" => target.default_model = source.default_model.clone(),
        "permission_mode" => target.permission_mode = source.permission_mode,
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

fn mehh_themes_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))
        .map(PathBuf::from)?;
    Some(home.join(".mehh").join("agent").join("themes"))
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
    let Some(themes_dir) = mehh_themes_dir() else {
        return false;
    };
    let filename = if trimmed.ends_with(".json") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.json")
    };
    themes_dir.join(filename).exists()
}

/// Logs into a provider and stores the resulting credential, delivering any interactive
/// OAuth URL or device code through `on_prompt` instead of writing directly to stdout.
pub async fn perform_login_with_prompt<F>(
    provider: &str,
    api_key: Option<&str>,
    auth_file: Option<&Path>,
    on_prompt: F,
) -> anyhow::Result<String>
where
    F: FnOnce(String) + Send,
{
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        let cred = StoredCredential::ApiKey {
            key: Some(key.to_string()),
            env: None,
        };
        crate::auth::save_credential_async(provider, &cred, auth_file).await?;
        return Ok(format!("Saved API key for {provider}"));
    }

    let kind = crate::auth::oauth_kind_for_provider(provider).ok_or_else(|| {
        anyhow::anyhow!("Provider '{provider}' does not support OAuth. Provide an API key instead.")
    })?;

    let client = mehh_ai::default_http_client();
    let cred = match kind {
        OAuthProviderKind::Anthropic | OAuthProviderKind::OpenAiChatGpt => {
            login_via_pkce_with_prompt(&client, kind, on_prompt).await?
        }
        OAuthProviderKind::GitHubCopilot => {
            login_via_device_flow_with_prompt(&client, on_prompt).await?
        }
    };

    crate::auth::save_credential_async(provider, &cred, auth_file).await?;
    Ok(format!("Logged in to {provider}"))
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
    perform_login_with_prompt(provider, api_key, auth_file, |msg| println!("{msg}")).await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ToolApprovalDecision {
    pub(super) allow: bool,
    pub(super) persist_rule: bool,
    pub(super) session_rule: bool,
}

/// Maps a key press at the tool approval prompt to a decision.
/// The generic interrupt handler runs only after the selector branch returns,
/// so Ctrl+C must be handled here or it is swallowed by the prompt.
pub(super) fn tool_approval_decision(
    key_str: &str,
    keybindings: &Keybindings,
    selected_id: Option<&str>,
) -> Option<ToolApprovalDecision> {
    if keybindings.matches(key_str, TuiAction::Interrupt) {
        return Some(ToolApprovalDecision {
            allow: false,
            persist_rule: false,
            session_rule: false,
        });
    }
    let (allow, persist_rule, session_rule) = match key_str {
        "y" => (true, false, false),
        "a" => (true, false, true),
        "A" => (true, true, false),
        "n" | "escape" => (false, false, false),
        "enter" => match selected_id {
            None | Some("allow_once") => (true, false, false),
            Some("allow_session") => (true, false, true),
            Some("always_allow") => (true, true, false),
            Some(_) => (false, false, false),
        },
        _ => return None,
    };
    Some(ToolApprovalDecision {
        allow,
        persist_rule,
        session_rule,
    })
}

/// Runs the OAuth PKCE flow against a loopback callback listener.
async fn login_via_pkce_with_prompt<F>(
    client: &reqwest::Client,
    kind: OAuthProviderKind,
    on_prompt: F,
) -> anyhow::Result<StoredCredential>
where
    F: FnOnce(String),
{
    // Keep the listener bound until the callback helper takes ownership;
    // dropping it here would let another process steal the port (TOCTOU).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind loopback listener for OAuth callback")?;
    let port = listener
        .local_addr()
        .context("Failed to read loopback listener address")?
        .port();

    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let pkce = oauth::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::build_authorize_url(kind, &pkce.challenge, &state, &redirect_uri)?;

    on_prompt(format!("Open the following URL to authenticate:\n{url}"));

    let code =
        oauth::wait_for_loopback_callback(listener, &state, Duration::from_secs(300)).await?;
    oauth::exchange_code_for_tokens(client, kind, &code, &pkce.verifier, &redirect_uri).await
}

/// Runs the GitHub Copilot device code flow until the user approves or the code expires.
async fn login_via_device_flow_with_prompt<F>(
    client: &reqwest::Client,
    on_prompt: F,
) -> anyhow::Result<StoredCredential>
where
    F: FnOnce(String),
{
    let challenge = oauth::start_copilot_device_flow(client).await?;
    on_prompt(format!(
        "Enter code {} at {}",
        challenge.user_code, challenge.verification_uri
    ));

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

/// Status line shown after an approval decision saved a permission rule.
/// `save_result` is `None` for a session-only rule, which is never persisted.
pub(super) fn permission_rule_status_message(
    persist_rule: bool,
    tool_name: &str,
    save_result: Option<&anyhow::Result<()>>,
) -> String {
    let scope = if persist_rule { "Rule" } else { "Session rule" };
    match save_result {
        Some(Err(e)) => format!("Could not save rule: {e}"),
        Some(Ok(())) | None => format!("{scope} added: always allow tool '{tool_name}'"),
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

/// Discovers built-in themes plus any `.json` files in `~/.mehh/agent/themes/`.
pub(super) fn discover_available_themes() -> Vec<String> {
    let mut themes = vec!["dark".to_string(), "light".to_string()];
    if let Some(dir) = mehh_themes_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut custom: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| ext == "json")
                })
                .filter_map(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                })
                .collect();
            custom.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then(a.cmp(b)));
            for name in custom {
                if !themes.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                    themes.push(name);
                }
            }
        }
    }
    themes
}

/// Monotonic permission-mode order from safest/read-only to full autonomy.
pub(super) const PERMISSION_MODES: [crate::tool_inspection::PermissionMode; 4] = [
    crate::tool_inspection::PermissionMode::Chat,
    crate::tool_inspection::PermissionMode::Approve,
    crate::tool_inspection::PermissionMode::SmartApprove,
    crate::tool_inspection::PermissionMode::Auto,
];

/// Returns the live runtime permission mode when unlocked, falling back to saved settings.
pub(super) fn effective_permission_mode(
    session: &AgentSession,
) -> crate::tool_inspection::PermissionMode {
    session
        .inspection_manager
        .try_mode()
        .or(session.settings.permission_mode)
        .unwrap_or_default()
}

/// Builds a settings snapshot reflecting active runtime mode and thinking level without mutating `session.settings`.
pub(super) async fn effective_settings_snapshot(session: &AgentSession) -> Settings {
    let mut snapshot = session.settings.clone();
    snapshot.permission_mode = Some(session.inspection_manager.mode().await);
    snapshot.default_thinking_level = Some(session.thinking_level().unwrap_or(ThinkingLevel::Off));
    snapshot
}

fn available_themes_with_current(current: &str) -> Vec<String> {
    let mut themes = discover_available_themes();
    if !current.trim().is_empty() && !themes.iter().any(|t| t.eq_ignore_ascii_case(current)) {
        themes.push(current.to_string());
    }
    themes
}

pub(super) fn set_session_thinking_level(session: &mut AgentSession, level: ThinkingLevel) {
    session.set_thinking_level_explicit(level);
}

fn thinking_levels_for_model(model: &mehh_ai::Model, current: ThinkingLevel) -> Vec<ThinkingLevel> {
    let profile = mehh_ai::model_thinking_profile(model);
    let mut levels = profile.levels;
    if profile.supported
        && !levels.contains(&current)
        && (model.provider.eq_ignore_ascii_case("faux") || current != ThinkingLevel::Off)
    {
        levels.push(current);
    }
    levels
}

pub(super) fn parse_thinking_level_str(raw: &str) -> Option<ThinkingLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" | "med" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn thinking_levels_for_current(current: ThinkingLevel) -> Vec<ThinkingLevel> {
    let mut levels = vec![
        ThinkingLevel::Off,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
    ];
    if !levels.contains(&current) {
        let insert_at = match current {
            ThinkingLevel::Minimal => 1,
            ThinkingLevel::Xhigh | ThinkingLevel::Max => levels.len(),
            _ => levels.len(),
        };
        levels.insert(insert_at, current);
    }
    levels
}

/// Canonical lowercase label for a permission mode.
pub(super) fn permission_mode_label(mode: crate::tool_inspection::PermissionMode) -> &'static str {
    use crate::tool_inspection::PermissionMode;
    match mode {
        PermissionMode::Auto => "auto",
        PermissionMode::SmartApprove => "smart_approve",
        PermissionMode::Approve => "approve",
        PermissionMode::Chat => "chat",
    }
}

/// Concise explanation of what each permission mode permits.
pub(super) fn permission_mode_description(
    mode: crate::tool_inspection::PermissionMode,
) -> &'static str {
    use crate::tool_inspection::PermissionMode;
    match mode {
        PermissionMode::SmartApprove => {
            "Auto-allows reads & safe cmds; asks before edits or risky shell"
        }
        PermissionMode::Approve => "Asks confirmation before every tool call",
        PermissionMode::Auto => {
            "Runs tools automatically without asking (security blocks still active)"
        }
        PermissionMode::Chat => {
            "Read-only exploration; blocks all file edits and shell commands"
        }
    }
}

/// Concise explanation of each thinking effort level.
pub(super) fn thinking_level_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "Disabled — fastest responses, no extended reasoning",
        ThinkingLevel::Minimal => "Minimal reasoning for very light tasks",
        ThinkingLevel::Low => "Light reasoning for straightforward tasks",
        ThinkingLevel::Medium => "Balanced reasoning depth and speed",
        ThinkingLevel::High => "Deep reasoning for complex debugging and design",
        ThinkingLevel::Xhigh => "Extended deep reasoning for hard problems",
        ThinkingLevel::Max => "Maximum reasoning budget",
    }
}

fn theme_description(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("dark") {
        "Low-glare dark terminal palette"
    } else if name.eq_ignore_ascii_case("light") {
        "High-contrast palette for bright terminals"
    } else {
        "Custom theme from ~/.mehh/agent/themes"
    }
}

/// Applies and persists a validated theme selection, returning a status message.
pub(super) fn apply_theme_choice(
    name: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> String {
    let theme = Theme::load(Some(name));
    state.theme = theme.clone();
    session.settings.theme = Some(name.to_string());
    match persist_settings(&session.settings, "theme", state.settings_file.as_deref()) {
        Ok(()) => format!("Theme set to '{name}' (active: {})", theme.name),
        Err(e) => format!("Theme set to '{name}' but saving settings failed: {e}"),
    }
}

/// Applies a live theme preview while the `/theme` picker is open.
pub(super) fn sync_theme_preview(state: &mut InteractiveState) {
    if let Some(ref sel) = state.selector {
        if sel.kind == SelectorKind::Theme {
            if let Some(item) = sel.list.selected_item() {
                state.theme = Theme::load(Some(&item.id));
            }
        }
    }
}

fn select_list_preselected(
    items: Vec<SelectItem>,
    max_visible: usize,
    selected_idx: usize,
) -> SelectList {
    let mut list = SelectList::new(items, max_visible);
    if !list.items.is_empty() {
        list.selected = selected_idx.min(list.items.len() - 1);
    }
    list
}

fn cycle_index(len: usize, current: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    }
}

/// Builds the picker entries for the settings screen from `build_settings_list`.
pub(super) fn settings_selector_items(settings: &Settings) -> Vec<SelectItem> {
    build_settings_list(settings)
        .items
        .into_iter()
        .map(|it| {
            let mut item = SelectItem::new(it.id, format!("{}: {}", it.label, it.current_value));
            if let Some(desc) = it.description {
                item = item.with_description(desc);
            }
            item
        })
        .collect()
}

/// Builds the two-column settings list component for the settings screen.
pub(super) fn build_settings_list(settings: &Settings) -> SettingsList {
    let current_mode = settings.permission_mode.unwrap_or_default();
    let mode_values: Vec<String> = PERMISSION_MODES
        .iter()
        .map(|&m| permission_mode_label(m).to_string())
        .collect();
    let mode_descs: Vec<(String, String)> = PERMISSION_MODES
        .iter()
        .map(|&m| {
            (
                permission_mode_label(m).to_string(),
                permission_mode_description(m).to_string(),
            )
        })
        .collect();

    let current_thinking_enum = settings.default_thinking_level.unwrap_or(ThinkingLevel::Off);
    let current_thinking = thinking_level_label(current_thinking_enum);
    let thinking_levels = thinking_levels_for_current(current_thinking_enum);
    let thinking_values: Vec<String> = thinking_levels
        .iter()
        .map(|&l| thinking_level_label(l).to_string())
        .collect();
    let thinking_descs: Vec<(String, String)> = thinking_levels
        .iter()
        .map(|&l| {
            (
                thinking_level_label(l).to_string(),
                thinking_level_description(l).to_string(),
            )
        })
        .collect();

    let current_auto_compact = if settings.auto_compact.unwrap_or(true) {
        "on"
    } else {
        "off"
    };
    let current_theme = settings.theme.clone().unwrap_or_else(|| "dark".to_string());
    let themes = available_themes_with_current(&current_theme);
    let theme_descs: Vec<(String, String)> = themes
        .iter()
        .map(|t| (t.clone(), theme_description(t).to_string()))
        .collect();
    let current_thinking_blocks = if settings.hide_thinking_block.unwrap_or(false) {
        "hide"
    } else {
        "show"
    };

    let items = vec![
        SettingItem::new(
            "permission_mode",
            "Permission mode",
            permission_mode_label(current_mode),
        )
        .with_category("SAFETY & AGENT")
        .with_values(mode_values)
        .with_description("Controls when the agent asks before running tools or shell commands")
        .with_value_descriptions(mode_descs),
        SettingItem::new(
            "default_thinking_level",
            "Thinking effort",
            current_thinking,
        )
        .with_category("SAFETY & AGENT")
        .with_values(thinking_values)
        .with_description("Default reasoning depth for supported models")
        .with_value_descriptions(thinking_descs),
        SettingItem::new("auto_compact", "Auto-compact", current_auto_compact)
            .with_category("SAFETY & AGENT")
            .with_values(vec!["on".to_string(), "off".to_string()])
            .with_description("Automatically summarize older history before hitting the context limit")
            .with_value_descriptions(vec![
                (
                    "on".to_string(),
                    "Compacts older turns automatically before context overflows".to_string(),
                ),
                (
                    "off".to_string(),
                    "Manual compaction only via /compact".to_string(),
                ),
            ]),
        SettingItem::new("theme", "Color theme", current_theme)
            .with_category("APPEARANCE")
            .with_values(themes)
            .with_description("Terminal color palette (built-in or ~/.mehh/agent/themes)")
            .with_value_descriptions(theme_descs),
        SettingItem::new(
            "hide_thinking_block",
            "Thinking blocks",
            current_thinking_blocks,
        )
        .with_category("APPEARANCE")
        .with_values(vec!["show".to_string(), "hide".to_string()])
        .with_description(
            "Show or hide model reasoning blocks in transcript (Ctrl+T toggles expand)",
        )
        .with_value_descriptions(vec![
            (
                "show".to_string(),
                "Render reasoning preview blocks above assistant replies".to_string(),
            ),
            (
                "hide".to_string(),
                "Hide reasoning blocks completely from the transcript".to_string(),
            ),
        ]),
        SettingItem::new(
            "steering_mode",
            "Mid-turn messages",
            queue_mode_label(settings.steering_mode.unwrap_or_default()),
        )
        .with_category("MESSAGE QUEUES")
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("How messages sent while the agent is busy are delivered (steering)")
        .with_value_descriptions(vec![
            (
                "one-at-a-time".to_string(),
                "Deliver one queued message per step so the agent adjusts incrementally"
                    .to_string(),
            ),
            (
                "all".to_string(),
                "Deliver all queued messages together in one batch".to_string(),
            ),
        ]),
        SettingItem::new(
            "follow_up_mode",
            "Follow-up queue",
            queue_mode_label(settings.follow_up_mode.unwrap_or_default()),
        )
        .with_category("MESSAGE QUEUES")
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()])
        .with_description("How follow-ups queued with Alt+Enter or /follow-up run after a turn")
        .with_value_descriptions(vec![
            (
                "one-at-a-time".to_string(),
                "Run one queued follow-up per turn in order".to_string(),
            ),
            (
                "all".to_string(),
                "Combine all queued follow-ups into the next turn".to_string(),
            ),
        ]),
    ];
    SettingsList::new(items, 10)
}

/// Returns the setting ID currently focused in `SelectorKind::Settings`.
/// Reads `settings_list` first so searches matching value/category descriptions stay in sync.
pub(super) fn settings_selected_id(selector: &Selector) -> Option<String> {
    selector
        .settings_list
        .as_ref()
        .and_then(|sl| sl.selected_item().map(|i| i.id.clone()))
        .or_else(|| selector.list.selected_item().map(|i| i.id.clone()))
}

/// Rebuilds the settings overlay after a toggle while preserving focused row, filter, and optional status banner.
pub(super) fn refresh_settings_selector_with_status(
    state: &mut InteractiveState,
    settings: &Settings,
    focus_id: Option<&str>,
    status: Option<(String, bool)>,
) {
    let prev = state
        .selector
        .as_ref()
        .map(|sel| sel.list.selected)
        .unwrap_or(0);
    if let Some(sel) = state.selector.as_mut() {
        sel.list.set_items(settings_selector_items(settings));
        let filter = sel.list.filter.clone();
        let mut detail = build_settings_list(settings);
        detail.set_filter(&filter);
        let resolved_idx = focus_id
            .and_then(|target_id| {
                let filtered = detail.filtered_indices();
                filtered
                    .iter()
                    .position(|&idx| detail.items[idx].id == target_id)
            })
            .unwrap_or_else(|| {
                let count = if focus_id.is_some() {
                    detail.filtered_indices().len()
                } else {
                    sel.list.items.len()
                };
                prev.min(count.saturating_sub(1))
            });
        sel.list.selected = resolved_idx;
        detail.selected = resolved_idx;
        detail.status_message = status;
        sel.settings_list = Some(detail);
    }
}

/// Rebuild the settings overlay after a toggle without moving the cursor.
/// `SelectList::set_items` resets selection to zero, so restore it explicitly.
#[cfg(test)]
pub(super) fn refresh_settings_selector(state: &mut InteractiveState, settings: &Settings) {
    refresh_settings_selector_with_status(state, settings, None, None);
}

/// Feed a typed character or Backspace into a picker's fuzzy filter.
/// Returns false for navigation/action keys so the caller keeps handling them.
pub(super) fn update_selector_filter(selector: &mut Selector, key: &str) -> bool {
    let next = if key == "backspace" {
        let mut filter = selector.list.filter.clone();
        if filter.pop().is_none() {
            return true;
        }
        filter
    } else if key == " "
        && (selector.kind != SelectorKind::Settings || !selector.list.filter.is_empty())
    {
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
        detail.clear_status();
    }
    true
}

/// Cycles a single setting value forward (`forward = true`) or backward (`forward = false`).
pub(super) fn apply_setting_cycle_dir(
    key: &str,
    forward: bool,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<String> {
    match key {
        "permission_mode" => {
            let current = session.settings.permission_mode.unwrap_or_default();
            let curr_idx = PERMISSION_MODES
                .iter()
                .position(|&m| m == current)
                .unwrap_or(0);
            let next = PERMISSION_MODES[cycle_index(PERMISSION_MODES.len(), curr_idx, forward)];
            session.settings.permission_mode = Some(next);
            let label = permission_mode_label(next);
            if next == crate::tool_inspection::PermissionMode::Auto {
                Ok(format!(
                    "Permission mode → {label} (no approval prompts; default for all sessions)"
                ))
            } else {
                Ok(format!("Permission mode → {label}"))
            }
        }
        "theme" => {
            let current = session
                .settings
                .theme
                .clone()
                .unwrap_or_else(|| "dark".to_string());
            let themes = available_themes_with_current(&current);
            let curr_idx = themes
                .iter()
                .position(|t| t.eq_ignore_ascii_case(&current))
                .unwrap_or(0);
            let next = themes[cycle_index(themes.len(), curr_idx, forward)].clone();
            state.theme = Theme::load(Some(&next));
            session.settings.theme = Some(next.clone());
            Ok(format!("Color theme → {next}"))
        }
        "steering_mode" => {
            let current = session.settings.steering_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.steering_mode = Some(next);
            Ok(format!("Mid-turn messages → {}", queue_mode_label(next)))
        }
        "follow_up_mode" => {
            let current = session.settings.follow_up_mode.unwrap_or_default();
            let next = match current {
                QueueMode::OneAtATime => QueueMode::All,
                QueueMode::All => QueueMode::OneAtATime,
            };
            session.settings.follow_up_mode = Some(next);
            Ok(format!("Follow-up queue → {}", queue_mode_label(next)))
        }
        "default_thinking_level" => {
            let current = session.settings.default_thinking_level.unwrap_or(ThinkingLevel::Off);
            let levels = thinking_levels_for_current(current);
            let curr_idx = levels.iter().position(|&l| l == current).unwrap_or(0);
            let next = levels[cycle_index(levels.len(), curr_idx, forward)];
            session.settings.default_thinking_level = Some(next);
            set_session_thinking_level(session, next);
            Ok(format!(
                "Thinking effort → {}",
                thinking_level_label(next)
            ))
        }
        "hide_thinking_block" => {
            let next = !session.settings.hide_thinking_block.unwrap_or(false);
            session.settings.hide_thinking_block = Some(next);
            Ok(if next {
                "Thinking blocks → hide".to_string()
            } else {
                "Thinking blocks → show".to_string()
            })
        }
        "auto_compact" => {
            let next = !session.settings.auto_compact.unwrap_or(true);
            session.settings.auto_compact = Some(next);
            Ok(if next {
                "Auto-compact → on".to_string()
            } else {
                "Auto-compact → off".to_string()
            })
        }
        other => anyhow::bail!("Unknown setting: '{other}'"),
    }
}

/// Cycles a single setting value and applies it to the running session.
#[cfg(test)]
pub(super) fn apply_setting_cycle(
    key: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<String> {
    apply_setting_cycle_dir(key, true, session, state)
}

fn canonical_setting_key(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "permission_mode" | "permission" | "mode" => Some("permission_mode"),
        "default_thinking_level" | "thinking" | "thinking_level" | "thinking_effort" => {
            Some("default_thinking_level")
        }
        "auto_compact" | "compact" => Some("auto_compact"),
        "theme" | "color_theme" => Some("theme"),
        "hide_thinking_block" | "hide_thinking" | "thinking_blocks" => Some("hide_thinking_block"),
        "steering_mode" | "steering" | "mid_turn" => Some("steering_mode"),
        "follow_up_mode" | "follow_up" | "followup" => Some("follow_up_mode"),
        _ => None,
    }
}

fn parse_queue_mode(raw: &str) -> anyhow::Result<QueueMode> {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "one-at-a-time" | "one" | "single" => Ok(QueueMode::OneAtATime),
        "all" | "batch" => Ok(QueueMode::All),
        other => anyhow::bail!("Invalid queue mode '{other}'. Choose: one-at-a-time, all"),
    }
}

fn parse_bool_toggle(raw: &str, truthy: &[&str], falsy: &[&str], valid_hint: &str) -> anyhow::Result<bool> {
    let lower = raw.trim().to_ascii_lowercase();
    if truthy.contains(&lower.as_str()) {
        Ok(true)
    } else if falsy.contains(&lower.as_str()) {
        Ok(false)
    } else {
        anyhow::bail!("Invalid value '{raw}'. Choose: {valid_hint}")
    }
}

/// Applies an explicit `/settings <key> <value>` update and syncs runtime state.
pub(super) async fn apply_setting_value(
    raw_key: &str,
    raw_value: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> anyhow::Result<(String, String)> {
    let Some(key) = canonical_setting_key(raw_key) else {
        anyhow::bail!(
            "Unknown setting '{raw_key}'. Available: permission_mode, thinking, auto_compact, theme, thinking_blocks, steering_mode, follow_up_mode"
        );
    };
    let val = raw_value.trim();
    let summary = match key {
        "permission_mode" => {
            let mode = val.parse::<crate::tool_inspection::PermissionMode>().map_err(|e| anyhow::anyhow!(e))?;
            session.inspection_manager.set_mode(mode).await;
            session.settings.permission_mode = Some(mode);
            format!("Permission mode → {}", permission_mode_label(mode))
        }
        "default_thinking_level" => {
            let Some(lvl) = parse_thinking_level_str(val) else {
                anyhow::bail!(
                    "Invalid thinking effort '{val}'. Choose: off, minimal, low, medium, high, xhigh, max"
                );
            };
            session.settings.default_thinking_level = Some(lvl);
            set_session_thinking_level(session, lvl);
            format!("Thinking effort → {}", thinking_level_label(lvl))
        }
        "auto_compact" => {
            let enabled = parse_bool_toggle(val, &["on", "true", "yes", "1"], &["off", "false", "no", "0"], "on, off")?;
            session.settings.auto_compact = Some(enabled);
            if enabled {
                "Auto-compact → on".to_string()
            } else {
                "Auto-compact → off".to_string()
            }
        }
        "theme" => {
            if !theme_source_exists(val) {
                anyhow::bail!("Unknown theme '{val}'. Use dark, light, or a theme file name.");
            }
            state.theme = Theme::load(Some(val));
            session.settings.theme = Some(val.to_string());
            format!("Color theme → {val}")
        }
        "hide_thinking_block" => {
            let normalized_raw = raw_key.trim().to_ascii_lowercase().replace('-', "_");
            let hidden = if normalized_raw == "thinking_blocks" {
                let show = parse_bool_toggle(
                    val,
                    &["show", "on", "true", "1"],
                    &["hide", "off", "false", "0"],
                    "show, hide",
                )?;
                !show
            } else {
                parse_bool_toggle(
                    val,
                    &["hide", "true", "on", "1"],
                    &["show", "false", "off", "0"],
                    "show, hide",
                )?
            };
            session.settings.hide_thinking_block = Some(hidden);
            if hidden {
                "Thinking blocks → hide".to_string()
            } else {
                "Thinking blocks → show".to_string()
            }
        }
        "steering_mode" => {
            let mode = parse_queue_mode(val)?;
            session.settings.steering_mode = Some(mode);
            format!("Mid-turn messages → {}", queue_mode_label(mode))
        }
        "follow_up_mode" => {
            let mode = parse_queue_mode(val)?;
            session.settings.follow_up_mode = Some(mode);
            format!("Follow-up queue → {}", queue_mode_label(mode))
        }
        _ => unreachable!(),
    };
    Ok((key.to_string(), summary))
}

/// Splits a rendered system prompt into its preamble and its addendum.
pub(super) fn split_system_prompt(prompt: &str) -> (Option<String>, Option<String>) {
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
pub(super) fn sync_mcp_tools(
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

/// Loads `ModelRegistry` with optional discovered-models cache, surfacing non-fatal warnings.
pub(super) fn load_interactive_registry(
    state: &InteractiveState,
) -> (crate::models_config::ModelRegistry, Option<String>) {
    match crate::models_config::ModelRegistry::load_with_discovered(
        state.models_file.as_deref(),
        state.discovered_models_file.as_deref(),
    ) {
        Ok(reg) => (reg, None),
        Err(disc_err) => {
            let base_res = match state.models_file.as_deref() {
                Some(p) => crate::models_config::ModelRegistry::load_with_path(p),
                None => crate::models_config::ModelRegistry::load_default(),
            };
            match base_res {
                Ok(reg) => (
                    reg,
                    Some(format!(
                        "Discovered models cache could not be read: {disc_err}"
                    )),
                ),
                Err(base_err) => (
                    crate::models_config::ModelRegistry::builtin_only(),
                    Some(format!("Failed to load models registry: {base_err}")),
                ),
            }
        }
    }
}

/// Computes connected providers from disk/env/config plus any active session credential.
pub(super) fn connected_providers_with_session(
    session: &AgentSession,
    state: &InteractiveState,
    reg: &crate::models_config::ModelRegistry,
) -> (Vec<crate::auth::ConnectedProvider>, Option<String>) {
    let mut warn = None;
    let mut connected = match crate::auth::list_connected_providers(state.auth_file.as_deref(), reg) {
        Ok(list) => list,
        Err(e) => {
            warn = Some(format!("Failed to read stored credentials: {e}"));
            Vec::new()
        }
    };

    let active_provider = session.model.provider.trim().to_ascii_lowercase();
    let has_active_key = session
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    if (active_provider == "faux" || has_active_key)
        && !active_provider.is_empty()
        && !connected
            .iter()
            .any(|p| p.provider_id.eq_ignore_ascii_case(&active_provider))
    {
        connected.push(crate::auth::ConnectedProvider {
            provider_id: active_provider,
            source: crate::auth::CredentialSource::AuthFile,
            has_oauth: false,
        });
    }

    (connected, warn)
}

/// Refreshes `state.connected_provider_count` from auth/env/models configuration.
pub(super) fn refresh_connected_state(session: &AgentSession, state: &mut InteractiveState) {
    let (reg, _warn) = load_interactive_registry(state);
    let (connected, _auth_warn) = connected_providers_with_session(session, state, &reg);
    state.connected_provider_count = connected
        .iter()
        .filter(|p| !p.provider_id.eq_ignore_ascii_case("faux"))
        .count();
}

/// Opens the unified `/connect` provider selector modal.
pub(super) fn open_connect_provider_selector(
    session: &AgentSession,
    state: &mut InteractiveState,
) {
    let (reg, _warn) = load_interactive_registry(state);
    let (connected, auth_err) = connected_providers_with_session(session, state, &reg);
    if let Some(err) = auth_err {
        state.items.push(TranscriptItem::System(err));
    }

    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut selected_idx = 0usize;

    for spec in mehh_ai::KNOWN_PROVIDERS {
        let lower_id = spec.id.to_ascii_lowercase();
        seen.insert(lower_id.clone());
        let conn = connected
            .iter()
            .find(|c| c.provider_id.eq_ignore_ascii_case(spec.id));
        let auth_methods = if spec
            .auth_methods
            .contains(&mehh_ai::ProviderAuthMethod::OAuth)
            && spec
                .auth_methods
                .contains(&mehh_ai::ProviderAuthMethod::ApiKey)
        {
            "OAuth · API key"
        } else if spec
            .auth_methods
            .contains(&mehh_ai::ProviderAuthMethod::OAuth)
        {
            "OAuth"
        } else {
            "API key"
        };

        let is_active_provider = session.model.provider.eq_ignore_ascii_case(spec.id);
        let (label, desc) = if let Some(c) = conn {
            let src = if c.has_oauth {
                "OAuth"
            } else {
                auth_methods
            };
            (
                format!("{} ✓ connected", spec.display_name),
                format!("{} · {} · connected", spec.id, src),
            )
        } else {
            (
                spec.display_name.to_string(),
                format!("{} · {}", spec.id, auth_methods),
            )
        };
        let mut item = SelectItem::new(spec.id, label).with_description(desc);
        if conn.is_some() {
            item = item.with_label_color(state.theme.success.clone());
        }
        if is_active_provider && conn.is_some() {
            selected_idx = items.len();
        }
        items.push(item);
    }

    let mut custom_ids: Vec<String> = reg
        .provider_configs()
        .keys()
        .filter(|k| !seen.contains(&k.trim().to_ascii_lowercase()))
        .cloned()
        .collect();
    custom_ids.sort();
    for id in custom_ids {
        let base_desc = reg
            .provider_config(&id)
            .and_then(|c| c.base_url.as_deref())
            .unwrap_or("custom endpoint");
        let is_conn = connected
            .iter()
            .any(|c| c.provider_id.eq_ignore_ascii_case(&id));
        let label = if is_conn {
            format!("{id} ✓ connected")
        } else {
            id.clone()
        };
        let mut item =
            SelectItem::new(id.clone(), label).with_description(format!("Custom · {base_desc}"));
        if is_conn {
            item = item.with_label_color(state.theme.success.clone());
        }
        items.push(item);
    }

    items.push(
        SelectItem::new(
            "__custom__",
            "+ Custom OpenAI-compatible (Ollama / vLLM / Proxy)",
        )
        .with_description("Configure custom base URL and optional API key")
        .with_label_color(state.theme.accent.clone()),
    );

    state.connect_input = None;
    state.connect_provider_target = None;
    state.selector = Some(Selector {
        kind: SelectorKind::Login,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
}

/// Opens `/model` listing ONLY models from connected providers, or redirects to `/connect` when none are connected.
pub(super) fn open_model_selector(session: &AgentSession, state: &mut InteractiveState) {
    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let (connected, auth_err) = connected_providers_with_session(session, state, &reg);
    if let Some(err) = auth_err {
        state.items.push(TranscriptItem::System(err));
    }

    if connected.is_empty() {
        state.items.push(TranscriptItem::System(
            "No provider connected yet. Connect a provider to load its models.".to_string(),
        ));
        open_connect_provider_selector(session, state);
        return;
    }

    let models = reg.models_for_connected_providers(&connected);
    if models.is_empty() {
        state.items.push(TranscriptItem::System(
            "No models available yet. Connect a provider or refresh models.".to_string(),
        ));
        open_connect_provider_selector(session, state);
        return;
    }

    let mut items: Vec<SelectItem> = Vec::with_capacity(models.len() + 2);
    let mut selected_idx = 0usize;
    for m in &models {
        let is_current = m.provider.eq_ignore_ascii_case(&session.model.provider)
            && m.id.eq_ignore_ascii_case(&session.model.id);
        if is_current {
            selected_idx = items.len();
        }
        let ctx_k = m.context_window / 1000;
        let prof = mehh_ai::model_thinking_profile(m);
        let mut desc = if m.reasoning {
            if ctx_k > 0 {
                format!(
                    "{} · {}k ctx · thinking ({})",
                    m.id,
                    ctx_k,
                    thinking_level_label(prof.default_level)
                )
            } else {
                format!(
                    "{} · thinking ({})",
                    m.id,
                    thinking_level_label(prof.default_level)
                )
            }
        } else if ctx_k > 0 {
            format!("{} · {}k ctx", m.id, ctx_k)
        } else {
            m.id.clone()
        };
        if is_current {
            desc.push_str(" (current)");
        }
        let label = if is_current {
            format!("{} ({}) (current)", m.name, m.provider)
        } else {
            format!("{} ({})", m.name, m.provider)
        };
        let mut item =
            SelectItem::new(format!("{}/{}", m.provider, m.id), label).with_description(desc);
        if is_current {
            item = item.with_label_color(state.theme.accent.clone());
        }
        items.push(item);
    }

    items.push(
        SelectItem::new(
            "__action:refresh_models",
            "↻ Refresh models from connected providers",
        )
        .with_description("Fetch latest models from provider APIs")
        .with_label_color(state.theme.accent.clone()),
    );
    items.push(
        SelectItem::new("__action:connect_provider", "+ Connect another provider...")
            .with_description("Add OAuth, API key, or custom endpoint")
            .with_label_color(state.theme.accent.clone()),
    );

    state.selector = Some(Selector {
        kind: SelectorKind::Model,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
}

/// Opens the session switcher modal with `+ New session` at the top and current session highlighted.
pub(super) fn open_session_switcher(session: &AgentSession, state: &mut InteractiveState) {
    state.invalidate_context_tokens();
    let mut items = vec![
        SelectItem::new("__action:new_session", "+ New session")
            .with_description("Start a fresh blank conversation")
            .with_label_color(state.theme.accent.clone()),
    ];
    let mut selected_idx = 0usize;
    let cur_id = session.session().session_id();
    let cur_path = session.session().file_path().map(|p| p.to_path_buf());

    match crate::session::list_sessions(&session.cwd) {
        Ok(sessions) => {
            if sessions.is_empty() {
                let short_id: String = cur_id.chars().take(8).collect();
                let name = session
                    .session()
                    .session_name()
                    .unwrap_or_else(|| format!("session {short_id}"));
                items.push(
                    SelectItem::new("__action:current_session", format!("{name} (current)"))
                        .with_description("Current active session (not yet saved to disk)")
                        .with_label_color(state.theme.accent.clone()),
                );
                selected_idx = 1;
            } else {
                for summary in &sessions {
                    let is_current = cur_path
                        .as_ref()
                        .is_some_and(|p| p == &summary.path)
                        || summary.id == cur_id;
                    if is_current {
                        selected_idx = items.len();
                    }
                    let base_label = summary
                        .name
                        .as_deref()
                        .or(summary.first_prompt.as_deref())
                        .map(|s| s.chars().take(50).collect::<String>())
                        .unwrap_or_else(|| summary.id.chars().take(12).collect());
                    let label = if is_current {
                        format!("{base_label} (current)")
                    } else {
                        base_label
                    };
                    let desc = format!(
                        "{} · {} msgs · {}",
                        summary.id.chars().take(8).collect::<String>(),
                        summary.entry_count,
                        summary.timestamp.chars().take(19).collect::<String>()
                    );
                    let mut item = SelectItem::new(
                        summary.path.to_string_lossy().to_string(),
                        label,
                    )
                    .with_description(desc);
                    if is_current {
                        item = item.with_label_color(state.theme.accent.clone());
                    }
                    items.push(item);
                }
            }
        }
        Err(e) => {
            state
                .items
                .push(TranscriptItem::System(format!("Failed to list sessions: {e}")));
        }
    }

    state.selector = Some(Selector {
        kind: SelectorKind::Resume,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
}

/// Cycles the active model's supported thinking levels directly (e.g., clicking `thinking:` pill).
pub(super) fn cycle_session_thinking(session: &mut AgentSession, state: &mut InteractiveState) {
    let profile = mehh_ai::model_thinking_profile(&session.model);
    if !profile.supported || profile.levels.is_empty() {
        state.items.push(TranscriptItem::System(format!(
            "Model '{}' does not support extended thinking.",
            session.model.id
        )));
        return;
    }
    let current = session.thinking_level().unwrap_or(ThinkingLevel::Off);
    let idx = profile
        .levels
        .iter()
        .position(|&lvl| lvl == current)
        .unwrap_or(0);
    let next = profile.levels[(idx + 1) % profile.levels.len()];
    session.set_thinking_level_explicit(next);
    state.status = None;
    let label = thinking_level_label(next);
    state.items.push(TranscriptItem::System(if next == ThinkingLevel::Off {
        "Thinking disabled for this session.".to_string()
    } else {
        format!("Thinking level set to: {label}")
    }));
}

/// Switches the session's model via [`ModelRegistry`] and asynchronously refreshes credentials.
pub(super) async fn apply_interactive_model_switch(
    target: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let resolved = reg.resolve(target, None).or_else(|| {
        reg.models()
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(target.trim()))
            .cloned()
    });
    let Some(m) = resolved else {
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
    session.sync_thinking_for_model(true);
    refresh_connected_state(session, state);

    if !m.provider.eq_ignore_ascii_case("faux") {
        session.settings.default_provider = Some(m.provider.clone());
        session.settings.default_model = Some(m.id.clone());
        if let Err(e) = persist_settings(
            &session.settings,
            "default_provider",
            state.settings_file.as_deref(),
        ) {
            state.items.push(TranscriptItem::System(format!(
                "Warning: could not persist default provider: {e}"
            )));
        }
        if let Err(e) = persist_settings(
            &session.settings,
            "default_model",
            state.settings_file.as_deref(),
        ) {
            state.items.push(TranscriptItem::System(format!(
                "Warning: could not persist default model: {e}"
            )));
        }
    }

    let profile = mehh_ai::model_thinking_profile(&session.model);
    let thinking_note = if profile.supported {
        format!(
            "thinking: {}",
            thinking_level_label(session.thinking_level().unwrap_or(ThinkingLevel::Off))
        )
    } else {
        "thinking: n/a".to_string()
    };
    state.items.push(TranscriptItem::System(format!(
        "Switched model to {} ({}) · {thinking_note}",
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
  Settings & Model:
    /settings [query | key value]  Open settings panel, filter, or set value
    /mode [mode]                   Pick permission mode (smart_approve / approve / auto / chat)
    /model [model_id]              Switch active model (or open picker)
    /thinking [off|low|med|high]   Set thinking effort (or open picker)
    /theme [name]                  Pick color theme (or open live preview)
    /scoped-models [list]          Configure models for quick cycling

  Session & Branching:
    /connect                       Connect a provider (OAuth, API key, or custom URL)
    /new                           Start a fresh session
    /sessions (or /resume)         Switch session or start a new one
    /tree [entry_id]               Browse and switch session branches
    /fork [entry_id]               Fork the session into a new one
    /clone                         Clone the current session
    /compact [instructions]        Summarize older session history
    /name [title]                  Set or view session display name
    /session                       Show session info and context stats
    /export [path]                 Export session to a JSONL file
    /import <path>                 Import a JSONL session

  Tools, Auth & Queue:
    /tools                         List all active tools
    /mcp [status|reconnect|auth|resources]
                                   Inspect and manage MCP servers
    /connect                       Connect a provider (OAuth, API key, or custom URL)
    /login [provider] [key]        Log in to a provider (OAuth or API key)
    /logout <provider>             Remove a stored provider credential
    /follow-up <text>              Queue a message for after the current turn

  Transcript & App:
    /help                          Show this help message
    /hotkeys                       Show all keyboard shortcuts
    /copy                          Copy last assistant reply to clipboard
    /reload                        Reload settings, skills, prompts, and theme
    /clear                         Clear terminal transcript
    /exit or /quit                 Exit interactive mode

Keybindings:
  Enter        Submit prompt (while busy: queue steering message)
  Shift+Enter  Insert newline
  Alt+Enter    Queue follow-up message while busy
  Ctrl+T       Expand / collapse thinking block
  Ctrl+C       Interrupt the turn, clear the input, or exit on an empty prompt
  Esc          Interrupt the turn, clear the input, or close active picker
  Up / Down    Navigate history, picker, or autocomplete";
            state
                .items
                .push(TranscriptItem::System(help_text.to_string()));
            SlashCommandOutcome::Handled
        }
        "/mode" => {
            use crate::tool_inspection::PermissionMode;
            if let Some(mode_str) = full_arg {
                match mode_str.parse::<PermissionMode>() {
                    Ok(new_mode) => {
                        session.inspection_manager.set_mode(new_mode).await;
                        session.settings.permission_mode = Some(new_mode);
                        state.items.push(TranscriptItem::System(format!(
                            "Permission mode set to: {:?} — {} (this session; use /settings to save default)",
                            new_mode,
                            permission_mode_description(new_mode)
                        )));
                    }
                    Err(e) => {
                        state.items.push(TranscriptItem::System(format!(
                            "{e} (or run /mode with no arguments to pick interactively)"
                        )));
                    }
                }
            } else {
                let current = session.inspection_manager.mode().await;
                let selected_idx = PERMISSION_MODES
                    .iter()
                    .position(|&m| m == current)
                    .unwrap_or(0);
                let items: Vec<SelectItem> = PERMISSION_MODES
                    .iter()
                    .map(|&mode| {
                        let key = permission_mode_label(mode);
                        let label = if mode == current {
                            format!("{key} (current)")
                        } else {
                            key.to_string()
                        };
                        let mut item = SelectItem::new(key, label)
                            .with_description(permission_mode_description(mode));
                        if mode == PermissionMode::Auto {
                            item = item.with_label_color(state.theme.warning.clone());
                        } else if mode == current {
                            item = item.with_label_color(state.theme.accent.clone());
                        }
                        item
                    })
                    .collect();
                state.selector = Some(Selector {
                    kind: SelectorKind::Mode,
                    list: select_list_preselected(items, 8, selected_idx),
                    settings_list: None,
                });
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
            // Use the suffix after the command itself. Searching for the first
            // argument inside the whole line also matches "/compact" when the
            // argument is e.g. "act", duplicating the first word.
            let custom_instructions = full_arg
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            SlashCommandOutcome::Compact(custom_instructions)
        }
        "/model" => {
            if let Some(target) = arg {
                if target.eq_ignore_ascii_case("refresh") {
                    state.pending_model_refresh = Some("__all__".to_string());
                    state.pending_open_model_picker = true;
                    state.status =
                        Some("Refreshing models from connected providers...".to_string());
                } else {
                    apply_interactive_model_switch(target, session, state).await;
                }
            } else {
                open_model_selector(session, state);
            }
            SlashCommandOutcome::Handled
        }
        "/thinking" => {
            let profile = mehh_ai::model_thinking_profile(&session.model);
            if let Some(level_str) = arg {
                if let Some(level) = parse_thinking_level_str(level_str) {
                    if !profile.supported && level != ThinkingLevel::Off {
                        state.items.push(TranscriptItem::System(format!(
                            "Model '{}' does not support extended thinking.",
                            session.model.id
                        )));
                    } else if level == ThinkingLevel::Off && !profile.can_disable {
                        state.items.push(TranscriptItem::System(format!(
                            "Model '{}' requires thinking and cannot be disabled.",
                            session.model.id
                        )));
                    } else {
                        set_session_thinking_level(session, level);
                        let msg = if level == ThinkingLevel::Off {
                            "Thinking disabled (this session; use /settings to save default)"
                                .to_string()
                        } else {
                            format!(
                                "Thinking set to {} (this session; use /settings to save default)",
                                thinking_level_label(level)
                            )
                        };
                        state.items.push(TranscriptItem::System(msg));
                    }
                } else {
                    state.items.push(TranscriptItem::System(format!(
                        "Unknown thinking level: '{level_str}'. Choose from: off, low, medium, high"
                    )));
                }
            } else if !profile.supported {
                state.items.push(TranscriptItem::System(format!(
                    "Model '{}' does not support extended thinking.",
                    session.model.id
                )));
            } else {
                let current = session.thinking_level().unwrap_or(ThinkingLevel::Off);
                let levels = thinking_levels_for_model(&session.model, current);
                let selected_idx = levels.iter().position(|&l| l == current).unwrap_or(0);
                let items: Vec<SelectItem> = levels
                    .iter()
                    .map(|&lvl| {
                        let key = thinking_level_label(lvl);
                        let is_current = lvl == current;
                        let label = if is_current {
                            format!("{key} (current)")
                        } else {
                            key.to_string()
                        };
                        let mut item = SelectItem::new(key, label)
                            .with_description(thinking_level_description(lvl));
                        if is_current {
                            item = item.with_label_color(state.theme.accent.clone());
                        }
                        item
                    })
                    .collect();
                state.selector = Some(Selector {
                    kind: SelectorKind::Thinking,
                    list: select_list_preselected(items, 8, selected_idx),
                    settings_list: None,
                });
            }
            SlashCommandOutcome::Handled
        }
        "/theme" => {
            match full_arg {
                Some(name) => {
                    if !theme_source_exists(name) {
                        state.items.push(TranscriptItem::System(format!(
                            "Unknown theme: '{name}'. Use dark, light, or a theme file name."
                        )));
                    } else {
                        let msg = apply_theme_choice(name, session, state);
                        state.items.push(TranscriptItem::System(msg));
                    }
                }
                None => {
                    state.theme_preview_restore = Some(state.theme.clone());
                    let active_name = session
                        .settings
                        .theme
                        .clone()
                        .unwrap_or_else(|| state.theme.name.clone());
                    let themes = available_themes_with_current(&active_name);
                    let selected_idx = themes
                        .iter()
                        .position(|t| t.eq_ignore_ascii_case(&active_name))
                        .or_else(|| {
                            themes
                                .iter()
                                .position(|t| t.eq_ignore_ascii_case(&state.theme.name))
                        })
                        .unwrap_or(0);
                    let items: Vec<SelectItem> = themes
                        .iter()
                        .enumerate()
                        .map(|(idx, name)| {
                            let is_current = idx == selected_idx;
                            let label = if is_current {
                                format!("{name} (current)")
                            } else {
                                name.clone()
                            };
                            let mut item = SelectItem::new(name.clone(), label)
                                .with_description(theme_description(name));
                            if is_current {
                                item = item.with_label_color(state.theme.accent.clone());
                            }
                            item
                        })
                        .collect();
                    state.selector = Some(Selector {
                        kind: SelectorKind::Theme,
                        list: select_list_preselected(items, 8, selected_idx),
                        settings_list: None,
                    });
                }
            }
            SlashCommandOutcome::Handled
        }
        "/settings" => {
            let snapshot = effective_settings_snapshot(session).await;
            if let Some(first) = arg {
                let value_part = rest.trim();
                if value_part.is_empty() {
                    let mut list = SelectList::new(settings_selector_items(&snapshot), 8);
                    let mut sl = build_settings_list(&snapshot);
                    list.set_filter(first);
                    sl.set_filter(first);
                    state.selector = Some(Selector {
                        kind: SelectorKind::Settings,
                        list,
                        settings_list: Some(sl),
                    });
                } else {
                    match apply_setting_value(first, value_part, session, state).await {
                        Ok((key, summary)) => {
                            let msg = match persist_settings(
                                &session.settings,
                                &key,
                                state.settings_file.as_deref(),
                            ) {
                                Ok(()) => format!("Saved: {summary}"),
                                Err(e) => format!("{summary} (saving settings failed: {e})"),
                            };
                            state.items.push(TranscriptItem::System(msg));
                        }
                        Err(e) => {
                            state.items.push(TranscriptItem::System(e.to_string()));
                        }
                    }
                }
            } else {
                let items = settings_selector_items(&snapshot);
                let sl = build_settings_list(&snapshot);
                state.selector = Some(Selector {
                    kind: SelectorKind::Settings,
                    list: SelectList::new(items, 8),
                    settings_list: Some(sl),
                });
            }
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
                    } else if mcp_manager.is_none() {
                        state.items.push(TranscriptItem::System(
                            "MCP is not enabled for this session.".to_string(),
                        ));
                    } else {
                        return SlashCommandOutcome::StartMcpAuth(name);
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
        "/connect" => {
            open_connect_provider_selector(session, state);
            SlashCommandOutcome::Handled
        }
        "/sessions" | "/resume" => {
            open_session_switcher(session, state);
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
                    if let Some(key) = api_key {
                        match perform_login(provider, Some(key), state.auth_file.as_deref()).await {
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
                                refresh_connected_state(session, state);
                                state.pending_model_refresh =
                                    Some(provider.trim().to_ascii_lowercase());
                            }
                            Err(e) => state
                                .items
                                .push(TranscriptItem::System(format!("Login failed: {e}"))),
                        }
                        SlashCommandOutcome::Handled
                    } else if crate::auth::oauth_kind_for_provider(provider).is_some() {
                        SlashCommandOutcome::StartOAuthLogin(provider.to_string())
                    } else {
                        let prov_lower = provider.trim().to_ascii_lowercase();
                        let display = mehh_ai::find_provider_spec(&prov_lower)
                            .map(|s| s.display_name.to_string())
                            .unwrap_or_else(|| prov_lower.clone());
                        state.connect_input = Some(crate::interactive::ConnectInputState {
                            kind: crate::interactive::ConnectInputKind::ApiKey {
                                provider: prov_lower,
                                display_name: display,
                            },
                            value: String::new(),
                            masked: true,
                            error: None,
                        });
                        state.selector = Some(Selector {
                            kind: SelectorKind::ConnectInput,
                            list: SelectList::new(Vec::new(), 1),
                            settings_list: None,
                        });
                        SlashCommandOutcome::Handled
                    }
                }
                None => {
                    open_connect_provider_selector(session, state);
                    SlashCommandOutcome::Handled
                }
            }
        }
        "/logout" => {
            match arg {
                Some(provider) => match crate::auth::remove_credential_async(provider, state.auth_file.as_deref()).await {
                    Ok(true) => {
                        let _ = crate::models_config::remove_discovered_provider_models(
                            provider,
                            state.discovered_models_file.as_deref(),
                        );
                        refresh_connected_state(session, state);
                        state.items.push(TranscriptItem::System(format!(
                            "Removed stored credential for {provider}"
                        )));
                    }
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
                    match crate::clipboard::copy_to_clipboard_async(text).await {
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

fn mode_autocomplete_items(session: Option<&AgentSession>) -> Vec<SelectItem> {
    let current = session.map(effective_permission_mode);
    PERMISSION_MODES
        .iter()
        .map(|&mode| {
            let key = permission_mode_label(mode);
            let desc = if current == Some(mode) {
                format!("{} (current)", permission_mode_description(mode))
            } else {
                permission_mode_description(mode).to_string()
            };
            SelectItem::new(key, key).with_description(desc)
        })
        .collect()
}

fn thinking_autocomplete_items(session: Option<&AgentSession>) -> Vec<SelectItem> {
    let current = session.map(|s| s.thinking_level().unwrap_or(ThinkingLevel::Off));
    let levels = match (session, current) {
        (Some(s), Some(cur)) => thinking_levels_for_model(&s.model, cur),
        _ => vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
    };
    levels
        .iter()
        .map(|&lvl| {
            let key = thinking_level_label(lvl);
            let desc = if current == Some(lvl) {
                format!("{} (current)", thinking_level_description(lvl))
            } else {
                thinking_level_description(lvl).to_string()
            };
            SelectItem::new(key, key).with_description(desc)
        })
        .collect()
}

fn theme_autocomplete_items(current_theme: Option<&str>) -> Vec<SelectItem> {
    let themes = match current_theme {
        Some(ct) => available_themes_with_current(ct),
        None => discover_available_themes(),
    };
    themes
        .into_iter()
        .map(|name| {
            let is_current = current_theme.is_some_and(|ct| ct.eq_ignore_ascii_case(&name));
            let desc = if is_current {
                format!("{} (current)", theme_description(&name))
            } else {
                theme_description(&name).to_string()
            };
            SelectItem::new(name.clone(), name).with_description(desc)
        })
        .collect()
}

fn settings_key_autocomplete_items() -> Vec<SelectItem> {
    [
        ("permission_mode", "Approval policy (chat / approve / smart_approve / auto)"),
        ("default_thinking_level", "Thinking effort (off / low / medium / high)"),
        ("auto_compact", "Auto-compact context (on / off)"),
        ("theme", "Color theme (dark / light / custom)"),
        ("hide_thinking_block", "Thinking blocks (show / hide)"),
        ("steering_mode", "Mid-turn messages (one-at-a-time / all)"),
        ("follow_up_mode", "Follow-up queue (one-at-a-time / all)"),
    ]
    .iter()
    .map(|&(key, desc)| SelectItem::new(format!("{key} "), key).with_description(desc))
    .collect()
}

fn settings_value_autocomplete_items(
    key: &str,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) -> Vec<SelectItem> {
    match canonical_setting_key(key) {
        Some("permission_mode") => mode_autocomplete_items(session),
        Some("default_thinking_level") => thinking_autocomplete_items(session),
        Some("theme") => theme_autocomplete_items(current_theme),
        Some("auto_compact") => {
            let cur = session.map(|s| s.settings.auto_compact.unwrap_or(true));
            [
                ("on", true, "Automatically summarize older history"),
                ("off", false, "Manual compaction only via /compact"),
            ]
            .iter()
            .map(|&(val, b, desc)| {
                let d = if cur == Some(b) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("hide_thinking_block") => {
            let cur = session.map(|s| s.settings.hide_thinking_block.unwrap_or(false));
            [
                ("show", false, "Render reasoning blocks in transcript"),
                ("hide", true, "Hide reasoning blocks from transcript"),
            ]
            .iter()
            .map(|&(val, b, desc)| {
                let d = if cur == Some(b) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("steering_mode") => {
            let cur = session.map(|s| s.settings.steering_mode.unwrap_or_default());
            [
                ("one-at-a-time", QueueMode::OneAtATime, "Deliver one steering message per step"),
                ("all", QueueMode::All, "Deliver all steering messages together"),
            ]
            .iter()
            .map(|&(val, m, desc)| {
                let d = if cur == Some(m) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        Some("follow_up_mode") => {
            let cur = session.map(|s| s.settings.follow_up_mode.unwrap_or_default());
            [
                ("one-at-a-time", QueueMode::OneAtATime, "Run one follow-up per turn in order"),
                ("all", QueueMode::All, "Combine all follow-ups into next turn"),
            ]
            .iter()
            .map(|&(val, m, desc)| {
                let d = if cur == Some(m) {
                    format!("{desc} (current)")
                } else {
                    desc.to_string()
                };
                SelectItem::new(val, val).with_description(d)
            })
            .collect()
        }
        _ => Vec::new(),
    }
}

struct ArgAutocompletePlan {
    candidates: Vec<SelectItem>,
    prefix: String,
    replace_start: usize,
    is_settings_key: bool,
}

/// Resolves argument suggestions for slash commands whose options are known statically.
/// Returns `None` when the command should fall through to `@file` completion.
fn slash_argument_candidates(
    cmd: &str,
    after_cmd: &str,
    total_chars: usize,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) -> Option<ArgAutocompletePlan> {
    let trimmed_after = after_cmd.trim_start();
    match cmd {
        "/settings" => {
            if let Some(split_idx) = trimmed_after.find(char::is_whitespace) {
                let key_tok = &trimmed_after[..split_idx];
                let after_key = &trimmed_after[split_idx..];
                let val_trimmed = after_key.trim_start();
                if val_trimmed.chars().any(char::is_whitespace) {
                    return Some(ArgAutocompletePlan {
                        candidates: Vec::new(),
                        prefix: String::new(),
                        replace_start: 0,
                        is_settings_key: false,
                    });
                }
                let replace_start = total_chars.saturating_sub(val_trimmed.chars().count());
                let items = settings_value_autocomplete_items(key_tok, session, current_theme);
                Some(ArgAutocompletePlan {
                    candidates: items,
                    prefix: val_trimmed.to_string(),
                    replace_start,
                    is_settings_key: false,
                })
            } else {
                let replace_start = total_chars.saturating_sub(trimmed_after.chars().count());
                Some(ArgAutocompletePlan {
                    candidates: settings_key_autocomplete_items(),
                    prefix: trimmed_after.to_string(),
                    replace_start,
                    is_settings_key: true,
                })
            }
        }
        "/mode" | "/thinking" | "/theme" | "/mcp" | "/connect" | "/login" | "/logout" => {
            if trimmed_after.chars().any(char::is_whitespace) {
                return Some(ArgAutocompletePlan {
                    candidates: Vec::new(),
                    prefix: String::new(),
                    replace_start: 0,
                    is_settings_key: false,
                });
            }
            let replace_start = total_chars.saturating_sub(trimmed_after.chars().count());
            let items = match cmd {
                "/mode" => mode_autocomplete_items(session),
                "/thinking" => thinking_autocomplete_items(session),
                "/theme" => theme_autocomplete_items(current_theme),
                "/mcp" => vec![
                    SelectItem::new("status", "status")
                        .with_description("Show MCP connection and tool counts"),
                    SelectItem::new("reconnect", "reconnect")
                        .with_description("Reconnect all or one MCP server"),
                    SelectItem::new("auth", "auth")
                        .with_description("Authenticate an MCP server via OAuth"),
                    SelectItem::new("resources", "resources")
                        .with_description("List resources exposed by MCP servers"),
                ],
                "/connect" | "/login" => mehh_ai::KNOWN_PROVIDERS
                    .iter()
                    .map(|spec| {
                        SelectItem::new(spec.id, spec.id).with_description(spec.display_name)
                    })
                    .collect(),
                "/logout" => vec![
                    SelectItem::new("anthropic", "anthropic")
                        .with_description("Remove stored credential"),
                    SelectItem::new("openai", "openai")
                        .with_description("Remove stored credential"),
                    SelectItem::new("github-copilot", "github-copilot")
                        .with_description("Remove stored credential"),
                ],
                _ => Vec::new(),
            };
            Some(ArgAutocompletePlan {
                candidates: items,
                prefix: trimmed_after.to_string(),
                replace_start,
                is_settings_key: false,
            })
        }
        _ => None,
    }
}

fn filter_command_or_arg_candidates(
    candidates: Vec<SelectItem>,
    prefix: &str,
    match_description: bool,
) -> Vec<SelectItem> {
    if prefix.is_empty() {
        return candidates;
    }
    let lower_prefix = prefix.to_ascii_lowercase();
    let mut matched: Vec<SelectItem> = candidates
        .iter()
        .filter(|it| {
            it.id.trim().to_ascii_lowercase().starts_with(&lower_prefix)
                || it.label.to_ascii_lowercase().starts_with(&lower_prefix)
        })
        .cloned()
        .collect();
    for (_, item) in mehh_tui::fuzzy_filter_indexed(&candidates, prefix, |it| {
        if match_description {
            format!(
                "{} {} {}",
                it.id,
                it.label,
                it.description.as_deref().unwrap_or("")
            )
        } else {
            it.label.clone()
        }
    }) {
        if !matched.iter().any(|existing| existing.id == item.id) {
            matched.push(item.clone());
        }
    }
    matched
}

/// Refreshes the slash command and argument autocomplete popup with optional session context.
pub(super) fn update_autocomplete_with_context(
    editor: &mut Editor,
    current_text: &str,
    cwd: &Path,
    session: Option<&AgentSession>,
    current_theme: Option<&str>,
) {
    let total_chars = current_text.chars().count();
    if current_text.starts_with('/')
        && !current_text.chars().any(char::is_whitespace)
        && editor.cursor_row == 0
        && editor.cursor_col == total_chars
    {
        let commands = builtin_slash_commands();
        if commands.iter().any(|cmd| cmd.id == current_text) {
            editor.clear_autocomplete();
            return;
        }

        let mut matches: Vec<SelectItem> = commands
            .iter()
            .filter(|cmd| cmd.id.starts_with(current_text))
            .map(|cmd| {
                let id = cmd.id.clone();
                let description = cmd
                    .description
                    .clone()
                    .unwrap_or_else(|| "Slash command".to_string());
                SelectItem::new(id.clone(), id).with_description(description)
            })
            .collect();

        if current_text.chars().count() >= 2 {
            let query = current_text.trim_start_matches('/');
            for (_, cmd) in mehh_tui::fuzzy_filter_indexed(&commands, query, |cmd| {
                format!(
                    "{} {}",
                    cmd.id,
                    cmd.description.as_deref().unwrap_or("")
                )
            }) {
                if !matches.iter().any(|existing| existing.id == cmd.id) {
                    let id = cmd.id.clone();
                    let description = cmd
                        .description
                        .clone()
                        .unwrap_or_else(|| "Slash command".to_string());
                    matches.push(SelectItem::new(id.clone(), id).with_description(description));
                }
            }
        }

        if matches.is_empty() {
            editor.clear_autocomplete();
        } else {
            editor.set_autocomplete_replacing(matches, 0);
            if let Some(ref mut ac) = editor.autocomplete {
                ac.max_visible = 7;
            }
        }
        return;
    }

    if current_text.starts_with('/')
        && current_text.contains(' ')
        && editor.cursor_row == 0
        && editor.cursor_col == total_chars
    {
        if let Some(ws_pos) = current_text.find(char::is_whitespace) {
            let cmd = &current_text[..ws_pos];
            let after_cmd = &current_text[ws_pos..];
            if let Some(plan) = slash_argument_candidates(
                cmd,
                after_cmd,
                total_chars,
                session,
                current_theme,
            ) {
                let exact_match = !plan.prefix.is_empty()
                    && (plan
                        .candidates
                        .iter()
                        .any(|c| c.id.trim().eq_ignore_ascii_case(&plan.prefix))
                        || (plan.is_settings_key
                            && canonical_setting_key(&plan.prefix).is_some()));
                if exact_match {
                    editor.clear_autocomplete();
                    return;
                }
                let matches = filter_command_or_arg_candidates(
                    plan.candidates,
                    &plan.prefix,
                    !plan.is_settings_key,
                );
                if matches.is_empty() {
                    editor.clear_autocomplete();
                } else {
                    editor.set_autocomplete_replacing(matches, plan.replace_start);
                    if let Some(ref mut ac) = editor.autocomplete {
                        ac.max_visible = 7;
                    }
                }
                return;
            }
        }
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

/// Refreshes the slash command autocomplete popup for the current editor text.
///
/// The popup displays the full command in the label while the id carries the text
/// inserted at the cursor, matching the file-completion path. A prefix that already
/// spells a command exactly leaves the popup closed, so `Enter` submits the command
/// instead of completing it a second time.
#[cfg(test)]
pub(super) fn update_autocomplete_with_cwd(editor: &mut Editor, current_text: &str, cwd: &Path) {
    update_autocomplete_with_context(editor, current_text, cwd, None, None);
}

