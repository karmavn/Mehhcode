use mehh_ai::{faux_model, FauxProvider};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::interactive::{handle_slash_command, SlashCommandOutcome, TranscriptItem};
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use mehh_coding_agent::tool_inspection::PermissionMode;
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
