use std::sync::Arc;
use mehh_ai::{faux_model, AssistantMessage, FauxProvider, ToolCall};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use mehh_agent::{AgentContext, BeforeToolCallContext};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_agent_session_wires_before_tool_call() {
    let temp_dir = std::env::temp_dir().join(format!("agent_session_gov_{}", uuid::Uuid::now_v7()));
    let session_mgr = SessionManager::in_memory(&temp_dir);
    let session = AgentSession::new(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        session_mgr,
        Settings::default(),
    );

    let (_msg, _hist, config) = session.prepare_prompt("test prompt");
    assert!(config.before_tool_call.is_some(), "before_tool_call hook must be wired");

    let before_hook = config.before_tool_call.unwrap();

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("rm -rf /"));
    let before_ctx = BeforeToolCallContext {
        assistant_message: AssistantMessage {
            content: vec![],
            stop_reason: mehh_ai::StopReason::ToolUse,
            usage: mehh_ai::Usage::default(),
            timestamp: 0,
            model: "faux".to_string(),
            provider: "faux".to_string(),
            api: "faux".to_string(),
            response_id: None,
            error_message: None,
        },
        tool_call: ToolCall {
            id: "call_rm".to_string(),
            name: "bash".to_string(),
            arguments: args.clone(),
            arguments_error: None,
            thought_signature: None,
        },
        args,
        context: AgentContext {
            messages: vec![],
            tools: Some(session.tools.clone()),
        },
    };

    let result = before_hook(before_ctx, CancellationToken::new()).await;
    assert!(result.block, "Dangerous command must be blocked");
    assert!(result.reason.is_some());
}

#[tokio::test]
async fn test_headless_deny_handler_fails_closed() {
    let temp_dir = std::env::temp_dir().join(format!("agent_session_gov_h_{}", uuid::Uuid::now_v7()));
    let session_mgr = SessionManager::in_memory(&temp_dir);
    let mut session = AgentSession::new(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        session_mgr,
        Settings {
            permission_mode: Some(mehh_coding_agent::tool_inspection::PermissionMode::SmartApprove),
            ..Default::default()
        },
    );
    session.approval_handler = Some(Arc::new(mehh_coding_agent::tool_inspection::HeadlessDenyHandler));

    let (_msg, _hist, config) = session.prepare_prompt("test prompt");
    let before_hook = config.before_tool_call.unwrap();

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    let before_ctx = BeforeToolCallContext {
        assistant_message: AssistantMessage {
            content: vec![],
            stop_reason: mehh_ai::StopReason::ToolUse,
            usage: mehh_ai::Usage::default(),
            timestamp: 0,
            model: "faux".to_string(),
            provider: "faux".to_string(),
            api: "faux".to_string(),
            response_id: None,
            error_message: None,
        },
        tool_call: ToolCall {
            id: "call_edit".to_string(),
            name: "edit".to_string(),
            arguments: args.clone(),
            arguments_error: None,
            thought_signature: None,
        },
        args,
        context: AgentContext {
            messages: vec![],
            tools: Some(session.tools.clone()),
        },
    };

    let result = before_hook(before_ctx, CancellationToken::new()).await;
    assert!(result.block, "Mutating tool must be blocked in headless mode when unapproved");
}

#[test]
fn test_workspace_settings_escalation_guard() {
    let temp_dir = std::env::temp_dir().join(format!("agent_session_esc_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let global_path = temp_dir.join("global_settings.json");
    let local_path = temp_dir.join("local_settings.json");

    std::fs::write(&global_path, "{}").unwrap();
    std::fs::write(&local_path, r#"{"permissionMode": "auto"}"#).unwrap();

    let loaded = mehh_coding_agent::settings::SettingsManager::try_load_from_paths(
        &global_path,
        &local_path,
        true,
    ).unwrap();

    assert_eq!(
        loaded.permission_mode,
        None,
        "Workspace setting specifying auto must be neutralized to None to prevent escalation or downgrades"
    );
}
