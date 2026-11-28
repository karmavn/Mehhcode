use std::sync::Mutex as StdMutex;
use crate::subagent_control::SubagentControl;
use mehh_ai::{now_ms, UserContent, UserMessage};

use super::*;
use mehh_agent::AgentTool;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use mehh_ai::{
    create_event_stream, AssistantContentBlock, AssistantEventStream,
    AssistantMessage, AssistantMessageEvent, Context, FauxProvider, LlmProvider, Model,
    ModelCost, StopReason, StreamOptions, TextContent, Usage,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Model provider that blocks until the test releases it, so commands can
/// be sent while a prompt is demonstrably still in flight.
struct GatedProvider {
    started: tokio::sync::watch::Sender<bool>,
    release: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl LlmProvider for GatedProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: &StreamOptions,
        cancel: CancellationToken,
    ) -> AssistantEventStream {
        let (sender, stream) = create_event_stream();
        let started = self.started.clone();
        let release = self.release.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let _ = started.send(true);
            while !release.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
                if cancel.is_cancelled() {
                    return;
                }
            }
            let msg = AssistantMessage {
                content: vec![AssistantContentBlock::Text(TextContent {
                    text: "done".into(),
                    text_signature: None,
                })],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now_ms(),
            };
            sender.push(AssistantMessageEvent::Start {
                partial: msg.clone(),
            });
            sender.push(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: msg,
            });
        });
        stream
    }
}

use crate::session::SessionManager;
use crate::settings::Settings;

fn make_test_model() -> Model {
    Model {
        id: "faux-test".to_string(),
        name: "Faux Test".to_string(),
        api: "faux".to_string(),
        provider: "faux".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        input: vec!["text".to_string()],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 4096,
        headers: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_run_rpc_mode_lifecycle() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_test_{}", Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let faux = FauxProvider::new();
    // Response for prompt
    faux.add_response(AssistantMessage {
        content: vec![AssistantContentBlock::Text(TextContent {
            text: "Hello from RPC".to_string(),
            text_signature: None,
        })],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-test".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    });
    // Response for compaction
    faux.add_response(AssistantMessage {
        content: vec![AssistantContentBlock::Text(TextContent {
            text: "## Goal\nTest compaction via RPC".to_string(),
            text_signature: None,
        })],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-test".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    });

    let session_mgr = SessionManager::in_memory(temp_dir.clone());
    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(faux),
        session_mgr,
        Settings::default(),
    );

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);

    let mut client_writer = client_write;
    let mut client_reader = BufReader::new(client_read);

    // Spawn RPC runner
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    // 1. Send get_state
    let cmd_get_state = serde_json::json!({
        "id": "req_1",
        "type": "get_state"
    });
    client_writer
        .write_all(format!("{cmd_get_state}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    let mut resp_line = String::new();
    client_reader.read_line(&mut resp_line).await.unwrap();
    let resp_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(resp_json["id"], "req_1");
    assert_eq!(resp_json["command"], "get_state");
    assert_eq!(resp_json["success"], true);

    // 2. Send prompt
    let cmd_prompt = serde_json::json!({
        "id": "req_2",
        "type": "prompt",
        "message": "Hello"
    });
    client_writer
        .write_all(format!("{cmd_prompt}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    // Read until prompt response is received
    let mut prompt_succeeded = false;
    loop {
        resp_line.clear();
        let n = client_reader.read_line(&mut resp_line).await.unwrap();
        if n == 0 {
            break;
        }
        let ev: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
        if ev.get("type").and_then(|v| v.as_str()) == Some("response")
            && ev.get("command").and_then(|v| v.as_str()) == Some("prompt")
        {
            assert_eq!(ev["id"], "req_2");
            assert_eq!(ev["success"], true);
            prompt_succeeded = true;
            break;
        }
    }
    assert!(prompt_succeeded);

    // 3. Send get_messages
    let cmd_get_messages = serde_json::json!({
        "id": "req_3",
        "type": "get_messages"
    });
    client_writer
        .write_all(format!("{cmd_get_messages}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    resp_line.clear();
    client_reader.read_line(&mut resp_line).await.unwrap();
    let msgs_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(msgs_json["id"], "req_3");
    assert_eq!(msgs_json["command"], "get_messages");
    assert_eq!(msgs_json["success"], true);
    let msgs = msgs_json["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2); // User + Assistant

    // 4. Send compact
    let cmd_compact = serde_json::json!({
        "id": "req_4",
        "type": "compact"
    });
    client_writer
        .write_all(format!("{cmd_compact}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    resp_line.clear();
    client_reader.read_line(&mut resp_line).await.unwrap();
    let comp_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(comp_json["id"], "req_4");
    assert_eq!(comp_json["command"], "compact");
    assert_eq!(comp_json["success"], true);
    let summary = comp_json["data"]["summary"]
        .as_str()
        .expect("compact must return a non-null summary string");
    assert!(
        summary.contains("## Goal\nTest compaction via RPC"),
        "unexpected compaction summary: {summary}"
    );

    // Close client writer to shut down RPC cleanly
    drop(client_writer);
    let res = rpc_handle.await.unwrap();
    assert!(res.is_ok());

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn subagent_kill_without_agent_id_reports_false() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_kill_{}", Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let release = Arc::new(AtomicBool::new(false));
    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(GatedProvider {
            started: started_tx,
            release: release.clone(),
        }),
        SessionManager::in_memory(temp_dir.clone()),
        Settings::default(),
    );

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut client_writer = client_write;
    let mut client_reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    client_writer
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
        .await
        .unwrap();
    client_writer.flush().await.unwrap();
    while !*started_rx.borrow() {
        started_rx.changed().await.unwrap();
    }

    client_writer
        .write_all(b"{\"id\":\"k1\",\"type\":\"subagent_kill\"}\n")
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    let answered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            if client_reader.read_line(&mut line).await.unwrap() == 0 {
                return None;
            }
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == "k1" {
                return Some(value);
            }
        }
    })
    .await
    .expect("subagent_kill must be answered")
    .expect("RPC stream closed unexpectedly");
    assert_eq!(
        answered["data"]["ok"], false,
        "a kill request without agent_id must not report success"
    );

    release.store(true, Ordering::SeqCst);
    drop(client_writer);
    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn subagent_list_responds_while_prompt_is_still_running() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_busy_{}", Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let release = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(GatedProvider {
        started: started_tx,
        release: release.clone(),
    });
    let session_mgr = SessionManager::in_memory(temp_dir.clone());
    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        provider,
        session_mgr,
        Settings::default(),
    );

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut client_writer = client_write;
    let mut client_reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    client_writer
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
        .await
        .unwrap();
    client_writer.flush().await.unwrap();
    while !*started_rx.borrow() {
        started_rx.changed().await.unwrap();
    }

    client_writer
        .write_all(b"{\"id\":\"q1\",\"type\":\"subagent_list\"}\n")
        .await
        .unwrap();
    client_writer.flush().await.unwrap();

    let answered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            if client_reader.read_line(&mut line).await.unwrap() == 0 {
                return None;
            }
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == "q1" {
                return Some(value);
            }
        }
    })
    .await
    .expect("subagent_list must be answered while the prompt is in flight")
    .expect("RPC stream closed unexpectedly");
    assert_eq!(answered["command"], "subagent_list");
    assert_eq!(answered["success"], true);

    release.store(true, Ordering::SeqCst);
    drop(client_writer);
    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn test_rpc_rejects_unknown_model_and_reports_provider_error() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_err_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let faux = Arc::new(FauxProvider::with_responses(vec![
        mehh_ai::AssistantMessage {
            content: vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                text: String::new(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: None,
            usage: mehh_ai::Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("upstream rpc error".to_string()),
            timestamp: mehh_ai::now_ms(),
        },
    ]));

    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        faux,
        crate::session::SessionManager::in_memory(&temp_dir),
        crate::settings::Settings::default(),
    );

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);

    let mut client_write = client_write;
    let mut reader = tokio::io::BufReader::new(client_read);

    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(
            &mut session,
            tokio::io::BufReader::new(server_read),
            server_write,
        )
        .await
    });

    let mut line = String::new();

    // 1. Unknown model fails with success: false
    let bad_model = serde_json::json!({
        "id": "m1",
        "type": "set_model",
        "provider": "openai",
        "modelId": "nonexistent-model-xyz"
    });
    client_write
        .write_all(format!("{bad_model}\n").as_bytes())
        .await
        .unwrap();
    client_write.flush().await.unwrap();

    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let m_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(m_resp["id"], "m1");
    assert_eq!(m_resp["success"], false);

    // 2. Prompt returning StopReason::Error fails with success: false
    let bad_prompt = serde_json::json!({
        "id": "p1",
        "type": "prompt",
        "message": "trigger failure"
    });
    client_write
        .write_all(format!("{bad_prompt}\n").as_bytes())
        .await
        .unwrap();
    client_write.flush().await.unwrap();

    let mut prompt_err_seen = false;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            break;
        }
        let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
        if ev.get("type").and_then(|v| v.as_str()) == Some("response")
            && ev.get("command").and_then(|v| v.as_str()) == Some("prompt")
        {
            assert_eq!(ev["id"], "p1");
            assert_eq!(ev["success"], false);
            assert!(ev["error"]
                .as_str()
                .unwrap_or("")
                .contains("upstream rpc error"));
            prompt_err_seen = true;
            break;
        }
    }
    assert!(prompt_err_seen);

    drop(client_write);
    assert!(rpc_handle.await.unwrap().is_ok());
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn rpc_queue_session_tree_and_bash_commands() {
    let _env_lock = crate::ENV_MUTEX.lock().await;
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_commands_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let mut session = AgentSession::new(
        temp_dir.clone(),
        mehh_ai::faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings::default(),
    );
    // This test asserts RPC plumbing, not the global permission store. Other
    // tests mutate USERPROFILE/HOME in parallel, so make the governance
    // chain hermetic here instead of inheriting a possible deny-all store.
    session.inspection_manager = Arc::new(
        crate::tool_inspection::ToolInspectionManager::new_with_store(
            crate::tool_inspection::PermissionMode::Auto,
            temp_dir.clone(),
            crate::permissions::PermissionStore::new(Vec::new(), None),
        ),
    );

    let (client_write, server_read) = tokio::io::duplex(64 * 1024);
    let (server_write, client_read) = tokio::io::duplex(64 * 1024);

    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);

    async fn send_and_expect(
        writer: &mut tokio::io::DuplexStream,
        reader: &mut BufReader<tokio::io::DuplexStream>,
        cmd: serde_json::Value,
        expected_cmd: &str,
    ) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        writer
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "EOF before receiving response for {expected_cmd}");
            let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
            if ev.get("type").and_then(|v| v.as_str()) == Some("response")
                && ev.get("command").and_then(|v| v.as_str()) == Some(expected_cmd)
            {
                assert_eq!(
                    ev["success"], true,
                    "Expected success for {expected_cmd}: {:?}",
                    ev
                );
                return ev;
            }
        }
    }

    // 1. steer
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "1", "type": "steer", "message": "steering msg" }),
        "steer",
    )
    .await;

    // 2. follow_up
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "2", "type": "follow_up", "message": "follow up msg" }),
        "follow_up",
    )
    .await;

    // 3. clear_queue
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "3", "type": "clear_queue" }),
        "clear_queue",
    )
    .await;

    // 4. cycle_model
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "4", "type": "cycle_model" }),
        "cycle_model",
    )
    .await;

    // 5. set_session_name
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "5", "type": "set_session_name", "name": "RPC Named Session" }),
        "set_session_name",
    ).await;

    // 6. get_tree
    let tree_resp = send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "6", "type": "get_tree" }),
        "get_tree",
    )
    .await;
    assert!(tree_resp["data"]["tree"].is_array());

    // 7. clone
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "7", "type": "clone" }),
        "clone",
    )
    .await;

    // 8. fork
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "8", "type": "fork" }),
        "fork",
    )
    .await;

    // 9. bash
    let echo_cmd = if cfg!(windows) {
        "Write-Output 'hello_rpc'"
    } else {
        "echo 'hello_rpc'"
    };
    let bash_resp = send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "9", "type": "bash", "command": echo_cmd }),
        "bash",
    )
    .await;
    assert!(bash_resp["data"]["output"]
        .as_str()
        .unwrap()
        .contains("hello_rpc"));

    // 9b. bash non-zero exit code resolves with isError: true instead of RPC error
    let fail_cmd = "exit 1";
    let fail_resp = send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "9b", "type": "bash", "command": fail_cmd }),
        "bash",
    )
    .await;
    assert_eq!(fail_resp["data"]["isError"], true);
    assert!(fail_resp["data"]["output"]
        .as_str()
        .unwrap()
        .contains("Command exited with code 1"));

    // 10. abort_bash
    send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "10", "type": "abort_bash" }),
        "abort_bash",
    )
    .await;

    // 11. switch_session
    let session_file = temp_dir.join("switched_session.jsonl");
    let sw_mgr = SessionManager::open_file(&session_file, &temp_dir).unwrap();
    let switch_resp = send_and_expect(
        &mut writer,
        &mut reader,
        serde_json::json!({ "id": "11", "type": "switch_session", "sessionPath": session_file.to_string_lossy() }),
        "switch_session",
    ).await;
    assert_eq!(switch_resp["data"]["sessionId"], sw_mgr.session_id());

    drop(writer);
    assert!(rpc_handle.await.unwrap().is_ok());
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn rpc_bash_obeys_tool_inspection_deny() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_bash_deny_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let mut session = AgentSession::new(
        temp_dir.clone(),
        mehh_ai::faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings {
            permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
            ..Settings::default()
        },
    );

    let (client_write, server_read) = tokio::io::duplex(64 * 1024);
    let (server_write, client_read) = tokio::io::duplex(64 * 1024);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);
    writer
        .write_all(b"{\"id\":\"deny_1\",\"type\":\"bash\",\"command\":\"rm -rf /\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let mut response = None;
    for _ in 0..8 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "EOF before bash response");
        let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
        if ev.get("type").and_then(|v| v.as_str()) == Some("response")
            && ev.get("command").and_then(|v| v.as_str()) == Some("bash")
            && ev.get("id").and_then(|v| v.as_str()) == Some("deny_1")
        {
            response = Some(ev);
            break;
        }
    }
    let response = response.expect("bash response for deny_1");
    assert_eq!(
        response["success"], false,
        "dangerous RPC bash must be blocked: {response}"
    );
    let error = response["error"].as_str().unwrap_or("");
    assert!(
        error.contains("security policy"),
        "unexpected block reason: {error}"
    );

    drop(writer);
    assert!(rpc_handle.await.unwrap().is_ok());
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn rpc_switch_session_refuses_missing_path_and_clears_state_on_reset() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_switch_err_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let existing_file = temp_dir.join("existing.jsonl");
    let existing_mgr = SessionManager::open_file(&existing_file, &temp_dir).unwrap();
    let existing_sid = existing_mgr.session_id().to_string();
    let missing_file = temp_dir.join("missing.jsonl");

    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings::default(),
    );
    session.steer("queued steer");
    session.follow_up("queued follow");
    session.todo_state.write().await.add("old todo".into());

    let (client_write, server_read) = tokio::io::duplex(16 * 1024);
    let (server_write, client_read) = tokio::io::duplex(16 * 1024);
    let (tx_done, rx_done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let res = run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await;
        let _ = tx_done.send((
            res.is_ok(),
            session.pending_steering_count(),
            session.pending_follow_up_count(),
            session.todo_state.read().await.items.len(),
        ));
    });

    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);

    // Missing path must fail without creating file or clearing existing state.
    let missing_cmd = serde_json::json!({
        "id": "sw_missing",
        "type": "switch_session",
        "sessionPath": missing_file.to_string_lossy(),
    });
    writer
        .write_all(format!("{missing_cmd}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], "sw_missing");
    assert_eq!(resp["success"], false);
    assert!(!missing_file.exists());

    // Switching to an existing file clears todo_state and queues.
    let ok_cmd = serde_json::json!({
        "id": "sw_ok",
        "type": "switch_session",
        "sessionPath": existing_file.to_string_lossy(),
    });
    writer
        .write_all(format!("{ok_cmd}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp_ok: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp_ok["id"], "sw_ok");
    assert_eq!(resp_ok["success"], true);
    assert_eq!(resp_ok["data"]["sessionId"], existing_sid);

    // Re-populate queues/todo then call new_session, verifying it clears them too.
    writer
        .write_all(b"{\"id\":\"st1\",\"type\":\"steer\",\"message\":\"s\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    writer
        .write_all(b"{\"id\":\"ns1\",\"type\":\"new_session\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp_ns: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp_ns["success"], true);

    drop(writer);
    let (ok, steer_cnt, follow_cnt, todo_cnt) = rx_done.await.unwrap();
    assert!(ok);
    assert_eq!(steer_cnt, 0);
    assert_eq!(follow_cnt, 0);
    assert_eq!(todo_cnt, 0);
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn rpc_abort_preserves_cancel_token_in_slot() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_abort_slot_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let release = Arc::new(AtomicBool::new(false));
    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(GatedProvider {
            started: started_tx,
            release: release.clone(),
        }),
        SessionManager::in_memory(temp_dir.clone()),
        Settings::default(),
    );
    let cancel_slot = session.active_cancel_slot();

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    writer
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    while !*started_rx.borrow() {
        started_rx.changed().await.unwrap();
    }

    writer
        .write_all(b"{\"id\":\"a1\",\"type\":\"abort\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let abort_resp = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["id"] == "a1" {
                return Some(v);
            }
        }
    })
    .await
    .expect("abort response must arrive")
    .expect("EOF");
    assert_eq!(abort_resp["success"], true);

    // Token stays in slot as cancelled rather than being .take()n.
    {
        let guard = cancel_slot.lock().unwrap();
        if let Some(tok) = guard.as_ref() {
            assert!(tok.is_cancelled());
        }
    }

    release.store(true, Ordering::SeqCst);
    drop(writer);
    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn handle_concurrent_abort_cancels_in_place_without_taking_token() {
    let (client_read, server_write) = tokio::io::duplex(4096);
    let output = RpcOutput::new(server_write);
    let sub_control = Arc::new(SubagentControl::new(1));
    let steering = Arc::new(StdMutex::new(VecDeque::new()));
    let follow = Arc::new(StdMutex::new(VecDeque::new()));
    let tok = CancellationToken::new();
    let cancel_slot = Arc::new(StdMutex::new(Some(tok.clone())));
    let bash_cancel_slot = Arc::new(StdMutex::new(None));

    let cmd = serde_json::json!({ "id": "ab1", "type": "abort" });
    let handled = handle_concurrent_command(
        &cmd,
        &output,
        &sub_control,
        &steering,
        &follow,
        &cancel_slot,
        &bash_cancel_slot,
    )
    .await
    .unwrap();
    assert!(handled);
    assert!(tok.is_cancelled());
    assert!(
        cancel_slot.lock().unwrap().is_some(),
        "abort must cancel in place instead of taking the token"
    );

    // Repeat abort still succeeds cleanly.
    let cmd2 = serde_json::json!({ "id": "ab2", "type": "abort" });
    assert!(handle_concurrent_command(
        &cmd2,
        &output,
        &sub_control,
        &steering,
        &follow,
        &cancel_slot,
        &bash_cancel_slot,
    )
    .await
    .unwrap());
    drop(client_read);
}

#[tokio::test]
async fn rpc_bash_is_rejected_when_bash_tool_is_disabled() {
    let dir = crate::TempTestDir::new("rpc_bash_disabled");
    let faux = Arc::new(FauxProvider::new());
    let mut session = AgentSession::new(
        dir.path().to_path_buf(),
        mehh_ai::faux_model(),
        faux,
        SessionManager::in_memory(dir.path()),
        Settings::default(),
    );
    // Disable bash tool on this session.
    let filtered: Vec<_> = session
        .tools()
        .iter()
        .filter(|t| t.name() != "bash")
        .cloned()
        .collect();
    session.set_tools(filtered);

    let (client_read, server_write) = tokio::io::duplex(4096);
    let (server_read, mut client_write) = tokio::io::duplex(4096);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write)
            .await
            .unwrap();
    });

    client_write
        .write_all(b"{\"id\":\"b1\",\"type\":\"bash\",\"command\":\"echo hi\"}\n")
        .await
        .unwrap();
    client_write.flush().await.unwrap();
    drop(client_write);

    let mut reader = BufReader::new(client_read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], "b1");
    assert_eq!(resp["success"], false);
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("bash tool is disabled"),
        "unexpected rpc response: {resp}"
    );
    let _ = rpc_handle.await;
}

#[tokio::test]
async fn rpc_cycle_model_rolls_back_state_on_credential_failure() {
    let _env_lock = crate::ENV_MUTEX.lock().await;
    struct EnvGuard {
        userprofile: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
        provider_env: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match &self.home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match &self.provider_env {
                Some(v) => std::env::set_var("TEST_ROLLBACK_PROVIDER_API_KEY", v),
                None => std::env::remove_var("TEST_ROLLBACK_PROVIDER_API_KEY"),
            }
        }
    }
    let _guard = EnvGuard {
        userprofile: std::env::var_os("USERPROFILE"),
        home: std::env::var_os("HOME"),
        provider_env: std::env::var_os("TEST_ROLLBACK_PROVIDER_API_KEY"),
    };

    let dir = crate::TempTestDir::new("rpc_cycle_rollback");
    std::env::set_var("USERPROFILE", dir.path());
    std::env::set_var("HOME", dir.path());
    std::env::remove_var("TEST_ROLLBACK_PROVIDER_API_KEY");

    let agent_dir = dir.path().join(".mehh").join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("auth.json"), "{corrupt").unwrap();

    let initial_model = mehh_ai::faux_model();
    let next_model = mehh_ai::Model {
        id: "rollback-next".to_string(),
        name: "Rollback Next".to_string(),
        api: "openai-completions".to_string(),
        provider: "test-rollback-provider".to_string(),
        base_url: "https://example.com/v1".to_string(),
        reasoning: true,
        ..Default::default()
    };

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        initial_model.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session.set_explicit_api_key(Some("sk-initial-explicit".to_string()));
    session.set_scoped_models(vec![initial_model.clone(), next_model]);
    let initial_thinking = session.thinking_level;

    let (client_read, server_write) = tokio::io::duplex(4096);
    let (server_read, mut client_write) = tokio::io::duplex(4096);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write)
            .await
            .unwrap();
        session
    });

    client_write
        .write_all(b"{\"id\":\"c1\",\"type\":\"cycle_model\"}\n")
        .await
        .unwrap();
    client_write.flush().await.unwrap();
    drop(client_write);

    let mut reader = BufReader::new(client_read);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], "c1");
    assert_eq!(resp["success"], false);
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to resolve provider credentials"),
        "unexpected rpc response: {resp}"
    );

    let session = rpc_handle.await.unwrap();
    assert_eq!(session.model.provider, initial_model.provider);
    assert_eq!(session.model.id, initial_model.id);
    assert_eq!(session.api_key.as_deref(), Some("sk-initial-explicit"));
    assert!(session.has_explicit_api_key());
    assert_eq!(session.thinking_level, initial_thinking);
}

#[tokio::test]
async fn c18_aborted_prompt_returns_operation_aborted_error() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_c18_abort_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);

    struct CancelThenStopProvider {
        started: tokio::sync::watch::Sender<bool>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for CancelThenStopProvider {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            _options: &StreamOptions,
            cancel: CancellationToken,
        ) -> AssistantEventStream {
            let (sender, stream) = create_event_stream();
            let started = self.started.clone();
            let model = model.clone();
            tokio::spawn(async move {
                let _ = started.send(true);
                cancel.cancelled().await;
                let msg = AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "late done".into(),
                        text_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                };
                sender.push(AssistantMessageEvent::Start {
                    partial: msg.clone(),
                });
                sender.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: msg,
                });
            });
            stream
        }
    }

    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(CancelThenStopProvider {
            started: started_tx,
        }),
        SessionManager::in_memory(temp_dir.clone()),
        Settings::default(),
    );

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    writer
        .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    while !*started_rx.borrow() {
        started_rx.changed().await.unwrap();
    }

    writer
        .write_all(b"{\"id\":\"a1\",\"type\":\"abort\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let prompt_resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("type").and_then(|x| x.as_str()) == Some("response")
                && v.get("command").and_then(|x| x.as_str()) == Some("prompt")
                && v["id"] == "p1"
            {
                return Some(v);
            }
        }
    })
    .await
    .expect("prompt response must arrive")
    .expect("EOF");
    assert_eq!(
        prompt_resp["success"], false,
        "aborted prompt must not succeed: {prompt_resp}"
    );
    assert_eq!(prompt_resp["error"], "Operation aborted");

    drop(writer);
    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn i17_set_thinking_null_bash_validation_and_session_tool_forwarding() {
    use std::sync::Mutex as StdMutex;
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_i17_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    struct SpyBashTool {
        seen: Arc<StdMutex<Vec<serde_json::Map<String, serde_json::Value>>>>,
    }
    #[async_trait::async_trait]
    impl AgentTool for SpyBashTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "spy bash"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<mehh_agent::AgentToolUpdateCallback>,
        ) -> Result<mehh_agent::AgentToolResult, String> {
            self.seen.lock().unwrap().push(params);
            Ok(mehh_agent::AgentToolResult::text("spy_ok"))
        }
    }

    let seen = Arc::new(StdMutex::new(Vec::new()));
    let mut session = AgentSession::new(
        temp_dir.clone(),
        make_test_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(temp_dir.clone()),
        Settings::default(),
    );
    session.inspection_manager = Arc::new(
        crate::tool_inspection::ToolInspectionManager::new_with_store(
            crate::tool_inspection::PermissionMode::Auto,
            temp_dir.clone(),
            crate::permissions::PermissionStore::new(Vec::new(), None),
        ),
    );
    let mut tools: Vec<_> = session
        .tools()
        .iter()
        .filter(|t| t.name() != "bash")
        .cloned()
        .collect();
    tools.push(Arc::new(SpyBashTool { seen: seen.clone() }));
    session.set_tools(tools);

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    // 1. set_thinking_level: high then null
    writer
        .write_all(b"{\"id\":\"t1\",\"type\":\"set_thinking_level\",\"level\":\"high\"}\n")
        .await
        .unwrap();
    writer
        .write_all(b"{\"id\":\"t2\",\"type\":\"set_thinking_level\",\"level\":null}\n")
        .await
        .unwrap();
    // 2. bash with missing and empty command
    writer
        .write_all(b"{\"id\":\"b_missing\",\"type\":\"bash\"}\n")
        .await
        .unwrap();
    writer
        .write_all(b"{\"id\":\"b_empty\",\"type\":\"bash\",\"command\":\"   \"}\n")
        .await
        .unwrap();
    // 3. bash with valid command and timeoutMs
    writer
        .write_all(b"{\"id\":\"b_ok\",\"type\":\"bash\",\"command\":\"echo hi\",\"timeoutMs\":1234}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    drop(writer);

    let mut responses = std::collections::HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
            responses.insert(id.to_string(), v);
        }
    }

    assert_eq!(responses["t1"]["success"], true);
    assert_eq!(responses["t2"]["success"], true, "level: null must clear thinking level: {:?}", responses["t2"]);
    assert_eq!(responses["b_missing"]["success"], false, "missing command must error");
    assert_eq!(responses["b_empty"]["success"], false, "empty command must error");
    assert_eq!(responses["b_ok"]["success"], true);
    assert_eq!(responses["b_ok"]["data"]["output"], "spy_ok", "must dispatch through session bash tool");
    {
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].get("timeoutMs"), Some(&serde_json::json!(1234)));
    }

    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[tokio::test]
async fn i17_compact_processes_concurrent_commands_while_running() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_rpc_compact_conc_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let release = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(GatedProvider {
        started: started_tx,
        release: release.clone(),
    });
    let mut model = make_test_model();
    model.context_window = 1000;
    let mut session = AgentSession::new(
        temp_dir.clone(),
        model,
        provider,
        SessionManager::in_memory(temp_dir.clone()),
        Settings::default(),
    );
    // Seed enough messages to exceed compaction keep_recent_tokens budget.
    let big = "word ".repeat(400);
    for _ in 0..4 {
        session.session_mut().append_message(Message::User(UserMessage {
            content: UserContent::Text(big.clone()),
            timestamp: now_ms(),
        })).unwrap();
    }

    let (client_write, server_read) = tokio::io::duplex(8192);
    let (server_write, client_read) = tokio::io::duplex(8192);
    let mut writer = client_write;
    let mut reader = BufReader::new(client_read);
    let rpc_handle = tokio::spawn(async move {
        run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
    });

    writer
        .write_all(b"{\"id\":\"cmp1\",\"type\":\"compact\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    while !*started_rx.borrow() {
        started_rx.changed().await.unwrap();
    }

    writer
        .write_all(b"{\"id\":\"q1\",\"type\":\"subagent_list\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    let answered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["id"] == "q1" {
                return Some(v);
            }
        }
    })
    .await
    .expect("subagent_list must be answered while compact is in flight")
    .expect("EOF");
    assert_eq!(answered["success"], true);

    release.store(true, Ordering::SeqCst);
    drop(writer);
    let _ = rpc_handle.await;
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn format_rpc_event_matches_reference_value_for_all_assistant_event_variants() {
    use mehh_ai::{AssistantMessage, AssistantMessageEvent, ToolCall};

    fn reference_strip(event: &AgentEvent) -> serde_json::Value {
        let mut val = serde_json::to_value(event).unwrap();
        if let Some(obj) = val.as_object_mut() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("message_update") {
                obj.remove("message");
                if let Some(ame) = obj
                    .get_mut("assistantMessageEvent")
                    .and_then(|v| v.as_object_mut())
                {
                    ame.remove("partial");
                }
            }
        }
        val
    }

    let partial = AssistantMessage {
        content: vec![],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_id: None,
        usage: mehh_ai::Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 123,
    };
    let tc = ToolCall {
        id: "tc1".to_string(),
        name: "read".to_string(),
        arguments: serde_json::Map::new(),
        arguments_error: None,
        thought_signature: None,
    };
    let variants = vec![
        AssistantMessageEvent::Start {
            partial: partial.clone(),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: partial.clone(),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hi".to_string(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "hi".to_string(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ThinkingStart {
            content_index: 1,
            partial: partial.clone(),
        },
        AssistantMessageEvent::ThinkingDelta {
            content_index: 1,
            delta: "hmm".to_string(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ThinkingEnd {
            content_index: 1,
            content: "hmm".to_string(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolCallStart {
            content_index: 2,
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolCallDelta {
            content_index: 2,
            delta: "{".to_string(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolCallEnd {
            content_index: 2,
            tool_call: tc,
            partial: partial.clone(),
        },
        AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: partial.clone(),
        },
        AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: partial.clone(),
        },
    ];

    for ame in variants {
        let ev = AgentEvent::MessageUpdate {
            message: partial.clone(),
            assistant_message_event: ame,
        };
        assert_eq!(format_rpc_event(&ev), reference_strip(&ev));
        let mut buf = Vec::new();
        super::protocol::write_rpc_event_json(&mut buf, &ev).unwrap();
        let expected = serde_json::to_vec(&format_rpc_event(&ev)).unwrap();
        assert_eq!(buf, expected, "direct writer diverged for {:?}", format_rpc_event(&ev));
    }
}
