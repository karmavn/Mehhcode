use super::*;

mod auth;
mod autocomplete;
mod settings_ui;

pub use auth::{perform_login, perform_login_with_prompt, perform_logout, refresh_session_api_key};
pub(super) use auth::{
    apply_interactive_model_switch, connected_providers_with_session, disconnect_provider,
    load_interactive_registry, open_connect_for_provider, open_connect_provider_selector,
    open_connect_provider_selector_with, open_model_selector, refresh_connected_state,
};

pub use settings_ui::{copy_toggle, persist_settings, persist_settings_keys, theme_source_exists, thinking_level_label};
pub(super) use settings_ui::{
    PERMISSION_MODES, apply_setting_cycle_dir, apply_setting_value,
    apply_theme_choice, build_settings_list, cycle_session_thinking, discover_available_themes,
    effective_permission_mode, effective_settings_snapshot, parse_thinking_level_str,
    permission_mode_description, permission_mode_label, permission_rule_status_message,
    refresh_settings_selector_with_status, set_session_thinking_level,
    settings_selected_id, settings_selector_items_from_list, sync_theme_preview,
    thinking_level_description, update_selector_filter,
};
#[cfg(test)]
pub(super) use settings_ui::{
    apply_setting_cycle, refresh_settings_selector, settings_selector_items,
};
use settings_ui::{
    available_themes_with_current, select_list_preselected, theme_description,
    thinking_levels_for_model,
};

#[cfg(test)]
pub(super) use autocomplete::update_autocomplete_with_cwd;
pub(super) use autocomplete::update_autocomplete_with_context;

const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/connect", "Connect a provider (OAuth, API key, or custom endpoint)"),
    ("/settings", "Open settings panel (or filter/set: /settings <query|key val>)"),
    ("/mode", "Pick permission mode (chat / approve / smart_approve / auto)"),
    ("/model", "Switch active model"),
    ("/thinking", "Set thinking effort (off / low / medium / high)"),
    ("/theme", "Pick color theme with live preview"),
    ("/scoped-models", "Configure models for quick cycling"),
    ("/help", "Show commands and shortcuts"),
    ("/new", "Start a fresh session"),
    ("/sessions", "Switch session or start a new session"),
    ("/resume", "Resume a saved session"),
    ("/compact", "Summarize older history"),
    ("/tree", "Browse and switch session branches"),
    ("/fork", "Fork session into a new one"),
    ("/clone", "Clone the current session"),
    ("/name", "Set or view session name"),
    ("/session", "Show session info and context stats"),
    ("/export", "Export session to JSONL file"),
    ("/import", "Import session from JSONL file"),
    ("/tools", "List active tools"),
    ("/mcp", "MCP server status, reconnect, auth, and resources"),
    ("/login", "Log in to a provider (OAuth or API key)"),
    ("/logout", "Remove stored provider credential"),
    ("/follow-up", "Queue a follow-up message after current turn"),
    ("/copy", "Copy last assistant reply to clipboard"),
    ("/hotkeys", "Show all keyboard shortcuts"),
    ("/reload", "Reload settings, skills, prompts, and theme"),
    ("/clear", "Clear terminal transcript"),
    ("/exit", "Exit interactive mode"),
    ("/quit", "Quit interactive mode"),
];

/// Built-in slash commands offered for autocomplete and help.
pub fn builtin_slash_commands() -> Vec<SelectItem> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|&(name, desc)| SelectItem::new(name, name).with_description(desc))
        .collect()
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
                        .map(|s| {
                            mehh_tui::sanitize_terminal_text(s, false)
                                .chars()
                                .take(50)
                                .collect::<String>()
                        })
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

    state.open_modal(Selector {
        kind: SelectorKind::Resume,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
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

    match cmd.to_ascii_lowercase().as_str() {
        "/exit" | "/quit" => SlashCommandOutcome::Quit,
        "/clear" => {
            state.items.clear();
            state.reset_transcript_metadata();
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
    /connect [provider]            Connect a provider (OAuth, API key, or custom URL)
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
                state.open_modal(Selector {
                    kind: SelectorKind::Mode,
                    list: select_list_preselected(items, 8, selected_idx),
                    settings_list: None,
                });
            }
            SlashCommandOutcome::Handled
        }
        "/new" => {
            session.clear_queues();
            match session.session_mut().new_session() {
                Ok(()) => {
                    session.todo_state.write().await.clear();
                    state.items.clear();
                    state.reset_transcript_metadata();
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
                    state.discovery_status =
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
            let supported_labels = || -> String {
                profile
                    .levels
                    .iter()
                    .copied()
                    .map(thinking_level_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if let Some(level_str) = arg {
                if let Some(level) = parse_thinking_level_str(level_str) {
                    if !profile.supported && level != ThinkingLevel::Off {
                        state.items.push(TranscriptItem::System(format!(
                            "Model '{}' does not support extended thinking.",
                            session.model.id
                        )));
                    } else if profile.supported
                        && level == ThinkingLevel::Off
                        && !profile.can_disable
                    {
                        state.items.push(TranscriptItem::System(format!(
                            "Model '{}' requires thinking and cannot be disabled.",
                            session.model.id
                        )));
                    } else if profile.supported
                        && level != ThinkingLevel::Off
                        && !session.model.provider.eq_ignore_ascii_case("faux")
                        && !profile.levels.contains(&level)
                    {
                        state.items.push(TranscriptItem::System(format!(
                            "Model '{}' does not support thinking level '{}'. Supported: {}.",
                            session.model.id,
                            thinking_level_label(level),
                            supported_labels()
                        )));
                    } else {
                        set_session_thinking_level(session, level);
                        let effective = session
                            .effective_thinking_level()
                            .unwrap_or(ThinkingLevel::Off);
                        let msg = if effective == ThinkingLevel::Off {
                            "Thinking disabled (this session; use /settings to save default)"
                                .to_string()
                        } else {
                            format!(
                                "Thinking set to {} (this session; use /settings to save default)",
                                thinking_level_label(effective)
                            )
                        };
                        state.items.push(TranscriptItem::System(msg));
                    }
                } else {
                    let choices = if profile.supported && !profile.levels.is_empty() {
                        supported_labels()
                    } else {
                        "off, minimal, low, medium, high, xhigh, max".to_string()
                    };
                    state.items.push(TranscriptItem::System(format!(
                        "Unknown thinking level: '{level_str}'. Choose from: {choices}"
                    )));
                }
            } else if !profile.supported {
                state.items.push(TranscriptItem::System(format!(
                    "Model '{}' does not support extended thinking.",
                    session.model.id
                )));
            } else {
                let current = session
                    .effective_thinking_level()
                    .unwrap_or(ThinkingLevel::Off);
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
                state.open_modal(Selector {
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
                    let msg = apply_theme_choice(name, session, state);
                    state.items.push(TranscriptItem::System(msg));
                }
                None => {
                    if state.has_tool_approval() {
                        return SlashCommandOutcome::Handled;
                    }
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
                    state.open_modal(Selector {
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
                    let mut sl = build_settings_list(&snapshot);
                    let mut list = SelectList::new(settings_selector_items_from_list(&sl), 8);
                    list.set_filter(first);
                    sl.set_filter(first);
                    state.open_modal(Selector {
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
                let sl = build_settings_list(&snapshot);
                let items = settings_selector_items_from_list(&sl);
                state.open_modal(Selector {
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
                                super::state::sync_state_from_session(state, session);
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
                    } else {
                        state.items.push(TranscriptItem::System(
                            "Usage: /tree <entry-id> [--summarize]".to_string(),
                        ));
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
                                let clean_label =
                                    mehh_tui::sanitize_terminal_text(&node.label, false);
                                SelectItem::new(
                                    node.entry_id.clone(),
                                    format!("{indent}{marker}{clean_label}"),
                                )
                                .with_description(node.timestamp.clone())
                            })
                            .collect();
                        state.open_modal(Selector {
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
                        super::state::sync_state_from_session(state, session);
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
                    super::state::sync_state_from_session(state, session);
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
            if let Some(provider) = arg {
                open_connect_for_provider(provider, session, state);
            } else {
                open_connect_provider_selector(session, state);
            }
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
                        if !state.has_tool_approval() {
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
                            state.open_modal(Selector {
                                kind: SelectorKind::ConnectInput,
                                list: SelectList::new(Vec::new(), 1),
                                settings_list: None,
                            });
                        }
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
                Some(provider) => disconnect_provider(provider, session, state).await,
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
            } else if super::events::is_login_slash_command(
                super::state::peel_follow_up_prefixes(text),
            ) {
                state.items.push(TranscriptItem::System(
                    "Cannot queue /login as a follow-up; run /login directly.".to_string(),
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
                    if let Some(n) = session.session().session_name() {
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
            let thinking = session.effective_thinking_level();
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
            match session.reload() {
                Ok(()) => {
                    state.theme_preview_cache.clear();
                    state.set_theme(Theme::load(session.settings.theme.as_deref()));
                    state.keybindings = Keybindings::default_bindings();
                    state.items.push(TranscriptItem::System(
                        "Reloaded settings, skills, and prompt templates.".to_string(),
                    ));
                }
                Err(e) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Failed to reload configuration: {e}"
                    )));
                }
            }
            SlashCommandOutcome::Handled
        }
        "/scoped-models" => {
            match full_arg {
                Some(arg_str) => {
                    let (reg, warn) = load_interactive_registry(state);
                    if let Some(w) = warn {
                        state.items.push(TranscriptItem::System(w));
                    }
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
            let raw_path = full_arg.map(PathBuf::from).unwrap_or(default_path);
            let out_path = if raw_path.is_absolute() {
                raw_path
            } else {
                session.cwd.join(raw_path)
            };
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
            match full_arg {
                Some(file_path_str) => {
                    let path = PathBuf::from(file_path_str.trim());
                    match SessionManager::open_existing_file(&path, session.cwd.clone()) {
                        Ok(new_session) => {
                            let id = new_session.session_id().to_string();
                            let display_path = new_session
                                .file_path()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| path.display().to_string());
                            session.session = new_session;
                            session.clear_queues();
                            session.todo_state.write().await.clear();
                            super::state::sync_state_from_session(state, session);
                            state.items.push(TranscriptItem::System(format!(
                                "Imported session {id} from {display_path}"
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
        _ => SlashCommandOutcome::NotACommand,
    }
}

