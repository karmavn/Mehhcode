use super::*;

mod input;
mod signals;
mod tasks;

pub use signals::apply_agent_event;
pub(crate) use signals::AgentSignal;
#[cfg(test)]
pub(crate) use signals::{ModelsDiscoveredPayload, OverflowCompactedPayload, ReadyForTurnPayload};
pub(super) use signals::{
    apply_signal_outcome, drain_agent_signals_async, handle_agent_signal_async,
};
#[cfg(test)]
pub(super) use signals::{SignalOutcome, apply_agent_signal, drain_agent_signals};

pub(super) use tasks::{expand_prompt_with_files, trigger_model_refresh};
#[cfg(test)]
pub(super) use tasks::start_turn;
use tasks::{
    finish_mcp_auth_signal, spawn_mcp_auth, spawn_oauth_login, start_manual_compaction,
    start_turn_preparation,
};

pub(super) use input::{
    handle_interactive_paste, handle_non_approval_selector_key, handle_tool_approval_key,
    is_login_slash_command, queue_busy_submission, scrub_sensitive_submission,
};
#[cfg(test)]
pub(super) use input::{
    APPROVAL_MIN_DISPLAY_MS, handle_connect_input_paste, handle_mouse_event,
    handle_mouse_event_inner,
};
#[cfg(not(test))]
use input::handle_mouse_event_inner;

fn render_and_mark_approval(
    state: &mut InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    terminal: &mut dyn Terminal,
    renderer: &mut TuiRenderer,
) {
    render_now(state, editor, session, terminal, renderer);
    if state.has_tool_approval() && state.approval_first_rendered_at.is_none() {
        state.approval_first_rendered_at = Some(std::time::Instant::now());
    }
}

fn restore_terminal_best_effort() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

pub(super) struct ScopedPanicHook {
    restore: Option<Box<dyn FnOnce() + Send>>,
}

impl ScopedPanicHook {
    fn install() -> Self {
        let prev = Arc::new(std::panic::take_hook());
        let prev_in_hook = Arc::clone(&prev);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_best_effort();
            prev_in_hook(info);
        }));
        Self {
            restore: Some(Box::new(move || {
                std::panic::set_hook(Box::new(move |info| prev(info)));
            })),
        }
    }

    #[cfg(test)]
    pub(super) fn with_restore<F: FnOnce() + Send + 'static>(restore: F) -> Self {
        Self {
            restore: Some(Box::new(restore)),
        }
    }
}

impl Drop for ScopedPanicHook {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            if let Some(restore) = self.restore.take() {
                restore();
            }
        }
    }
}

struct InputReaderGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    rx: Option<tokio::sync::mpsc::Receiver<crossterm::event::Event>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl InputReaderGuard {
    fn rx_mut(&mut self) -> Option<&mut tokio::sync::mpsc::Receiver<crossterm::event::Event>> {
        self.rx.as_mut()
    }
}

impl Drop for InputReaderGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.rx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct TaskAbortGuard {
    handles: Vec<tokio::task::AbortHandle>,
}

impl TaskAbortGuard {
    fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    fn push(&mut self, handle: tokio::task::AbortHandle) {
        self.handles.retain(|h| !h.is_finished());
        self.handles.push(handle);
    }

    fn abort_all(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

impl Drop for TaskAbortGuard {
    fn drop(&mut self) {
        self.abort_all();
    }
}

pub(super) fn apply_mcp_refresh_report(
    report: crate::mcp_manager::McpRefreshReport,
    tools: Vec<Arc<dyn AgentTool>>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    codemode_enabled: bool,
) {
    if report.changed {
        sync_mcp_tools(session, tools, codemode_enabled);
        state.items.push(TranscriptItem::System(
            "MCP tools updated via tools/list_changed.".to_string(),
        ));
    }
    for err in report.errors {
        state.items.push(TranscriptItem::System(format!(
            "Warning: MCP tool refresh failed: {err}"
        )));
    }
}

async fn refresh_dirty_mcp_tools(
    mgr: &mut McpManager,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    codemode_enabled: bool,
) {
    if mgr.take_tools_dirty() {
        let report = mgr.refresh_server_tools_report().await;
        let tools = mgr.tools().to_vec();
        apply_mcp_refresh_report(report, tools, session, state, codemode_enabled);
    }
}

pub(super) fn should_handle_transcript_scroll(editor: &Editor, key_str: &str) -> bool {
    !(editor.autocomplete.is_some() && matches!(key_str, "pageup" | "pagedown"))
}

pub(super) fn sync_autocomplete_on_editor_none(
    editor: &mut Editor,
    key_str: &str,
    session: &AgentSession,
    state: &InteractiveState,
) {
    if editor.autocomplete.is_some()
        && !matches!(key_str, "up" | "down" | "pageup" | "pagedown")
    {
        let current_text = editor.text();
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

/// Runs the full interactive TUI mode loop.
pub async fn run_interactive_mode(
    session: &mut AgentSession,
    mcp_summaries: &[McpConnectionSummary],
    mut mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> anyhow::Result<()> {
    let _panic_hook = ScopedPanicHook::install();
    let mut terminal = CrosstermTerminal::new()?;
    // CrosstermTerminal hides the hardware cursor and enters the alternate screen.
    let mut renderer = TuiRenderer::new();
    let mut state = Box::new(InteractiveState::new(mcp_summaries.to_vec(), &session.settings));
    super::state::sync_state_from_session(&mut state, session);
    let mut editor = Editor::new()
        .with_prefix("> ")
        .with_placeholder("Type a message or /help...")
        .with_theme(state.theme.clone());
    let mut keybindings = state.keybindings.clone();

    // Background input reader thread with cooperative stop flag so exiting
    // interactive mode never leaks a permanently blocked read thread.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<crossterm::event::Event>(128);
    let reader_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop_bg = reader_stop.clone();
    let reader_handle = std::thread::spawn(move || {
        while !reader_stop_bg.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(evt) => {
                        if event_tx.blocking_send(evt).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    let mut reader_guard = InputReaderGuard {
        stop: reader_stop,
        rx: Some(event_rx),
        handle: Some(reader_handle),
    };
    let mut background_tasks = TaskAbortGuard::new();

    // Agent event channel. The extra sender keeps the channel open between turns.
    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let _agent_keepalive = agent_tx.clone();

    let approval_broker = Arc::new(InteractiveApprovalBroker::new(agent_tx.clone()));
    session.set_approval_handler(Some(approval_broker));

    let mut tick_interval = tokio::time::interval(Duration::from_millis(80));
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut turn_running = false;
    let mut wheel_accelerator = WheelScrollAccelerator::default();
    let mut deferred_event: Option<crossterm::event::Event> = None;

    // Sync thinking and provider onboarding before the first frame
    let (startup_reg, _) = load_interactive_registry(&state);
    let (startup_connected, startup_auth_err) =
        connected_providers_with_session(session, &state, &startup_reg);
    state.connected_provider_count = startup_connected
        .iter()
        .filter(|p| !p.provider_id.eq_ignore_ascii_case("faux"))
        .count();
    if state.connected_provider_count == 0
        && !session.model.provider.eq_ignore_ascii_case("faux")
        && session.api_key.is_none()
    {
        super::commands::open_connect_provider_selector_with(
            session,
            &mut state,
            &startup_reg,
            &startup_connected,
            startup_auth_err,
        );
    } else {
        session.sync_thinking_for_model(false);
        if !session.model.provider.eq_ignore_ascii_case("faux")
            && startup_reg
                .discovered_models_for(&session.model.provider)
                .is_none()
        {
            let active_prov = session.model.provider.clone();
            trigger_model_refresh(&active_prov, session, &mut state, false, &agent_tx).await;
        }
    }

    // Render initial screen
    render_and_mark_approval(&mut state, &editor, session, &mut terminal, &mut renderer);

    'outer: loop {
        enum LoopInput {
            Term(crossterm::event::Event),
            Signal(AgentSignal),
            Tick,
        }

        let input = if let Some(evt) = deferred_event.take() {
            LoopInput::Term(evt)
        } else {
            let Some(event_rx) = reader_guard.rx_mut() else {
                break 'outer;
            };
            tokio::select! {
                evt = event_rx.recv() => {
                    let Some(evt) = evt else { break 'outer; };
                    LoopInput::Term(evt)
                }
                Some(sig) = agent_rx.recv() => LoopInput::Signal(sig),
                _ = tick_interval.tick(), if turn_running => LoopInput::Tick,
            }
        };

        match input {
            LoopInput::Term(evt) => {
                match evt {
                    Event::Resize(_new_cols, _new_rows) => {
                        for _ in 0..32 {
                            let next = match reader_guard.rx_mut() {
                                Some(rx) => rx.try_recv(),
                                None => break,
                            };
                            match next {
                                Ok(Event::Resize(_, _)) => {}
                                Ok(other) => {
                                    deferred_event = Some(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        renderer.reset();
                        render_and_mark_approval(
                            &mut state,
                            &editor,
                            session,
                            &mut terminal,
                            &mut renderer,
                        );
                    }
                    Event::Mouse(mouse) => {
                        let (mut dirty, spawned) = handle_mouse_event_inner(
                            mouse,
                            session,
                            &mut state,
                            &mut editor,
                            &mut wheel_accelerator,
                            &keybindings,
                            &agent_tx,
                        )
                        .await;
                        if let Some(handle) = spawned {
                            background_tasks.push(handle);
                        }
                        if matches!(
                            mouse.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        ) {
                            for _ in 0..32 {
                                let next = match reader_guard.rx_mut() {
                                    Some(rx) => rx.try_recv(),
                                    None => break,
                                };
                                match next {
                                    Ok(Event::Mouse(m2))
                                        if matches!(
                                            m2.kind,
                                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                                        ) =>
                                    {
                                        let (d2, spawned2) = handle_mouse_event_inner(
                                            m2,
                                            session,
                                            &mut state,
                                            &mut editor,
                                            &mut wheel_accelerator,
                                            &keybindings,
                                            &agent_tx,
                                        )
                                        .await;
                                        dirty |= d2;
                                        if let Some(handle) = spawned2 {
                                            background_tasks.push(handle);
                                        }
                                    }
                                    Ok(other) => {
                                        deferred_event = Some(other);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        if dirty {
                            render_and_mark_approval(
                                &mut state,
                                &editor,
                                session,
                                &mut terminal,
                                &mut renderer,
                            );
                        }
                    }
                    Event::Paste(pasted) => {
                        if handle_interactive_paste(&pasted, session, &mut state, &mut editor) {
                            render_and_mark_approval(
                                &mut state,
                                &editor,
                                session,
                                &mut terminal,
                                &mut renderer,
                            );
                        }
                    }
                    Event::Key(key_event) => {
                        let had_tool_approval = state.has_tool_approval();
                        // Apply agent signals that are already queued so a key press
                        // cannot start a new turn before the previous one was persisted.
                        if let Some(running) = drain_agent_signals_async(
                            &mut agent_rx,
                            session,
                            &mut state,
                            &agent_tx,
                            mcp_manager.as_deref_mut(),
                            codemode_enabled,
                        )
                        .await
                        {
                            turn_running = running;
                        }
                        // Zero-frame approval guard: if an approval request arrived during signal
                        // drain, render it first and drop this keypress rather than confirming blindly.
                        if !had_tool_approval && state.has_tool_approval() {
                            render_and_mark_approval(
                                &mut state,
                                &editor,
                                session,
                                &mut terminal,
                                &mut renderer,
                            );
                            continue 'outer;
                        }
                        if let Some(key_str) = normalize_crossterm_key(key_event) {
                            if key_str == "ctrl+t" {
                                state.thinking_expanded = !state.thinking_expanded;
                                render_and_mark_approval(
                                    &mut state,
                                    &editor,
                                    session,
                                    &mut terminal,
                                    &mut renderer,
                                );
                                continue 'outer;
                            }
                            // 1. Selector overlay handling
                            if state.selector.is_some() {
                                if state.selector.as_ref().map(|s| s.kind)
                                    == Some(SelectorKind::ToolApproval)
                                {
                                    handle_tool_approval_key(
                                        &key_str,
                                        &keybindings,
                                        session,
                                        &mut state,
                                    )
                                    .await;
                                    render_and_mark_approval(
                                        &mut state,
                                        &editor,
                                        session,
                                        &mut terminal,
                                        &mut renderer,
                                    );
                                    continue 'outer;
                                }

                                if let Some(handle) = handle_non_approval_selector_key(
                                    &key_str,
                                    &keybindings,
                                    session,
                                    &mut state,
                                    &agent_tx,
                                )
                                .await
                                {
                                    background_tasks.push(handle);
                                }
                                render_and_mark_approval(
                                    &mut state,
                                    &editor,
                                    session,
                                    &mut terminal,
                                    &mut renderer,
                                );
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
                                    render_and_mark_approval(
                                        &mut state,
                                        &editor,
                                        session,
                                        &mut terminal,
                                        &mut renderer,
                                    );
                                    continue 'outer;
                                }

                                if key_str == "alt+enter" {
                                    let text = editor.text();
                                    let trimmed = text.trim().to_string();
                                    if !trimmed.is_empty() {
                                        queue_busy_submission(&trimmed, true, session, &mut state);
                                        editor.clear();
                                    }
                                    render_and_mark_approval(
                                        &mut state,
                                        &editor,
                                        session,
                                        &mut terminal,
                                        &mut renderer,
                                    );
                                    continue 'outer;
                                }
                            } else if keybindings.matches(&key_str, TuiAction::ClearOrExit)
                                && editor.is_empty()
                            {
                                break 'outer;
                            }

                            // Scroll keybindings operate regardless of turn state when selector is closed
                            if should_handle_transcript_scroll(&editor, &key_str)
                                && keybindings.matches(&key_str, TuiAction::ScrollUp)
                            {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                scroll_up_anchored(&mut state, 5, max_scroll);
                                render_and_mark_approval(
                                    &mut state,
                                    &editor,
                                    session,
                                    &mut terminal,
                                    &mut renderer,
                                );
                                continue 'outer;
                            }
                            if should_handle_transcript_scroll(&editor, &key_str)
                                && keybindings.matches(&key_str, TuiAction::ScrollDown)
                            {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                scroll_down_anchored(&mut state, 5, max_scroll);
                                render_and_mark_approval(
                                    &mut state,
                                    &editor,
                                    session,
                                    &mut terminal,
                                    &mut renderer,
                                );
                                continue 'outer;
                            }
                            if keybindings.matches(&key_str, TuiAction::ScrollBottom) {
                                state.scroll_offset = 0;
                                state.scroll_anchor.store(0, Ordering::Relaxed);
                                render_and_mark_approval(
                                    &mut state,
                                    &editor,
                                    session,
                                    &mut terminal,
                                    &mut renderer,
                                );
                                continue 'outer;
                            }

                            // 3. Normal editor input handling
                            let output = editor.handle_editor_key(&key_str, &keybindings);
                            match output {
                                EditorOutput::Submit(submitted) => {
                                    scrub_sensitive_submission(&mut editor);
                                    let trimmed = submitted.trim();
                                    if trimmed.is_empty() {
                                        render_and_mark_approval(
                                            &mut state,
                                            &editor,
                                            session,
                                            &mut terminal,
                                            &mut renderer,
                                        );
                                        continue 'outer;
                                    }

                                    if turn_running {
                                        queue_busy_submission(trimmed, false, session, &mut state);
                                        render_and_mark_approval(
                                            &mut state,
                                            &editor,
                                            session,
                                            &mut terminal,
                                            &mut renderer,
                                        );
                                        continue 'outer;
                                    }

                                    match handle_slash_command(
                                        trimmed,
                                        session,
                                        &mut state,
                                        mcp_manager.as_deref_mut(),
                                        codemode_enabled,
                                    )
                                    .await
                                    {
                                        SlashCommandOutcome::Quit => {
                                            break 'outer;
                                        }
                                        SlashCommandOutcome::Handled => {
                                            keybindings = state.keybindings.clone();
                                            if let Some(refresh_target) =
                                                state.pending_model_refresh.take()
                                            {
                                                let open_picker = std::mem::take(
                                                    &mut state.pending_open_model_picker,
                                                );
                                                trigger_model_refresh(
                                                    &refresh_target,
                                                    session,
                                                    &mut state,
                                                    open_picker,
                                                    &agent_tx,
                                                )
                                                .await;
                                            }
                                            render_and_mark_approval(
                                                &mut state,
                                                &editor,
                                                session,
                                                &mut terminal,
                                                &mut renderer,
                                            );
                                        }
                                        SlashCommandOutcome::StartOAuthLogin(provider) => {
                                            let auth_file = state.auth_file.clone();
                                            background_tasks.push(spawn_oauth_login(
                                                &provider,
                                                auth_file,
                                                &mut state,
                                                &agent_tx,
                                            ));
                                            render_and_mark_approval(
                                                &mut state,
                                                &editor,
                                                session,
                                                &mut terminal,
                                                &mut renderer,
                                            );
                                        }
                                        SlashCommandOutcome::StartMcpAuth(name) => {
                                            if let Some(handle) = spawn_mcp_auth(
                                                &name,
                                                mcp_manager.as_deref(),
                                                &mut state,
                                                &agent_tx,
                                            ) {
                                                background_tasks.push(handle);
                                            }
                                            render_and_mark_approval(
                                                &mut state,
                                                &editor,
                                                session,
                                                &mut terminal,
                                                &mut renderer,
                                            );
                                        }
                                        SlashCommandOutcome::Compact(custom_instructions) => {
                                            let keep = crate::compaction::CompactionSettings::for_context_window(
                                                session.model.context_window,
                                                true,
                                            )
                                            .keep_recent_tokens;
                                            let active_entries =
                                                session.session().active_path_entries();
                                            if let Some(prep) =
                                                crate::compaction::prepare_compaction_refs(
                                                    &active_entries,
                                                    keep,
                                                )
                                            {
                                                state.is_busy = true;
                                                state.status = Some(
                                                    "Compacting session history...".to_string(),
                                                );
                                                *state
                                                    .turn_started_at
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) =
                                                    Some(std::time::Instant::now());
                                                *state
                                                    .turn_completed_at
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) = None;
                                                background_tasks.push(start_manual_compaction(
                                                    session,
                                                    prep,
                                                    custom_instructions,
                                                    &agent_tx,
                                                ));
                                                turn_running = true;
                                            } else {
                                                state.invalidate_context_tokens();
                                                state.items.push(TranscriptItem::System(
                                                    "History below compaction threshold; no compaction performed.".to_string(),
                                                ));
                                            }
                                            render_and_mark_approval(
                                                &mut state,
                                                &editor,
                                                session,
                                                &mut terminal,
                                                &mut renderer,
                                            );
                                        }
                                        SlashCommandOutcome::NotACommand => {
                                            if !session.model.provider.eq_ignore_ascii_case("faux")
                                                && session.api_key.is_none()
                                                && state.connected_provider_count == 0
                                            {
                                                state.items.push(TranscriptItem::System(
                                                    "No provider connected yet. Connect a provider first:"
                                                        .to_string(),
                                                ));
                                                open_connect_provider_selector(
                                                    session,
                                                    &mut state,
                                                );
                                                render_and_mark_approval(
                                                    &mut state,
                                                    &editor,
                                                    session,
                                                    &mut terminal,
                                                    &mut renderer,
                                                );
                                                continue 'outer;
                                            }
                                            if let Some(ref mut mgr) = mcp_manager {
                                                refresh_dirty_mcp_tools(
                                                    mgr,
                                                    session,
                                                    &mut state,
                                                    codemode_enabled,
                                                )
                                                .await;
                                            }
                                            let (expanded_text, prompt_blocks) =
                                                match expand_prompt_with_files(&session.cwd, trimmed) {
                                                    Ok(v) => v,
                                                    Err(e) => {
                                                        state.items.push(TranscriptItem::System(
                                                            format!("Failed to attach @file: {e}"),
                                                        ));
                                                        render_and_mark_approval(
                                                            &mut state,
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
                                            // Keep the fully expanded prompt so the overflow
                                            // retry does not drop attached @file content.
                                            state.last_prompt_content = Some((
                                                expanded_text.clone(),
                                                prompt_blocks.clone(),
                                            ));
                                            state.overflow_retried = false;
                                            state.is_busy = true;
                                            state.status = Some("Preparing turn...".to_string());
                                            *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
                                            *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                                            render_and_mark_approval(
                                                &mut state,
                                                &editor,
                                                session,
                                                &mut terminal,
                                                &mut renderer,
                                            );
                                            background_tasks.push(start_turn_preparation(
                                                session,
                                                &expanded_text,
                                                prompt_blocks,
                                                state.auth_file.as_deref(),
                                                &agent_tx,
                                            ));
                                            turn_running = true;
                                        }
                                    }
                                }
                                EditorOutput::Changed(current_text) => {
                                    update_autocomplete_with_context(
                                        &mut editor,
                                        &current_text,
                                        &session.cwd,
                                        Some(session),
                                        session.settings.theme.as_deref().or(Some(state.theme.name.as_str())),
                                    );
                                    render_and_mark_approval(
                                        &mut state,
                                        &editor,
                                        session,
                                        &mut terminal,
                                        &mut renderer,
                                    );
                                }
                                EditorOutput::None => {
                                    let before_cursor = (
                                        editor.cursor_row,
                                        editor.cursor_col,
                                        editor
                                            .autocomplete
                                            .as_ref()
                                            .map(|a| (a.selected, a.items.len())),
                                    );
                                    let had_approval = state.has_tool_approval();
                                    sync_autocomplete_on_editor_none(
                                        &mut editor,
                                        &key_str,
                                        session,
                                        &state,
                                    );
                                    let after_cursor = (
                                        editor.cursor_row,
                                        editor.cursor_col,
                                        editor
                                            .autocomplete
                                            .as_ref()
                                            .map(|a| (a.selected, a.items.len())),
                                    );
                                    if after_cursor != before_cursor
                                        || state.has_tool_approval() != had_approval
                                    {
                                        render_and_mark_approval(
                                            &mut state,
                                            &editor,
                                            session,
                                            &mut terminal,
                                            &mut renderer,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            LoopInput::Signal(first_sig) => {
                let mut should_render = false;
                let mut next_opt = Some(first_sig);
                for _ in 0..64 {
                    let Some(sig) = next_opt.take() else {
                        break;
                    };
                    if let AgentSignal::McpAuthCompleted { server, result } = sig {
                        finish_mcp_auth_signal(
                            server,
                            result,
                            mcp_manager.as_deref_mut(),
                            session,
                            &mut state,
                            codemode_enabled,
                        )
                        .await;
                        should_render = true;
                    } else {
                        let render_immediately = matches!(
                            sig,
                            AgentSignal::ToolApprovalRequest(_)
                                | AgentSignal::SystemMessage(_)
                                | AgentSignal::LoginCompleted { .. }
                                | AgentSignal::ModelsDiscovered { .. }
                        );
                        let outcome =
                            handle_agent_signal_async(sig, session, &mut state, &agent_tx).await;
                        let changed = apply_signal_outcome(outcome, &mut state, &mut turn_running);
                        if changed || render_immediately {
                            should_render = true;
                        }
                    }
                    next_opt = agent_rx.try_recv().ok();
                }
                if should_render {
                    render_and_mark_approval(
                        &mut state,
                        &editor,
                        session,
                        &mut terminal,
                        &mut renderer,
                    );
                }
                // Streaming updates are painted by the 80 ms tick, so a burst of
                // token deltas does not rebuild the frame for every delta.
            }
            LoopInput::Tick => {
                state.loader.tick();
                render_and_mark_approval(
                    &mut state,
                    &editor,
                    session,
                    &mut terminal,
                    &mut renderer,
                );
            }
        }
    }

    session.abort();
    session.set_approval_handler(None);
    state.cancel_active_discovery();
    background_tasks.abort_all();
    if let Some(pending) = state.pending_approval.take() {
        let _ = pending.reply_tx.send(false);
    }
    state.approval_first_rendered_at = None;
    drop(reader_guard);
    let _ = terminal.write("\n");
    let _ = terminal.flush();
    Ok(())
}

