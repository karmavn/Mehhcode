use std::sync::Arc;
use tokio::io::{AsyncReadExt, BufReader};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::rpc::run_rpc_mode;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rpc_can_list_steer_and_kill_a_child() {
    let dir = std::env::temp_dir().join(format!("mehh_rpc_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::new(
        dir.clone(), mehh_ai::faux_model(), Arc::new(mehh_ai::FauxProvider::new()),
        SessionManager::in_memory(&dir), Settings::default(),
    );
    let child = session.subagent_control().register("sub-x".into(), "research".into(), CancellationToken::new());

    let input = b"{\"id\":\"1\",\"type\":\"subagent_list\"}\n{\"id\":\"2\",\"type\":\"subagent_steer\",\"agent_id\":\"sub-x\",\"message\":\"focus\"}\n{\"id\":\"3\",\"type\":\"subagent_kill\",\"agent_id\":\"sub-x\"}\n";
    let (writer, mut reader) = tokio::io::duplex(8192);
    run_rpc_mode(&mut session, BufReader::new(&input[..]), writer).await.unwrap();
    drop(session);

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("sub-x"), "list response must include the child id");
    assert!(text.contains("\"ok\":true"), "steer and kill must report success");
    assert_eq!(child.steering_queue.lock().unwrap().len(), 1);
    assert!(child.cancel.is_cancelled());
    std::fs::remove_dir_all(dir).unwrap();
}
