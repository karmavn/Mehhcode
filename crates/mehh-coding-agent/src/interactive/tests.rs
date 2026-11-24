use super::*;
use crate::session::SessionManager;
use crate::settings::Settings;
use mehh_ai::{Model, ModelCost, Usage, UsageCost};

#[test]
fn interrupt_key_denies_pending_tool_approval() {
    let bindings = Keybindings::default_bindings();
    let decision = tool_approval_decision("ctrl+c", &bindings, Some("allow_once"))
        .expect("interrupt must deny a pending tool approval");
    assert!(!decision.allow, "interrupt must not allow the tool");
    assert!(!decision.persist_rule && !decision.session_rule);

    let mut custom = Keybindings::default_bindings();
    custom.set_bindings(TuiAction::Interrupt, vec!["ctrl+q".to_string()]);
    let decision = tool_approval_decision("ctrl+q", &custom, Some("allow_once"))
        .expect("a configured interrupt binding must deny a pending tool approval");
    assert!(!decision.allow);

    // Normal approval keys must keep their behavior.
    let allow = tool_approval_decision("y", &bindings, Some("deny"))
        .expect("y must still allow");
    assert!(allow.allow);
}

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

#[tokio::test]
async fn overflow_retry_resends_expanded_file_blocks() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let _keepalive = agent_tx.clone();

    let marker = "EXPANDED_DIFF_MARKER_9F2A";
    let expanded = format!("review <file name=\"large.diff\">{marker}</file>");
    let blocks = vec![UserContentBlock::Text(mehh_ai::TextContent {
        text: expanded.clone(),
        text_signature: None,
    })];

    // The first turn was submitted as `review @large.diff`, but the prompt
    // actually sent contained the expanded file content.
    state.last_prompt_text = Some("review @large.diff".to_string());
    state.last_prompt_content = Some((expanded.clone(), blocks));
    state.overflow_retried = true;

    let outcome = handle_agent_signal_async(
        AgentSignal::OverflowCompacted {
            original_messages: Vec::new(),
            prep: Some(("entry-1".to_string(), 100)),
            result: Ok("summary".to_string()),
            cancelled: false,
            prefix_persisted: false,
        },
        &mut session,
        &mut state,
        &agent_tx,
    )
    .await;
    assert_eq!(outcome, SignalOutcome::RetryStarted);

    let mut finished = None;
    while let Some(sig) = agent_rx.recv().await {
        if let AgentSignal::Finished(messages) = sig {
            finished = Some(messages);
            break;
        }
    }
    let retry_messages = finished.expect("retry turn must finish");
    let sent_text = retry_messages
        .iter()
        .find_map(|m| match m {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => Some(t.clone()),
                UserContent::Blocks(bs) => bs.iter().find_map(|b| match b {
                    UserContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }),
            },
            _ => None,
        })
        .expect("retry turn must carry a user message");
    assert!(
        sent_text.contains(marker),
        "overflow retry must resend expanded @file content, got: {sent_text}"
    );
}

#[test]
fn test_permission_rule_status_reports_save_failure() {
    let saved = Ok::<(), anyhow::Error>(());
    assert_eq!(
        permission_rule_status_message(true, "bash", Some(&saved)),
        "Rule added: always allow tool 'bash'"
    );
    assert_eq!(
        permission_rule_status_message(false, "bash", None),
        "Session rule added: always allow tool 'bash'"
    );

    let failed = permission_rule_status_message(
        true,
        "bash",
        Some(&Err::<(), anyhow::Error>(anyhow::anyhow!("disk full"))),
    );
    assert!(
        failed.contains("Could not save rule") && failed.contains("disk full"),
        "failed save must be reported, got: {failed}"
    );
    assert!(
        !failed.contains("Rule added"),
        "failed save must not claim success: {failed}"
    );
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

    // Without an argument the interactive theme picker opens pre-selected on the active theme.
    assert_eq!(
        handle_slash_command("/theme", &mut session, &mut state, None, false).await,
        SlashCommandOutcome::Handled
    );
    let selector = state.selector.as_ref().expect("theme picker must open");
    assert_eq!(selector.kind, SelectorKind::Theme);
    let selected = selector
        .list
        .selected_item()
        .expect("active theme must be selected");
    assert_eq!(selected.id, "light");
    assert!(selected.label.contains("(current)"), "{:?}", selected.label);

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
fn scroll_down_anchored_transfers_anchor_instead_of_jumping_to_bottom() {
    let mut state = make_test_state();
    state.scroll_offset = 2;
    state.scroll_anchor.store(50, Ordering::Relaxed);

    super::scroll_down_anchored(&mut state, 3, 200);
    let anchor = state.scroll_anchor.load(Ordering::Relaxed);
    assert_eq!(state.scroll_offset + anchor, 49);
    assert!(state.scroll_offset >= 1, "scroll_offset must stay non-zero above bottom");

    super::scroll_down_anchored(&mut state, 49, 200);
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.scroll_anchor.load(Ordering::Relaxed), 0);
}

#[test]
fn slice_transcript_segments_returns_exact_viewport_window() {
    let segments: Vec<Arc<[String]>> = vec![
        Arc::from(["a".to_string(), "b".to_string()]),
        Arc::from([String::new()]),
        Arc::from(["c".to_string(), "d".to_string()]),
    ];
    assert_eq!(
        super::slice_transcript_segments(&segments, 1, 4),
        vec!["b".to_string(), String::new(), "c".to_string()]
    );
    assert_eq!(
        super::slice_transcript_segments(&segments, 3, 5),
        vec!["c".to_string(), "d".to_string()]
    );
    assert!(super::slice_transcript_segments(&segments, 5, 5).is_empty());
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
fn finished_turn_invalidates_context_cache_after_persisting_messages() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    // A render between MessageEnd and Finished can repopulate the cache with
    // the pre-turn transcript before the turn's messages are appended.
    *state.cached_ctx_tokens.lock().unwrap() = Some(0);

    let messages = vec![Message::User(mehh_ai::UserMessage {
        content: UserContent::Text("hello".to_string()),
        timestamp: mehh_ai::now_ms(),
    })];
    assert!(apply_agent_signal(
        AgentSignal::Finished(messages),
        &mut session,
        &mut state,
    ));

    assert!(
        state.cached_ctx_tokens.lock().unwrap().is_none(),
        "cache must be invalidated after finish_prompt mutates the session"
    );
}

#[test]
fn context_meter_shows_nonzero_for_small_usage_on_large_window() {
    let mut session = make_test_session();
    session.model.context_window = 1_000_000;
    session
        .session_mut()
        .append_message(Message::User(mehh_ai::UserMessage {
            content: UserContent::Text("hi".to_string()),
            timestamp: mehh_ai::now_ms(),
        }))
        .unwrap();

    let state = make_test_state();
    let editor = Editor::new();
    let frame = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
    let clean = mehh_tui::strip_ansi(&frame);
    assert!(
        clean.contains("ctx 1%"),
        "meter must show nonzero usage instead of 0%, footer: {clean}"
    );
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
    assert!(ids.contains(&"permission_mode"));
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

#[tokio::test]
async fn mid_turn_overflow_failure_persists_only_trailing_error_without_duplicating_prefix() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();

    let user = Message::User(mehh_ai::UserMessage {
        content: UserContent::Text("run tool".to_string()),
        timestamp: 1,
    });
    let assistant_call = Message::Assistant(mehh_ai::AssistantMessage {
        content: vec![AssistantContentBlock::ToolCall(mehh_ai::ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::Map::new(),
            arguments_error: None,
            thought_signature: None,
        })],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "test-model".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 2,
    });
    let tool_res = Message::ToolResult(mehh_ai::ToolResultMessage {
        tool_call_id: "call_1".to_string(),
        tool_name: "read".to_string(),
        content: vec![mehh_ai::ToolResultContentBlock::Text(mehh_ai::TextContent {
            text: "file body".to_string(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
        timestamp: 3,
        duration_ms: None,
        usage: None,
    });
    let overflow_err = Message::Assistant(mehh_ai::AssistantMessage {
        content: vec![AssistantContentBlock::Text(mehh_ai::TextContent {
            text: String::new(),
            text_signature: None,
        })],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "test-model".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("context length exceeded".to_string()),
        timestamp: 4,
    });

    let prefix = vec![user.clone(), assistant_call.clone(), tool_res.clone()];
    session.finish_prompt(&prefix).unwrap();
    let entries_before = session.session().entries().len();
    assert_eq!(entries_before, 3);

    let full_messages = vec![user, assistant_call, tool_res, overflow_err];
    let outcome = handle_agent_signal_async(
        AgentSignal::OverflowCompacted {
            original_messages: full_messages,
            prep: Some(("e1".to_string(), 1000)),
            result: Err("compaction failed".to_string()),
            cancelled: false,
            prefix_persisted: true,
        },
        &mut session,
        &mut state,
        &agent_tx,
    )
    .await;
    assert_eq!(outcome, SignalOutcome::TurnEnded);
    assert_eq!(
        session.session().entries().len(),
        entries_before + 1,
        "prefix must not be persisted twice on compaction failure"
    );
    assert!(
        state
            .last_turn_error
            .as_deref()
            .is_some_and(|e| e.contains("context length exceeded")),
        "overflow error must still surface in state: {:?}",
        state.last_turn_error
    );
}

#[test]
fn scroll_up_anchored_preserves_nonzero_offset_when_anchor_exceeds_max_scroll() {
    let mut state = make_test_state();
    state.scroll_offset = 1;
    state.scroll_anchor.store(20, std::sync::atomic::Ordering::Relaxed);

    // Simulates turn end where loader disappears and max_scroll shrinks from 21 to 20.
    scroll_up_anchored(&mut state, 1, 20);

    assert!(
        state.scroll_offset >= 1,
        "scroll_up_anchored must keep offset >= 1 so render preserves anchor, got {}",
        state.scroll_offset
    );
    assert_eq!(
        state.scroll_offset + state.scroll_anchor.load(std::sync::atomic::Ordering::Relaxed),
        20
    );
}

#[tokio::test]
async fn settings_filter_selection_uses_settings_list_to_avoid_desync() {
    let dir = temp_dir("mehh_settings_filter_sync");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();

    let kb = Keybindings::default_bindings();
    handle_slash_command("/settings", &mut session, &mut state, None, false).await;

    // "chat" appears in permission_mode's values (not in its label/description when current is auto)
    for ch in ["c", "h", "a", "t"] {
        handle_non_approval_selector_key(ch, &kb, &mut session, &mut state, &agent_tx).await;
    }
    assert_eq!(
        super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref(),
        Some("permission_mode")
    );

    // Pressing Enter cycles permission_mode rather than failing or toggling a stale row
    handle_non_approval_selector_key("enter", &kb, &mut session, &mut state, &agent_tx).await;
    assert_ne!(
        session.settings.permission_mode,
        Some(crate::tool_inspection::PermissionMode::Auto)
    );
    let persisted = SettingsManager::load_file(&settings_file).unwrap();
    assert_eq!(persisted.permission_mode, session.settings.permission_mode);

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_left_right_cycles_syncs_mode_and_escape_clears_filter_first() {
    let dir = temp_dir("mehh_settings_keys");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    session
        .inspection_manager
        .set_mode(crate::tool_inspection::PermissionMode::SmartApprove)
        .await;
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    // Right cycles monotonically forward: SmartApprove -> Auto (showing global auto warning)
    handle_non_approval_selector_key("right", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Auto
    );
    let auto_banner = state
        .selector
        .as_ref()
        .unwrap()
        .settings_list
        .as_ref()
        .unwrap()
        .status_message
        .clone();
    assert!(
        matches!(&auto_banner, Some((msg, false)) if msg.contains("auto") && msg.contains("no approval prompts")),
        "expected global auto warning, got {auto_banner:?}"
    );
    // Left cycles monotonically toward stricter modes: Auto -> SmartApprove -> Approve -> Chat
    handle_non_approval_selector_key("left", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::SmartApprove
    );
    handle_non_approval_selector_key("left", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Approve
    );
    handle_non_approval_selector_key("left", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Chat
    );
    let sl = state
        .selector
        .as_ref()
        .unwrap()
        .settings_list
        .as_ref()
        .unwrap();
    assert!(
        matches!(&sl.status_message, Some((msg, false)) if msg.contains("Saved:")),
        "expected inline saved banner, got {:?}",
        sl.status_message
    );

    // Typing a filter then pressing Esc once clears filter without closing modal; second Esc closes it.
    handle_non_approval_selector_key("t", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(state.selector.as_ref().unwrap().list.filter, "t");
    handle_non_approval_selector_key("escape", &kb, &mut session, &mut state, &agent_tx).await;
    assert!(state.selector.is_some());
    assert_eq!(state.selector.as_ref().unwrap().list.filter, "");
    handle_non_approval_selector_key("escape", &kb, &mut session, &mut state, &agent_tx).await;
    assert!(state.selector.is_none());

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_command_supports_filter_and_direct_key_value_aliases() {
    let dir = temp_dir("mehh_settings_args");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());

    // 1 arg opens /settings pre-filtered
    handle_slash_command("/settings compact", &mut session, &mut state, None, false).await;
    let sel = state.selector.take().expect("filtered settings modal");
    assert_eq!(sel.kind, SelectorKind::Settings);
    assert_eq!(sel.list.filter, "compact");
    assert_eq!(
        sel.settings_list
            .unwrap()
            .selected_item()
            .map(|i| i.id.as_str()),
        Some("auto_compact")
    );

    // 2 args with friendly aliases update runtime and persist
    handle_slash_command("/settings compact off", &mut session, &mut state, None, false).await;
    handle_slash_command("/settings mode chat", &mut session, &mut state, None, false).await;
    handle_slash_command(
        "/settings thinking_blocks hide",
        &mut session,
        &mut state,
        None,
        false,
    )
    .await;

    assert_eq!(session.settings.auto_compact, Some(false));
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Chat
    );
    assert_eq!(session.settings.hide_thinking_block, Some(true));

    let persisted = SettingsManager::load_file(&settings_file).unwrap();
    assert_eq!(persisted.auto_compact, Some(false));
    assert_eq!(
        persisted.permission_mode,
        Some(crate::tool_inspection::PermissionMode::Chat)
    );
    assert_eq!(persisted.hide_thinking_block, Some(true));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn mode_thinking_theme_pickers_preselect_and_preview_live() {
    let dir = temp_dir("mehh_pickers");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    // /mode opens picker and applies selection
    handle_slash_command("/mode", &mut session, &mut state, None, false).await;
    assert_eq!(
        state.selector.as_ref().map(|s| s.kind),
        Some(SelectorKind::Mode)
    );
    while state
        .selector
        .as_ref()
        .unwrap()
        .list
        .selected_item()
        .map(|i| i.id.as_str())
        != Some("chat")
    {
        handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    }
    handle_non_approval_selector_key("enter", &kb, &mut session, &mut state, &agent_tx).await;
    assert!(state.selector.is_none());
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Chat
    );

    // /thinking high applies session-only level without overwriting persisted settings default
    handle_slash_command("/thinking high", &mut session, &mut state, None, false).await;
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::High));
    assert_eq!(session.settings.default_thinking_level, None);
    handle_slash_command("/thinking", &mut session, &mut state, None, false).await;
    let t_sel = state.selector.take().expect("thinking picker");
    assert_eq!(t_sel.kind, SelectorKind::Thinking);
    assert_eq!(t_sel.list.selected_item().unwrap().id, "high");

    // /theme opens picker, previews on navigation, reverts on Esc, persists on Enter
    assert_eq!(state.theme.name, "dark");
    handle_slash_command("/theme", &mut session, &mut state, None, false).await;
    assert_eq!(
        state.selector.as_ref().map(|s| s.kind),
        Some(SelectorKind::Theme)
    );
    handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(state.theme.name, "light", "down must live-preview light theme");
    handle_non_approval_selector_key("escape", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(state.theme.name, "dark", "escape must revert live preview");

    handle_slash_command("/theme", &mut session, &mut state, None, false).await;
    handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    handle_non_approval_selector_key("enter", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(state.theme.name, "light");
    let persisted = SettingsManager::load_file(&settings_file).unwrap();
    assert_eq!(persisted.theme.as_deref(), Some("light"));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn slash_and_argument_autocomplete_suggestions() {
    let dir = temp_dir("mehh_ac_args");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("alpha.rs"), "fn a() {}").unwrap();

    let mut session = make_test_session();
    session
        .inspection_manager
        .set_mode(crate::tool_inspection::PermissionMode::SmartApprove)
        .await;
    session.settings.permission_mode = Some(crate::tool_inspection::PermissionMode::SmartApprove);
    let mut editor = Editor::new();

    // Fuzzy slash match: "/perm" surfaces "/mode" via description match
    editor.set_text("/perm");
    super::update_autocomplete_with_context(&mut editor, "/perm", &dir, Some(&session), Some("dark"));
    let ac = editor.autocomplete.as_ref().expect("fuzzy slash popup");
    assert!(ac.items.iter().any(|i| i.id == "/mode"), "{:?}", ac.items);

    // Argument completion for /mode shows options and marks current
    editor.set_text("/mode ");
    super::update_autocomplete_with_context(&mut editor, "/mode ", &dir, Some(&session), Some("dark"));
    let ac = editor.autocomplete.as_ref().expect("/mode args");
    assert_eq!(ac.items.len(), 4);
    assert!(ac
        .items
        .iter()
        .any(|i| i.id == "smart_approve" && i.description.as_deref().unwrap_or("").contains("(current)")));
    assert_eq!(editor.autocomplete_replace_start, Some(6));

    // Exact /mode argument closes popup so Enter submits immediately
    editor.set_text("/mode chat");
    super::update_autocomplete_with_context(&mut editor, "/mode chat", &dir, Some(&session), Some("dark"));
    assert!(editor.autocomplete.is_none());

    // /settings key then value completion
    editor.set_text("/settings th");
    super::update_autocomplete_with_context(&mut editor, "/settings th", &dir, Some(&session), Some("dark"));
    let ac = editor.autocomplete.as_ref().expect("/settings key");
    assert!(ac.items.iter().any(|i| i.id == "theme "));
    let kb = Keybindings::default_bindings();
    let _ = editor.handle_editor_key("tab", &kb);
    assert_eq!(editor.text(), "/settings theme ");

    let cur = editor.text();
    super::update_autocomplete_with_context(&mut editor, &cur, &dir, Some(&session), Some("dark"));
    let ac = editor.autocomplete.as_ref().expect("/settings theme values");
    assert!(ac.items.iter().any(|i| i.id == "dark"));
    assert!(ac.items.iter().any(|i| i.id == "light"));

    // Non-option commands still fall through to @file completion
    editor.set_text("/follow-up @src/al");
    super::update_autocomplete_with_context(&mut editor, "/follow-up @src/al", &dir, Some(&session), Some("dark"));
    let ac = editor.autocomplete.as_ref().expect("@file after /follow-up");
    assert!(ac.items.iter().any(|i| i.id == "@src/alpha.rs"));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_frame_fits_60x20_and_80x24() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    if let Some(ref mut sl) = state.selector.as_mut().unwrap().settings_list {
        sl.set_status("Saved: Permission mode → smart_approve", false);
    }
    let editor = Editor::new();

    for (w, h) in [(60, 20), (80, 24)] {
        let frame = render_interactive_frame(&state, &editor, &session, w, h);
        assert_eq!(frame.len(), h, "frame must match terminal height {h}");
        for line in &frame {
            let clean = mehh_tui::strip_ansi(line);
            assert!(
                mehh_tui::visible_width(&clean) <= w,
                "line exceeded width {w}: {clean}"
            );
        }
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("Settings"), "{plain}");
        assert!(plain.contains("Type to filter"), "{plain}");
        assert!(plain.contains("Options:"), "{plain}");
        assert!(plain.contains("Esc Close"), "{plain}");
    }
}

#[tokio::test]
async fn help_command_documents_every_builtin_command() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    handle_slash_command("/help", &mut session, &mut state, None, false).await;
    let help = match state.items.last() {
        Some(TranscriptItem::System(text)) => text.as_str(),
        other => panic!("expected /help system message, got {other:?}"),
    };
    for cmd in builtin_slash_commands() {
        assert!(
            help.contains(&cmd.id),
            "/help missing built-in command {}",
            cmd.id
        );
    }
}

#[tokio::test]
async fn settings_60x20_lower_rows_with_status_and_scroll_preserve_borders() {
    let dir = temp_dir("mehh_settings_60x20_borders");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    let mut state = make_test_state();
    state.settings_file = Some(settings_file);
    state.items.push(TranscriptItem::System("Turn 1".to_string()));
    state.scroll_offset = 1;
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    for _ in 0..5 {
        handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    }
    assert_eq!(
        super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref(),
        Some("steering_mode")
    );
    handle_non_approval_selector_key("right", &kb, &mut session, &mut state, &agent_tx).await;

    let editor = Editor::new();
    for (w, h) in [(60, 20), (80, 24)] {
        let frame = render_interactive_frame(&state, &editor, &session, w, h);
        assert_eq!(frame.len(), h, "frame must match terminal height {h}");
        for line in &frame {
            let clean = mehh_tui::strip_ansi(line);
            assert!(
                mehh_tui::visible_width(&clean) <= w,
                "line exceeded width {w}: {clean}"
            );
        }
        let plain = mehh_tui::strip_ansi(&frame.join("\n"));
        assert!(plain.contains("╭"), "top border deleted at {w}x{h}:\n{plain}");
        assert!(plain.contains("╰"), "bottom border deleted at {w}x{h}:\n{plain}");
        assert!(plain.contains("Saved:"), "{plain}");
        assert!(plain.contains("Mid-turn messages"), "{plain}");
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_thinking_blocks_polarity_matches_key_name() {
    let dir = temp_dir("mehh_thinking_blocks_polarity");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());

    handle_slash_command("/settings thinking_blocks off", &mut session, &mut state, None, false).await;
    assert_eq!(session.settings.hide_thinking_block, Some(true));
    assert!(matches!(
        state.items.last(),
        Some(TranscriptItem::System(msg)) if msg.contains("Thinking blocks → hide")
    ));

    handle_slash_command("/settings thinking_blocks on", &mut session, &mut state, None, false).await;
    assert_eq!(session.settings.hide_thinking_block, Some(false));
    assert!(matches!(
        state.items.last(),
        Some(TranscriptItem::System(msg)) if msg.contains("Thinking blocks → show")
    ));
    assert_eq!(
        SettingsManager::load_file(&settings_file)
            .unwrap()
            .hide_thinking_block,
        Some(false)
    );

    handle_slash_command("/settings hide_thinking_block true", &mut session, &mut state, None, false).await;
    assert_eq!(session.settings.hide_thinking_block, Some(true));
    handle_slash_command("/settings hide_thinking_block false", &mut session, &mut state, None, false).await;
    assert_eq!(session.settings.hide_thinking_block, Some(false));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_space_in_nonempty_filter_does_not_mutate_setting() {
    let dir = temp_dir("mehh_settings_filter_space");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    session
        .inspection_manager
        .set_mode(crate::tool_inspection::PermissionMode::SmartApprove)
        .await;
    let mut state = make_test_state();
    state.settings_file = Some(settings_file.clone());
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    for key in ["p", "e", "r", "m", "i", "s", "s", "i", "o", "n", " ", "m", "o", "d", "e"] {
        handle_non_approval_selector_key(key, &kb, &mut session, &mut state, &agent_tx).await;
    }

    let sel = state.selector.as_ref().unwrap();
    assert_eq!(sel.list.filter, "permission mode");
    assert_eq!(
        super::settings_selected_id(sel).as_deref(),
        Some("permission_mode")
    );
    assert!(sel.settings_list.as_ref().unwrap().status_message.is_none());
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::SmartApprove
    );
    assert!(!settings_file.exists());

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn settings_cycling_filtered_by_value_keeps_cursor_on_same_setting() {
    let dir = temp_dir("mehh_settings_filter_value_stability");
    let settings_file = dir.join("settings.json");

    let mut session = make_test_session();
    session.settings.steering_mode = Some(QueueMode::All);
    session.settings.follow_up_mode = Some(QueueMode::OneAtATime);
    let mut state = make_test_state();
    state.settings_file = Some(settings_file);
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    handle_slash_command("/settings all", &mut session, &mut state, None, false).await;
    for _ in 0..7 {
        if super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref()
            == Some("steering_mode")
        {
            break;
        }
        handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    }
    assert_eq!(
        super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref(),
        Some("steering_mode")
    );

    handle_non_approval_selector_key("right", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(session.settings.steering_mode, Some(QueueMode::OneAtATime));
    assert_eq!(
        super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref(),
        Some("steering_mode")
    );

    handle_non_approval_selector_key("left", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(session.settings.steering_mode, Some(QueueMode::All));
    assert_eq!(
        super::settings_selected_id(state.selector.as_ref().unwrap()).as_deref(),
        Some("steering_mode")
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn selector_empty_filter_enter_stays_open_and_ctrl_c_reverts_theme() {
    let mut session = make_test_session();
    let mut state = make_test_state();
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let kb = Keybindings::default_bindings();

    handle_slash_command("/theme", &mut session, &mut state, None, false).await;
    handle_non_approval_selector_key("down", &kb, &mut session, &mut state, &agent_tx).await;
    assert_eq!(state.theme.name, "light");

    for ch in ["z", "z", "z"] {
        handle_non_approval_selector_key(ch, &kb, &mut session, &mut state, &agent_tx).await;
    }
    handle_non_approval_selector_key("enter", &kb, &mut session, &mut state, &agent_tx).await;
    assert!(
        state.selector.is_some(),
        "Enter on empty filter must keep modal open"
    );
    assert!(state.theme_preview_restore.is_some());

    handle_non_approval_selector_key("ctrl+c", &kb, &mut session, &mut state, &agent_tx).await;
    assert!(state.selector.is_none());
    assert_eq!(state.theme.name, "dark");
    assert!(state.theme_preview_restore.is_none());

    // Custom Interrupt binding dismisses /settings cleanly
    let mut custom_kb = Keybindings::default_bindings();
    custom_kb.set_bindings(TuiAction::Interrupt, vec!["ctrl+q".to_string()]);
    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    assert!(state.selector.is_some());
    handle_non_approval_selector_key("ctrl+q", &custom_kb, &mut session, &mut state, &agent_tx).await;
    assert!(state.selector.is_none());
}

#[tokio::test]
async fn settings_non_mutating_open_extended_thinking_and_narrow_empty_filter() {
    let mut session = make_test_session();
    assert_eq!(session.settings.permission_mode, None);
    let mut state = make_test_state();
    let editor = Editor::new();

    // Header badge matches live Auto permission mode without mutating session.settings
    let initial = mehh_tui::strip_ansi(&render_interactive_frame(&state, &editor, &session, 80, 24).join("\n"));
    assert!(initial.contains("[auto]"), "{initial}");

    // Opening /settings + closing must not mutate session.settings
    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    state.selector = None;
    assert_eq!(session.settings.permission_mode, None);

    // Extended thinking level xhigh is preselected in /thinking and available in /settings
    session.set_thinking_level(Some(ThinkingLevel::Xhigh));
    handle_slash_command("/thinking", &mut session, &mut state, None, false).await;
    let t_sel = state.selector.take().unwrap();
    assert_eq!(t_sel.list.selected_item().unwrap().id, "xhigh");
    assert!(t_sel.list.selected_item().unwrap().label.contains("(current)"));
    handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    let s_sel = state.selector.as_ref().unwrap();
    let item = s_sel
        .settings_list
        .as_ref()
        .unwrap()
        .items
        .iter()
        .find(|i| i.id == "default_thinking_level")
        .unwrap();
    assert_eq!(item.current_value, "xhigh");
    assert!(item.values.contains(&"xhigh".to_string()));

    // Empty-filter recovery text does not clip key names at 60x20
    let kb = Keybindings::default_bindings();
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    for ch in ["z", "z", "z"] {
        handle_non_approval_selector_key(ch, &kb, &mut session, &mut state, &agent_tx).await;
    }
    let s_plain = mehh_tui::strip_ansi(&render_interactive_frame(&state, &editor, &session, 60, 20).join("\n"));
    assert!(s_plain.contains("Esc to clear"), "{s_plain}");
    assert!(s_plain.contains("Esc Clear filter"), "{s_plain}");

    handle_slash_command("/mode", &mut session, &mut state, None, false).await;
    for ch in ["z", "z", "z"] {
        handle_non_approval_selector_key(ch, &kb, &mut session, &mut state, &agent_tx).await;
    }
    let m_plain = mehh_tui::strip_ansi(&render_interactive_frame(&state, &editor, &session, 60, 20).join("\n"));
    assert!(m_plain.contains("Esc to clear"), "{m_plain}");
    assert!(m_plain.contains("Esc Clear filter"), "{m_plain}");

    // Custom theme preselection prefers session.settings.theme over fallback state.theme.name
    session.settings.theme = Some("custom-nord".to_string());
    state.theme = mehh_tui::Theme::dark();
    handle_slash_command("/theme", &mut session, &mut state, None, false).await;
    let theme_sel = state.selector.as_ref().unwrap();
    let selected_theme = theme_sel.list.selected_item().unwrap();
    assert_eq!(selected_theme.id, "custom-nord");
    assert!(
        selected_theme.label.contains("(current)"),
        "expected custom-nord marked current instead of dark: {}",
        selected_theme.label
    );
    state.selector = None;
    state.theme_preview_restore = None;
}

#[test]
fn autocomplete_handles_settings_aliases_multiline_midline_and_theme_stem() {
    let dir = temp_dir("mehh_ac_edge_cases");
    let session = make_test_session();
    let mut editor = Editor::new();

    // Alias queries in /settings <query> close popup so Enter submits directly
    for input in ["/settings compact", "/settings mode", "/settings chat"] {
        editor.set_text(input);
        super::update_autocomplete_with_context(&mut editor, input, &dir, Some(&session), Some("dark"));
        assert!(
            editor.autocomplete.is_none(),
            "expected no popup blocking submit for '{input}'"
        );
    }

    // Multiline and mid-line cursor do not trigger slash command completion
    editor.set_text("/th\nnext line");
    let ml_text = editor.text();
    super::update_autocomplete_with_context(&mut editor, &ml_text, &dir, Some(&session), Some("dark"));
    assert!(editor.autocomplete.is_none());

    editor.set_text("/theme");
    editor.cursor_col = 3;
    super::update_autocomplete_with_context(&mut editor, "/theme", &dir, Some(&session), Some("dark"));
    assert!(editor.autocomplete.is_none());

    // /logout options do not reuse /login OAuth instructions
    editor.set_text("/logout ");
    super::update_autocomplete_with_context(&mut editor, "/logout ", &dir, Some(&session), Some("dark"));
    let logout_ac = editor.autocomplete.as_ref().unwrap();
    assert!(logout_ac
        .items
        .iter()
        .all(|i| i.description.as_deref() == Some("Remove stored credential")));

    // Custom theme filename passed as current_theme marks itself as (current)
    editor.set_text("/theme ");
    super::update_autocomplete_with_context(&mut editor, "/theme ", &dir, Some(&session), Some("custom-nord"));
    let theme_ac = editor.autocomplete.as_ref().unwrap();
    assert!(theme_ac
        .items
        .iter()
        .any(|i| i.id == "custom-nord" && i.description.as_deref().unwrap_or("").contains("(current)")));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_tool_execution_start_summarizes_web_fetch_url() {
    let mut state = make_test_state();
    let mut args = serde_json::Map::new();
    args.insert(
        "url".to_string(),
        serde_json::Value::String("https://docs.rs/reqwest".to_string()),
    );
    apply_agent_event(
        &mut state,
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "wf_1".to_string(),
            tool_name: "web_fetch".to_string(),
            args,
        },
    );
    assert!(matches!(
        state.items.last(),
        Some(TranscriptItem::ToolCall {
            name,
            args_summary,
            ..
        }) if name == "web_fetch" && args_summary == "https://docs.rs/reqwest"
    ));
}
