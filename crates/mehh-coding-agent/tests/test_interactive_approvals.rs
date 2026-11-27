use mehh_ai::{faux_model, FauxProvider};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::interactive::{
    handle_slash_command, render_interactive_frame, InteractiveState, PendingApproval, Selector,
    SelectorKind, SlashCommandOutcome, TranscriptItem,
};
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use mehh_coding_agent::tool_inspection::PermissionMode;
use mehh_tui::{strip_ansi, Editor, Keybindings, SelectItem, SelectList};
use std::sync::Arc;

#[tokio::test]
async fn test_mode_slash_command_switches_mode() {
    let temp_dir = std::env::temp_dir().join(format!("test_mode_slash_{}", uuid::Uuid::now_v7()));
    let session_mgr = SessionManager::in_memory(&temp_dir);
    let mut session = AgentSession::new(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        session_mgr,
        Settings::default(),
    );

    let mut state =
        mehh_coding_agent::interactive::InteractiveState::new(vec![], &session.settings);

    let outcome = handle_slash_command("/mode chat", &mut session, &mut state, None, false).await;

    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert_eq!(
        session.inspection_manager.mode().await,
        PermissionMode::Chat
    );
    assert_eq!(session.settings.permission_mode, Some(PermissionMode::Chat));

    let last_item = state.items.last().unwrap();
    if let TranscriptItem::System(msg) = last_item {
        assert!(msg.contains("Permission mode set to: Chat"));
    } else {
        panic!("Expected System transcript item");
    }
}

#[test]
fn test_render_interactive_frame_sanitizes_tool_approval_and_clamps_height() {
    let temp_dir =
        std::env::temp_dir().join(format!("test_int_sanitize_{}", uuid::Uuid::now_v7()));
    let session_mgr = SessionManager::in_memory(&temp_dir);
    let session = AgentSession::new(
        temp_dir,
        faux_model(),
        Arc::new(FauxProvider::new()),
        session_mgr,
        Settings::default(),
    );
    let mut state = InteractiveState::new(vec![], &session.settings);
    state.items.push(TranscriptItem::ToolCall {
        id: "tc_1".to_string(),
        name: "bash\x1b[2J".to_string(),
        status: "completed".to_string(),
        is_error: false,
        args_summary: "echo \x1b]0;pwn\x07hi".to_string(),
        output_preview: vec!["line \x1b[2J\u{202E}secret".to_string()],
        diff: None,
        duration_ms: Some(10),
    });
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    state.pending_approval = Some(PendingApproval {
        tool_name: "bash\u{202E}".to_string(),
        args: serde_json::Map::new(),
        reason: "Approve \x1b]52;c;AAAA\x07command".to_string(),
        preview: Some("echo \x1b[2Jsafe".to_string()),
        reply_tx,
    });
    state.selector = Some(Selector {
        kind: SelectorKind::ToolApproval,
        list: SelectList::new(vec![SelectItem::new("allow_once", "Allow once")], 4),
        settings_list: None,
    });

    let editor = Editor::new();
    let lines = render_interactive_frame(&state, &editor, &session, 80, 24);
    assert!(lines.len() <= 24);
    let joined = lines.join("\n");
    assert!(!joined.contains("\x1b[2J"));
    assert!(!joined.contains("\x1b]0;"));
    assert!(!joined.contains("\x1b]52;"));
    assert!(!joined.contains('\u{202E}'));
    assert!(strip_ansi(&joined).contains("safe"));

    let clamped = render_interactive_frame(&state, &editor, &session, 40, 5);
    assert!(clamped.len() <= 5);
}

#[tokio::test]
async fn test_non_builtin_skill_and_template_slash_commands_return_not_a_command() {
    let temp_dir = std::env::temp_dir().join(format!("test_int_skill_{}", uuid::Uuid::now_v7()));
    let session_mgr = SessionManager::in_memory(&temp_dir);
    let mut session = AgentSession::new(
        temp_dir,
        faux_model(),
        Arc::new(FauxProvider::new()),
        session_mgr,
        Settings::default(),
    );
    let mut state = InteractiveState::new(vec![], &session.settings);

    assert_eq!(
        handle_slash_command("/skill:deploy env=prod", &mut session, &mut state, None, false)
            .await,
        SlashCommandOutcome::NotACommand
    );
    assert_eq!(
        handle_slash_command("/review src/lib.rs", &mut session, &mut state, None, false).await,
        SlashCommandOutcome::NotACommand
    );
}

#[test]
fn test_editor_scrub_last_history_if_removes_login_key() {
    let mut editor = Editor::new();
    let kb = Keybindings::default_bindings();
    editor.set_text("hello");
    let _ = editor.handle_editor_key("enter", &kb);
    editor.set_text("/login anthropic sk-ant-secret");
    let _ = editor.handle_editor_key("enter", &kb);
    editor.scrub_last_history_if(|entry| {
        entry
            .trim()
            .strip_prefix("/login")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    });
    assert_eq!(editor.history, vec!["hello".to_string()]);
}
