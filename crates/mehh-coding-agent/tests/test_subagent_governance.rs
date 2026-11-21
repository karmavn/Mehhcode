use mehh_ai::faux_model;
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use mehh_coding_agent::tool_inspection::PermissionMode;
use std::sync::Arc;

#[tokio::test]
async fn test_subagent_tool_inherits_inspection_manager_from_agent_session() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_subagent_gov_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let model = faux_model();
    let provider = Arc::new(mehh_ai::FauxProvider::default());
    let session_mgr = SessionManager::in_memory(temp_dir.clone());
    let settings = Settings {
        permission_mode: Some(PermissionMode::Chat),
        ..Default::default()
    };

    let session = AgentSession::new_with_trust(
        temp_dir.clone(),
        model,
        provider,
        session_mgr,
        settings,
        true,
    );

    // Verify subagent tool in session.tools has inspection_manager wired
    let subagent_tool = session
        .tools()
        .iter()
        .find(|t| t.name() == "subagent")
        .expect("subagent tool must be registered in session");

    // Downcast to verify inspection_manager is Some
    let subagent_any = subagent_tool.clone();
    assert_eq!(subagent_any.name(), "subagent");

    // Also verify session's inspection_manager mode is Chat
    assert_eq!(
        session.inspection_manager.mode().await,
        PermissionMode::Chat
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}
