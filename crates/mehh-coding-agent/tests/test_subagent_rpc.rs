use std::path::PathBuf;
use std::sync::Arc;

use mehh_ai::{Message, UserContent};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::rpc::run_rpc_mode;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use tokio::io::{AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Removes its directory on drop, including when an assertion panics.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn new_session(dir: &TempDir) -> AgentSession {
    AgentSession::new(
        dir.path.clone(),
        mehh_ai::faux_model(),
        Arc::new(mehh_ai::FauxProvider::new()),
        SessionManager::in_memory(dir.path.clone()),
        Settings::default(),
    )
}

/// Feeds every input line through RPC mode and parses each output JSONL line.
async fn run_commands(session: &mut AgentSession, input: &str) -> Vec<serde_json::Value> {
    let (writer, mut reader) = tokio::io::duplex(8192);
    run_rpc_mode(session, BufReader::new(input.as_bytes()), writer)
        .await
        .unwrap();

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_response(resp: &serde_json::Value, id: &str, command: &str) {
    assert_eq!(resp["id"], id, "unexpected response id: {resp}");
    assert_eq!(resp["type"], "response", "unexpected response type: {resp}");
    assert_eq!(resp["command"], command, "unexpected command: {resp}");
    assert!(
        resp.get("error").is_none(),
        "unexpected error response: {resp}"
    );
    assert_eq!(resp["success"], true, "expected success response: {resp}");
}

/// Asserts the data object contains only `ok: false`; RPC success is checked
/// by the caller, not here.
fn assert_ok_false(resp: &serde_json::Value) {
    let data = resp["data"]
        .as_object()
        .unwrap_or_else(|| panic!("response must contain a data object: {resp}"));
    assert_eq!(data.len(), 1, "data must contain only 'ok': {resp}");
    assert_eq!(
        data.get("ok"),
        Some(&serde_json::Value::Bool(false)),
        "expected ok:false: {resp}"
    );
}

#[tokio::test]
async fn rpc_can_list_steer_and_kill_a_child() {
    let dir = TempDir::new("mehh_rpc");
    let mut session = new_session(&dir);
    let child = session.subagent_control().register(
        "sub-x".into(),
        "research".into(),
        CancellationToken::new(),
    );

    let input = concat!(
        "{\"id\":\"1\",\"type\":\"subagent_list\"}\n",
        "{\"id\":\"2\",\"type\":\"subagent_steer\",",
        "\"agent_id\":\"sub-x\",\"message\":\"  focus  \"}\n",
        "{\"id\":\"3\",\"type\":\"subagent_kill\",\"agent_id\":\"sub-x\"}\n",
    );
    let responses = run_commands(&mut session, input).await;
    assert_eq!(responses.len(), 3, "expected one response per command");

    assert_response(&responses[0], "1", "subagent_list");
    let agents = responses[0]["data"]["agents"]
        .as_array()
        .expect("list response must contain data.agents");
    assert!(
        agents.iter().any(|a| a["agent_id"] == "sub-x"),
        "list response must include the child id: {agents:?}"
    );

    assert_response(&responses[1], "2", "subagent_steer");
    assert_eq!(
        responses[1]["data"]["ok"], true,
        "steer must report success"
    );

    assert_response(&responses[2], "3", "subagent_kill");
    assert_eq!(responses[2]["data"]["ok"], true, "kill must report success");

    let queued = child.steering_queue.lock().unwrap();
    assert_eq!(queued.len(), 1, "steer must queue exactly one message");
    match &queued[0] {
        Message::User(user) => assert_eq!(
            user.content,
            UserContent::Text("  focus  ".to_string()),
            "queued message must retain surrounding whitespace"
        ),
        other => panic!("expected a user message, got {other:?}"),
    }
    assert!(child.cancel.is_cancelled());
}

#[tokio::test]
async fn rpc_steer_rejects_absent_or_blank_message() {
    let dir = TempDir::new("mehh_rpc_empty");
    let mut session = new_session(&dir);
    let child = session.subagent_control().register(
        "sub-x".into(),
        "research".into(),
        CancellationToken::new(),
    );

    let input = concat!(
        "{\"id\":\"1\",\"type\":\"subagent_steer\",\"agent_id\":\"sub-x\"}\n",
        "{\"id\":\"2\",\"type\":\"subagent_steer\",",
        "\"agent_id\":\"sub-x\",\"message\":\"   \"}\n",
        "{\"id\":\"3\",\"type\":\"subagent_steer\",",
        "\"agent_id\":\"sub-x\",\"message\":\"\"}\n",
    );
    let responses = run_commands(&mut session, input).await;
    assert_eq!(responses.len(), 3, "expected one response per command");

    for (resp, id) in responses.iter().zip(["1", "2", "3"]) {
        assert_response(resp, id, "subagent_steer");
        assert_ok_false(resp);
    }
    assert_eq!(
        child.steering_queue.lock().unwrap().len(),
        0,
        "no message must be queued"
    );
}

#[tokio::test]
async fn rpc_steer_and_kill_reject_missing_or_unknown_agent_id() {
    let dir = TempDir::new("mehh_rpc_bad_id");
    let mut session = new_session(&dir);

    let input = concat!(
        "{\"id\":\"1\",\"type\":\"subagent_steer\",\"message\":\"hi\"}\n",
        "{\"id\":\"2\",\"type\":\"subagent_steer\",",
        "\"agent_id\":\"missing\",\"message\":\"hi\"}\n",
        "{\"id\":\"3\",\"type\":\"subagent_kill\"}\n",
        "{\"id\":\"4\",\"type\":\"subagent_kill\",\"agent_id\":\"missing\"}\n",
    );
    let responses = run_commands(&mut session, input).await;
    assert_eq!(responses.len(), 4, "expected one response per command");

    let expected = [
        ("1", "subagent_steer"),
        ("2", "subagent_steer"),
        ("3", "subagent_kill"),
        ("4", "subagent_kill"),
    ];
    for (resp, (id, command)) in responses.iter().zip(expected) {
        assert_response(resp, id, command);
        assert_ok_false(resp);
    }
}
