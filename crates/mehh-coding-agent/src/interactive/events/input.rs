use super::*;
use super::tasks::{cycle_active_setting_and_refresh, spawn_oauth_login, trigger_model_refresh};

pub(super) async fn apply_tool_approval_decision(
    decision: ToolApprovalDecision,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    state.selector = None;
    state.approval_first_rendered_at = None;
    if let Some(pending) = state.pending_approval.take() {
        if decision.persist_rule || decision.session_rule {
            let arg_prefix = pending
                .args
                .get("command")
                .and_then(|v| v.as_str())
                .and_then(mehh_execpolicy::safe_rule_prefix);
            if mehh_execpolicy::is_shell_tool(&pending.tool_name) && arg_prefix.is_none() {
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
                let save_result = if decision.persist_rule {
                    Some(session.inspection_manager.add_permission_rule(rule).await)
                } else {
                    session.inspection_manager.add_session_permission_rule(rule).await;
                    None
                };
                state.items.push(TranscriptItem::System(
                    permission_rule_status_message(
                        decision.persist_rule,
                        &pending.tool_name,
                        save_result.as_ref(),
                    ),
                ));
            }
        }
        let _ = pending.reply_tx.send(decision.allow);
    }
}

/// Routes pasted text safely: into active `ConnectInput`, dropped when another modal/approval
/// blocks editor input, or inserted into `editor` otherwise. Returns true when a re-render is needed.
pub(in crate::interactive) fn handle_interactive_paste(
    pasted: &str,
    session: &AgentSession,
    state: &mut InteractiveState,
    editor: &mut Editor,
) -> bool {
    if state.has_tool_approval() {
        return false;
    }
    if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ConnectInput) {
        return handle_connect_input_paste(pasted, state);
    }
    if state.selector.is_some() {
        return false;
    }
    editor.insert_text(pasted);
    let text = editor.text();
    update_autocomplete_with_context(
        editor,
        &text,
        &session.cwd,
        Some(session),
        session
            .settings
            .theme
            .as_deref()
            .or(Some(state.theme.name.as_str())),
    );
    true
}

/// Applies pasted text into the active `ConnectInput` modal without leaking secrets into `editor`.
pub(in crate::interactive) fn handle_connect_input_paste(pasted: &str, state: &mut InteractiveState) -> bool {
    let Some(ref mut inp) = state.connect_input else {
        return false;
    };
    let clean: String = pasted
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\r' | '\n'))
        .collect();
    let trimmed = clean.trim();
    if !trimmed.is_empty() {
        inp.value.push_str(trimmed);
        inp.error = None;
    }
    true
}

/// Handles keyboard input inside `SelectorKind::ConnectInput`.
pub(in crate::interactive) async fn handle_connect_input_key(
    key_str: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    match key_str {
        "escape" => {
            state.connect_input = None;
            open_connect_provider_selector(session, state);
        }
        "backspace" => {
            if let Some(ref mut inp) = state.connect_input {
                inp.value.pop();
                inp.error = None;
            }
        }
        "ctrl+u" => {
            if let Some(ref mut inp) = state.connect_input {
                inp.value.clear();
                inp.error = None;
            }
        }
        "enter" => {
            let Some(inp) = state.connect_input.clone() else {
                state.selector = None;
                return;
            };
            let val = inp.value.trim().to_string();
            match inp.kind {
                ConnectInputKind::ApiKey { provider, .. } => {
                    if val.is_empty() {
                        if let Some(ref mut cur) = state.connect_input {
                            cur.error = Some("API key must not be empty".to_string());
                        }
                        return;
                    }
                    match perform_login(&provider, Some(&val), state.auth_file.as_deref()).await {
                        Ok(msg) => {
                            let final_msg = match refresh_session_api_key(
                                session,
                                &provider,
                                state.auth_file.as_deref(),
                            )
                            .await
                            {
                                Ok(()) => msg,
                                Err(e) => {
                                    format!("{msg} (could not refresh the session key: {e})")
                                }
                            };
                            refresh_connected_state(session, state);
                            state.close_modal();
                            state.items.push(TranscriptItem::System(final_msg));
                            trigger_model_refresh(&provider, session, state, true, agent_tx).await;
                        }
                        Err(e) => {
                            if let Some(ref mut cur) = state.connect_input {
                                cur.error = Some(e.to_string());
                            }
                        }
                    }
                }
                ConnectInputKind::CustomProviderId => {
                    let id = val.to_ascii_lowercase();
                    if id.is_empty()
                        || id == "__custom__"
                        || id.starts_with("__action:")
                        || !id
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                    {
                        if let Some(ref mut cur) = state.connect_input {
                            cur.error = Some(
                                "Use letters, digits, '-', '_', or '.' (e.g. ollama)".to_string(),
                            );
                        }
                        return;
                    }
                    state.connect_input = Some(ConnectInputState {
                        kind: ConnectInputKind::CustomBaseUrl { provider_id: id },
                        value: String::new(),
                        masked: false,
                        error: None,
                    });
                }
                ConnectInputKind::CustomBaseUrl { provider_id } => {
                    let parsed = match mehh_ai::normalize_and_validate_http_url(&val) {
                        Ok(u) => u,
                        Err(e) => {
                            if let Some(ref mut cur) = state.connect_input {
                                cur.error = Some(e);
                            }
                            return;
                        }
                    };
                    if let Err(e) =
                        mehh_ai::check_url_host_policy(mehh_ai::DnsPolicy::AllowPrivate, &parsed)
                    {
                        if let Some(ref mut cur) = state.connect_input {
                            cur.error = Some(e);
                        }
                        return;
                    }
                    let base_url = parsed.as_str().trim_end_matches('/').to_string();
                    state.connect_input = Some(ConnectInputState {
                        kind: ConnectInputKind::CustomApiKey {
                            provider_id,
                            base_url,
                        },
                        value: String::new(),
                        masked: true,
                        error: None,
                    });
                }
                ConnectInputKind::CustomApiKey {
                    provider_id,
                    base_url,
                } => {
                    if let Err(e) = crate::models_config::upsert_custom_provider_async(
                        &provider_id,
                        &base_url,
                        None,
                        state.models_file.as_deref(),
                    )
                    .await
                    {
                        if let Some(ref mut cur) = state.connect_input {
                            cur.error = Some(format!("Failed to save provider config: {e}"));
                        }
                        return;
                    }
                    let mut warning = None;
                    if !val.is_empty() {
                        if let Err(e) =
                            perform_login(&provider_id, Some(&val), state.auth_file.as_deref()).await
                        {
                            if let Some(ref mut cur) = state.connect_input {
                                cur.error = Some(format!("Failed to store API key: {e}"));
                            }
                            return;
                        }
                        if let Err(e) = refresh_session_api_key(
                            session,
                            &provider_id,
                            state.auth_file.as_deref(),
                        )
                        .await
                        {
                            warning = Some(format!(
                                "Saved key for {provider_id}, but could not refresh session key: {e}"
                            ));
                        }
                    }
                    refresh_connected_state(session, state);
                    state.close_modal();
                    state.items.push(TranscriptItem::System(format!(
                        "Connected custom provider '{provider_id}' ({base_url})."
                    )));
                    if let Some(w) = warning {
                        state.items.push(TranscriptItem::System(w));
                    }
                    trigger_model_refresh(&provider_id, session, state, true, agent_tx).await;
                }
            }
        }
        other => {
            let mut chars = other.chars();
            if let (Some(ch), None) = (chars.next(), chars.next()) {
                if !ch.is_control() && !ch.is_whitespace() {
                    if let Some(ref mut inp) = state.connect_input {
                        inp.value.push(ch);
                        inp.error = None;
                    }
                }
            }
        }
    }
}

/// Handles keyboard input while any non-approval selector modal is open.
pub(in crate::interactive) async fn handle_non_approval_selector_key(
    key_str: &str,
    keybindings: &Keybindings,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> Option<tokio::task::AbortHandle> {
    if key_str != "escape" && keybindings.matches(key_str, TuiAction::Interrupt) {
        state.close_modal();
        return None;
    }

    if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ConnectInput) {
        handle_connect_input_key(key_str, session, state, agent_tx).await;
        return None;
    }

    if let Some(sel) = state.selector.as_mut() {
        if sel.kind != SelectorKind::ConnectMethod && update_selector_filter(sel, key_str) {
            sync_theme_preview(state);
            return None;
        }
    }

    let mut spawned = None;
    let is_settings = state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::Settings);
    match key_str {
        "left" | "right" if is_settings => {
            if let Some(id) = state.selector.as_ref().and_then(settings_selected_id) {
                cycle_active_setting_and_refresh(&id, key_str == "right", session, state).await;
            }
        }
        "enter" | " " | "space" if key_str == "enter" || is_settings => {
            if is_settings {
                if let Some(id) = state.selector.as_ref().and_then(settings_selected_id) {
                    cycle_active_setting_and_refresh(&id, true, session, state).await;
                }
                return None;
            }
            let item = state
                .selector
                .as_ref()
                .and_then(|s| s.list.selected_item())
                .cloned()?;
            let kind = state.selector.as_ref().map(|s| s.kind);
            state.selector = None;
            if kind != Some(SelectorKind::Theme) {
                if let Some(orig) = state.theme_preview_restore.take() {
                    state.set_theme(orig);
                }
            }
            match kind {
                Some(SelectorKind::Model) => {
                    if item.id == "__action:connect_provider" {
                        open_connect_provider_selector(session, state);
                    } else if item.id == "__action:refresh_models" {
                        trigger_model_refresh("__all__", session, state, true, agent_tx).await;
                    } else {
                        apply_interactive_model_switch(&item.id, session, state).await;
                    }
                }
                Some(SelectorKind::Mode) => {
                    match item.id.parse::<crate::tool_inspection::PermissionMode>() {
                        Ok(mode) => {
                            session.inspection_manager.set_mode(mode).await;
                            session.settings.permission_mode = Some(mode);
                            state.items.push(TranscriptItem::System(format!(
                                "Permission mode set to: {:?} — {} (this session; use /settings to save default)",
                                mode,
                                permission_mode_description(mode)
                            )));
                        }
                        Err(e) => state.items.push(TranscriptItem::System(e)),
                    }
                }
                Some(SelectorKind::Thinking) => {
                    let level = parse_thinking_level_str(&item.id).unwrap_or(ThinkingLevel::Off);
                    set_session_thinking_level(session, level);
                    state.items.push(TranscriptItem::System(format!(
                        "Thinking effort set to {} — {} (this session; use /settings to save default)",
                        thinking_level_label(level),
                        thinking_level_description(level)
                    )));
                }
                Some(SelectorKind::Theme) => {
                    let prev = state.theme_preview_restore.take();
                    let msg = apply_theme_choice(&item.id, session, state);
                    if !msg.starts_with("Theme set to") {
                        if let Some(orig) = prev {
                            state.set_theme(orig);
                        }
                    }
                    state.items.push(TranscriptItem::System(msg));
                }
                Some(SelectorKind::Tree) => {
                    match session.navigate_tree(&item.id, false, None).await {
                        Ok(_) => {
                            crate::interactive::state::sync_state_from_session(state, session);
                            state.items.push(TranscriptItem::System(format!(
                                "Switched to branch at {}",
                                item.id
                            )));
                        }
                        Err(e) => state.items.push(TranscriptItem::System(format!(
                            "Failed to switch branch: {e}"
                        ))),
                    }
                }
                Some(SelectorKind::Resume) => {
                    if item.id == "__action:new_session" {
                        if state.is_busy {
                            state.items.push(TranscriptItem::System(
                                "Cannot start a new session while a turn is running.".to_string(),
                            ));
                        } else {
                            let _ = handle_slash_command("/new", session, state, None, false).await;
                        }
                    } else if item.id == "__action:current_session" {
                        // Already active; nothing to switch.
                    } else if state.is_busy {
                        state.items.push(TranscriptItem::System(
                            "Cannot switch sessions while a turn is running.".to_string(),
                        ));
                    } else {
                        match SessionManager::open_existing_file(
                            std::path::Path::new(&item.id),
                            session.cwd.clone(),
                        )
                        {
                            Ok(mgr) => {
                                session.session = mgr;
                                session.clear_queues();
                                session.todo_state.write().await.clear();
                                crate::interactive::state::sync_state_from_session(state, session);
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
                }
                Some(SelectorKind::Login) => {
                    open_connect_for_provider(&item.id, session, state);
                }
                Some(SelectorKind::ConnectMethod) => {
                    let target = state
                        .connect_provider_target
                        .take()
                        .unwrap_or_default();
                    if !target.is_empty() {
                        match item.id.as_str() {
                            "oauth" => {
                                let auth_file = state.auth_file.clone();
                                spawned = Some(spawn_oauth_login(
                                    &target,
                                    auth_file,
                                    state,
                                    agent_tx,
                                ));
                            }
                            "api_key" => {
                                if !state.has_tool_approval() {
                                    let display = mehh_ai::find_provider_spec(&target)
                                        .map(|s| s.display_name.to_string())
                                        .unwrap_or_else(|| target.clone());
                                    state.connect_input = Some(ConnectInputState {
                                        kind: ConnectInputKind::ApiKey {
                                            provider: target,
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
                            }
                            "refresh_models" => {
                                trigger_model_refresh(&target, session, state, true, agent_tx)
                                    .await;
                            }
                            "disconnect" => {
                                disconnect_provider(&target, session, state).await;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        "escape" => {
            let had_filter = state
                .selector
                .as_ref()
                .is_some_and(|s| !s.list.filter.is_empty());
            if had_filter {
                if let Some(sel) = state.selector.as_mut() {
                    sel.list.set_filter("");
                    if let Some(ref mut sl) = sel.settings_list {
                        sl.set_filter("");
                        sl.clear_status();
                    }
                }
                sync_theme_preview(state);
            } else {
                state.close_modal();
            }
        }
        "up" | "down" | "pageup" | "pagedown" => {
            if let Some(sel) = state.selector.as_mut() {
                if sel.kind == SelectorKind::Settings {
                    if let Some(ref mut sl) = sel.settings_list {
                        match key_str {
                            "up" => sl.move_up(),
                            "down" => sl.move_down(),
                            "pageup" => sl.page_up(),
                            "pagedown" => sl.page_down(),
                            _ => {}
                        }
                        sl.clear_status();
                        sel.list.selected = sl.selected;
                    } else {
                        match key_str {
                            "up" => sel.list.move_up(),
                            "down" => sel.list.move_down(),
                            "pageup" => sel.list.page_up(),
                            "pagedown" => sel.list.page_down(),
                            _ => {}
                        }
                    }
                } else {
                    match key_str {
                        "up" => sel.list.move_up(),
                        "down" => sel.list.move_down(),
                        "pageup" => sel.list.page_up(),
                        "pagedown" => sel.list.page_down(),
                        _ => {}
                    }
                }
            }
            sync_theme_preview(state);
        }
        _ => {}
    }
    spawned
}

/// Handles direct mouse wheel and click interactions across the TUI frame.
#[cfg(test)]
pub(in crate::interactive) async fn handle_mouse_event(
    mouse: crossterm::event::MouseEvent,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    editor: &mut Editor,
    wheel_accelerator: &mut WheelScrollAccelerator,
    keybindings: &Keybindings,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> bool {
    handle_mouse_event_inner(
        mouse,
        session,
        state,
        editor,
        wheel_accelerator,
        keybindings,
        agent_tx,
    )
    .await
    .0
}

pub(in crate::interactive) async fn handle_mouse_event_inner(
    mouse: crossterm::event::MouseEvent,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    editor: &mut Editor,
    wheel_accelerator: &mut WheelScrollAccelerator,
    keybindings: &Keybindings,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> (bool, Option<tokio::task::AbortHandle>) {
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            if let Some(ref mut sel) = state.selector {
                if sel.kind != SelectorKind::ConnectInput {
                    if sel.kind == SelectorKind::Settings {
                        if let Some(ref mut sl) = sel.settings_list {
                            if up {
                                sl.move_up();
                            } else {
                                sl.move_down();
                            }
                            sl.clear_status();
                            sel.list.selected = sl.selected;
                        } else if up {
                            sel.list.move_up();
                        } else {
                            sel.list.move_down();
                        }
                    } else if up {
                        sel.list.move_up();
                    } else {
                        sel.list.move_down();
                    }
                    sync_theme_preview(state);
                    return (true, None);
                }
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            let lines = wheel_accelerator.next(if up { -1 } else { 1 }, now_ms);
            let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
            if up {
                scroll_up_anchored(state, lines, max_scroll);
            } else {
                scroll_down_anchored(state, lines, max_scroll);
            }
            (true, None)
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let row = mouse.row as usize;
            let col = mouse.column as usize;
            let action = state
                .click_regions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .rfind(|r| r.row == row && col >= r.col_start && col < r.col_end)
                .map(|r| r.action.clone());

            let Some(action) = action else {
                return (false, None);
            };

            if state.has_tool_approval() && !matches!(action, ClickAction::ApproveToolChoice(_)) {
                return (false, None);
            }
            if state.selector.is_some()
                && !matches!(
                    action,
                    ClickAction::SelectModalRow(_)
                        | ClickAction::ClickSettingsRow { .. }
                        | ClickAction::ApproveToolChoice(_)
                        | ClickAction::CloseModal
                )
            {
                return (false, None);
            }

            let mut spawned_task = None;
            match action {
                ClickAction::OpenModelPicker => {
                    if state.is_busy {
                        state.items.push(TranscriptItem::System(
                            "Cannot switch models while a turn is running.".to_string(),
                        ));
                    } else {
                        open_model_selector(session, state);
                    }
                }
                ClickAction::OpenConnectProvider => {
                    open_connect_provider_selector(session, state);
                }
                ClickAction::CycleThinking => {
                    cycle_session_thinking(session, state);
                }
                ClickAction::OpenThinkingPicker => {
                    let _ = handle_slash_command("/thinking", session, state, None, false).await;
                }
                ClickAction::OpenModePicker => {
                    let _ = handle_slash_command("/mode", session, state, None, false).await;
                }
                ClickAction::OpenSessionSwitcher => {
                    open_session_switcher(session, state);
                }
                ClickAction::NewSession => {
                    if state.is_busy {
                        state.items.push(TranscriptItem::System(
                            "Cannot start a new session while a turn is running.".to_string(),
                        ));
                    } else {
                        let _ = handle_slash_command("/new", session, state, None, false).await;
                    }
                }
                ClickAction::OpenSettings => {
                    let _ = handle_slash_command("/settings", session, state, None, false).await;
                }
                ClickAction::OpenHelp => {
                    let _ = handle_slash_command("/help", session, state, None, false).await;
                }
                ClickAction::ScrollToBottom => {
                    state.scroll_offset = 0;
                    state.scroll_anchor.store(0, Ordering::Relaxed);
                }
                ClickAction::SelectModalRow(idx) => {
                    if let Some(ref mut sel) = state.selector {
                        sel.list.selected = idx;
                    }
                    spawned_task = handle_non_approval_selector_key(
                        "enter",
                        keybindings,
                        session,
                        state,
                        agent_tx,
                    )
                    .await;
                }
                ClickAction::ClickSettingsRow {
                    filtered_idx,
                    setting_id,
                } => {
                    if let Some(ref mut sel) = state.selector {
                        sel.list.selected = filtered_idx;
                        if let Some(ref mut sl) = sel.settings_list {
                            sl.selected = filtered_idx;
                        }
                    }
                    cycle_active_setting_and_refresh(&setting_id, true, session, state).await;
                }
                ClickAction::ApproveToolChoice(choice_id) => {
                    if !approval_guard_allows(
                        state.approval_first_rendered_at,
                        std::time::Instant::now(),
                    ) {
                        return (false, None);
                    }
                    if let Some(decision) =
                        tool_approval_decision("enter", keybindings, Some(&choice_id))
                    {
                        apply_tool_approval_decision(decision, session, state).await;
                    }
                }
                ClickAction::SelectAutocompleteRow(idx) => {
                    if let Some(ref mut ac) = editor.autocomplete {
                        ac.selected = idx;
                    }
                    if let EditorOutput::Changed(current_text) =
                        editor.handle_editor_key("tab", keybindings)
                    {
                        update_autocomplete_with_context(
                            editor,
                            &current_text,
                            &session.cwd,
                            Some(session),
                            session
                                .settings
                                .theme
                                .as_deref()
                                .or(Some(state.theme.name.as_str())),
                        );
                    }
                }
                ClickAction::CloseModal => {
                    if !state.has_tool_approval() && state.selector.is_some() {
                        state.close_modal();
                    }
                }
            }
            (true, spawned_task)
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Right) => {
            if state.has_tool_approval() {
                return (false, None);
            }
            let row = mouse.row as usize;
            let col = mouse.column as usize;
            let action = state
                .click_regions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .rfind(|r| r.row == row && col >= r.col_start && col < r.col_end)
                .map(|r| r.action.clone());

            match action {
                Some(ClickAction::CycleThinking) if state.selector.is_none() => {
                    let _ = handle_slash_command("/thinking", session, state, None, false).await;
                    (true, None)
                }
                Some(ClickAction::SelectModalRow(idx)) => {
                    if let Some(ref mut sel) = state.selector {
                        sel.list.selected = idx;
                        sync_theme_preview(state);
                        return (true, None);
                    }
                    (false, None)
                }
                _ => (false, None),
            }
        }
        _ => (false, None),
    }
}

pub(in crate::interactive) const APPROVAL_MIN_DISPLAY_MS: u64 = 250;

pub(in crate::interactive) fn approval_guard_allows(
    first_rendered_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    first_rendered_at.is_some_and(|t| {
        now.saturating_duration_since(t) >= Duration::from_millis(APPROVAL_MIN_DISPLAY_MS)
    })
}

pub(in crate::interactive) async fn handle_tool_approval_key(
    key_str: &str,
    keybindings: &Keybindings,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    let Some(rendered_at) = state.approval_first_rendered_at else {
        // Zero-frame guard: prompt has not rendered yet, reject all approval keypresses.
        return false;
    };
    let selected_id = state
        .selector
        .as_ref()
        .and_then(|s| s.list.selected_item())
        .map(|i| i.id.clone());
    match key_str {
        "up" => {
            if let Some(sel) = state.selector.as_mut() {
                sel.list.move_up();
            }
            return false;
        }
        "down" => {
            if let Some(sel) = state.selector.as_mut() {
                sel.list.move_down();
            }
            return false;
        }
        "pageup" => {
            if let Some(sel) = state.selector.as_mut() {
                sel.list.page_up();
            }
            return false;
        }
        "pagedown" => {
            if let Some(sel) = state.selector.as_mut() {
                sel.list.page_down();
            }
            return false;
        }
        _ => {}
    }

    if !approval_guard_allows(Some(rendered_at), std::time::Instant::now()) {
        return false;
    }

    if let Some(decision) = tool_approval_decision(key_str, keybindings, selected_id.as_deref()) {
        apply_tool_approval_decision(decision, session, state).await;
        return true;
    }
    false
}

pub(in crate::interactive) fn is_login_slash_command(input: &str) -> bool {
    let trimmed = input.trim();
    let Some(prefix) = trimmed.get(..6) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("/login") {
        return false;
    }
    let rest = &trimmed[6..];
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

pub(in crate::interactive) fn scrub_sensitive_submission(editor: &mut Editor) {
    editor.scrub_last_history_if(|entry| {
        let candidate = crate::interactive::state::peel_follow_up_prefixes(entry);
        candidate
            .get(..6)
            .is_some_and(|p| p.eq_ignore_ascii_case("/login"))
            && candidate[6..].starts_with(char::is_whitespace)
    });
}

pub(in crate::interactive) fn queue_busy_submission(
    trimmed: &str,
    force_follow_up: bool,
    session: &AgentSession,
    state: &mut InteractiveState,
) {
    let inner = crate::interactive::state::peel_follow_up_prefixes(trimmed);
    if is_login_slash_command(trimmed) || is_login_slash_command(inner) {
        state.items.push(TranscriptItem::System(
            "Cannot run /login while a turn is running; wait for the turn to finish and retry."
                .to_string(),
        ));
        return;
    }
    if force_follow_up {
        session.follow_up(trimmed.to_string());
        state.items.push(TranscriptItem::System(format!(
            "Queued follow-up: {trimmed}"
        )));
        return;
    }
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
}
