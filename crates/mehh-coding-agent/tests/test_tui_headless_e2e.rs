use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mehh_agent::AgentEvent;
use mehh_ai::{FauxProvider, KNOWN_PROVIDERS, faux_assistant_message, faux_model};
use mehh_coding_agent::{
    AgentSession, InteractiveState, SelectorKind, SessionManager, Settings, SlashCommandOutcome,
    apply_agent_event, handle_slash_command, render_interactive_frame,
};
use mehh_tui::{Editor, MockTerminal, TuiRenderer, strip_ansi, visible_width};

static TUI_SEQ: AtomicU64 = AtomicU64::new(0);
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let seq = TUI_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mehh_tui_e2e_{prefix}_{}_{}",
            std::process::id(),
            seq
        ));
        fs::create_dir_all(&dir).expect("create temp tui dir");
        Self(dir)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct IsolatedEnvGuard {
    saved: Vec<(String, Option<OsString>)>,
}

impl IsolatedEnvGuard {
    fn isolate(home: &PathBuf) -> Self {
        fs::create_dir_all(home).expect("create isolated home dir");
        let mut keys: Vec<String> = vec![
            "HOME".to_string(),
            "USERPROFILE".to_string(),
            "GOOGLE_API_KEY".to_string(),
            "AWS_BEARER_TOKEN_BEDROCK".to_string(),
            "API_KEY".to_string(),
        ];
        for spec in KNOWN_PROVIDERS {
            keys.push(spec.env_var.to_string());
        }

        let mut saved = Vec::with_capacity(keys.len());
        for key in keys {
            saved.push((key.clone(), std::env::var_os(&key)));
            unsafe {
                if key == "HOME" || key == "USERPROFILE" {
                    std::env::set_var(&key, home);
                } else {
                    std::env::remove_var(&key);
                }
            }
        }
        Self { saved }
    }
}

impl Drop for IsolatedEnvGuard {
    fn drop(&mut self) {
        for (key, val) in self.saved.drain(..) {
            unsafe {
                match val {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

fn new_test_session(dir: &PathBuf) -> AgentSession {
    AgentSession::new(
        dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir),
        Settings::default(),
    )
}

fn assert_frame_invariants(lines: &[String], width: usize, height: usize) {
    assert_eq!(
        lines.len(),
        height,
        "frame line count {} must equal terminal height {height}",
        lines.len()
    );
    for (idx, line) in lines.iter().enumerate() {
        let clean = strip_ansi(line);
        let w = visible_width(&clean);
        assert!(
            w <= width,
            "row {idx} exceeds viewport width {width} (got {w}): '{clean}'"
        );
    }
}

fn render_to_mock(
    state: &InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    renderer: &mut TuiRenderer,
    term: &mut MockTerminal,
) -> String {
    let (cols, rows) = (term.cols as usize, term.rows as usize);
    let lines = render_interactive_frame(state, editor, session, cols, rows);
    assert_frame_invariants(&lines, cols, rows);
    renderer
        .render_frame(term, lines.clone(), false)
        .expect("MockTerminal render_frame must succeed");
    lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn tui_renders_startup_and_conversation_turn_within_bounds() {
    let _env_lock = ENV_LOCK.lock().await;
    let ws = TempWorkspace::new("startup_turn");
    let _env = IsolatedEnvGuard::isolate(&ws.0.join("home"));
    let session = new_test_session(&ws.0);
    let mut state = InteractiveState::new(Vec::new(), &Settings::default());
    let editor = Editor::new().with_prefix("> ");
    let mut renderer = TuiRenderer::new();
    let mut term = MockTerminal::new(90, 28);

    let welcome = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(welcome.contains("mehh"));
    assert!(welcome.contains("Faux Model"));

    let reply = faux_assistant_message("Đây là phản hồi tiếng Việt từ faux model.");
    apply_agent_event(&mut state, &AgentEvent::AgentStart);
    apply_agent_event(
        &mut state,
        &AgentEvent::MessageStart {
            message: mehh_ai::Message::Assistant(reply.clone()),
        },
    );
    apply_agent_event(
        &mut state,
        &AgentEvent::MessageEnd {
            message: mehh_ai::Message::Assistant(reply),
        },
    );
    apply_agent_event(
        &mut state,
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );

    let after = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(after.contains("phản hồi tiếng Việt"));
    assert!(!term.full_output().is_empty());
}

#[tokio::test]
async fn tui_slash_commands_update_state_and_render_cleanly() {
    let _env_lock = ENV_LOCK.lock().await;
    let ws = TempWorkspace::new("slash_cmds");
    let _env = IsolatedEnvGuard::isolate(&ws.0.join("home"));

    let mut session = new_test_session(&ws.0);
    let mut state = InteractiveState::new(Vec::new(), &Settings::default());
    let editor = Editor::new().with_prefix("> ");
    let mut renderer = TuiRenderer::new();
    let mut term = MockTerminal::new(100, 32);

    let help_out = handle_slash_command("/help", &mut session, &mut state, None, false).await;
    assert!(matches!(help_out, SlashCommandOutcome::Handled));
    let help_frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(
        help_frame.contains("/help") && help_frame.contains("Keybindings:"),
        "expected /help output in frame:\n{help_frame}"
    );

    let tools_out = handle_slash_command("/tools", &mut session, &mut state, None, false).await;
    assert!(matches!(tools_out, SlashCommandOutcome::Handled));
    let tools_frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(
        tools_frame.contains("Active tools") && tools_frame.contains("read"),
        "expected /tools output in frame:\n{tools_frame}"
    );

    let settings_out =
        handle_slash_command("/settings", &mut session, &mut state, None, false).await;
    assert!(matches!(settings_out, SlashCommandOutcome::Handled));
    assert_eq!(
        state.selector.as_ref().map(|s| s.kind),
        Some(SelectorKind::Settings)
    );
    let settings_frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(
        settings_frame.contains("Settings"),
        "expected Settings modal in frame:\n{settings_frame}"
    );
    state.selector = None;

    let mode_out = handle_slash_command("/mode", &mut session, &mut state, None, false).await;
    assert!(matches!(mode_out, SlashCommandOutcome::Handled));
    assert_eq!(
        state.selector.as_ref().map(|s| s.kind),
        Some(SelectorKind::Mode)
    );
    let mode_frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(
        mode_frame.contains("smart_approve") || mode_frame.contains("auto"),
        "expected mode picker options in frame:\n{mode_frame}"
    );
    state.selector = None;

    // With isolated empty HOME and cleared provider env vars, /model must not leak host credentials.
    let model_out = handle_slash_command("/model", &mut session, &mut state, None, false).await;
    assert!(matches!(model_out, SlashCommandOutcome::Handled));
    assert!(
        state.selector.is_some(),
        "/model should open a selector (Model or Connect redirect)"
    );
    assert!(
        state.items.iter().any(|item| matches!(
            item,
            mehh_coding_agent::TranscriptItem::System(msg)
                if msg.contains("No models available yet") || msg.contains("No provider connected yet")
        )),
        "expected disconnected-provider isolation on /model"
    );
    let model_frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(
        !model_frame.contains("✓ connected")
            && (model_frame.contains("Connect") || model_frame.contains("Anthropic")),
        "expected clean connect selector modal without host credentials on /model:\n{model_frame}"
    );
    state.selector = None;

    let clear_outcome =
        handle_slash_command("/clear", &mut session, &mut state, None, false).await;
    assert!(matches!(clear_outcome, SlashCommandOutcome::Handled));
    assert!(state.items.is_empty(), "/clear must empty transcript items");
    let cleared = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(cleared.contains("mehh"));
}

#[tokio::test]
async fn tui_handles_resize_with_unicode_and_code_blocks() {
    let _env_lock = ENV_LOCK.lock().await;
    let ws = TempWorkspace::new("resize_unicode");
    let _env = IsolatedEnvGuard::isolate(&ws.0.join("home"));
    let session = new_test_session(&ws.0);
    let mut state = InteractiveState::new(Vec::new(), &Settings::default());
    let editor = Editor::new().with_prefix("> ");
    let mut renderer = TuiRenderer::new();
    let mut term = MockTerminal::new(120, 40);

    state.items.push(mehh_coding_agent::TranscriptItem::User(
        "Kiểm tra xuống dòng với tiếng Việt có dấu và ký tự rộng: 你好世界 🦀".to_string(),
    ));
    state
        .items
        .push(mehh_coding_agent::TranscriptItem::Assistant {
            thinking: "Phân tích bố cục...".to_string(),
            text: "Đoạn mã mẫu:\n```rust\nfn very_long_function_name_for_wrapping_check(param_one: usize, param_two: &str) -> usize { param_one + param_two.len() }\n```".to_string(),
        });

    for (cols, rows) in [(120u16, 40u16), (80, 24), (50, 16), (30, 10)] {
        term.cols = cols;
        term.rows = rows;
        renderer.reset();
        let frame = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
        assert!(!frame.is_empty(), "frame empty at {cols}x{rows}");
    }
}

#[tokio::test]
async fn tui_renders_tool_execution_lifecycle_without_overflow() {
    let _env_lock = ENV_LOCK.lock().await;
    let ws = TempWorkspace::new("tool_lifecycle");
    let _env = IsolatedEnvGuard::isolate(&ws.0.join("home"));
    let session = new_test_session(&ws.0);
    let mut state = InteractiveState::new(Vec::new(), &Settings::default());
    let editor = Editor::new().with_prefix("> ");
    let mut renderer = TuiRenderer::new();
    let mut term = MockTerminal::new(84, 24);

    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String("crates/mehh-cli/src/main.rs".to_string()),
    );

    apply_agent_event(
        &mut state,
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "call_e2e_1".to_string(),
            tool_name: "read".to_string(),
            args: args.clone(),
        },
    );
    let running = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(running.contains("read"));

    apply_agent_event(
        &mut state,
        &AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call_e2e_1".to_string(),
            tool_name: "read".to_string(),
            args,
            partial_result: mehh_agent::AgentToolResult::text("line 1\nline 2\nline 3"),
        },
    );
    let _mid = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);

    apply_agent_event(
        &mut state,
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_e2e_1".to_string(),
            tool_name: "read".to_string(),
            result: mehh_agent::AgentToolResult::text("line 1\nline 2\nline 3\nline 4"),
            is_error: false,
            duration_ms: Some(12),
        },
    );
    let done = render_to_mock(&state, &editor, &session, &mut renderer, &mut term);
    assert!(done.contains("read"));
}
