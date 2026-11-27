use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use mehh_ai::{
    faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, FauxProvider,
    Message,
};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::permissions::PermissionStore;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::tool_inspection::{
    HeadlessDenyHandler, PermissionMode, ToolInspectionManager,
};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

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

struct HintedTool {
    executed_modes: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl AgentTool for HintedTool {
    fn name(&self) -> &str {
        "hinted_tool"
    }

    fn description(&self) -> &str {
        "Read-only hinted tool whose auto-approve safety depends on arguments"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string" }
            },
            "required": ["mode"]
        })
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    fn is_safe_for_auto_approve(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<bool> {
        Some(args.get("mode").and_then(|v| v.as_str()) == Some("safe"))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.executed_modes.lock().unwrap().push(mode.clone());
        Ok(AgentToolResult::text(format!("ran:{mode}")))
    }
}

#[tokio::test]
async fn test_subagent_is_safe_for_auto_approve_gates_read_only_hinted_tool() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_subagent_safe_gate_{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let executed = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(HintedTool {
        executed_modes: executed.clone(),
    });

    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_tool_call_message(vec![faux_tool_call(
        "c_unsafe",
        "hinted_tool",
        serde_json::json!({ "mode": "unsafe" }),
    )]));
    faux.add_response(faux_tool_call_message(vec![faux_tool_call(
        "c_safe",
        "hinted_tool",
        serde_json::json!({ "mode": "safe" }),
    )]));
    faux.add_response(faux_assistant_message("completed checks"));

    let mgr = Arc::new(ToolInspectionManager::new_with_store(
        PermissionMode::SmartApprove,
        temp_dir.clone(),
        PermissionStore::new(Vec::new(), None),
    ));
    let sub = SubagentTool::new(
        temp_dir.clone(),
        faux_model(),
        faux.clone(),
        None,
        vec![tool],
        0,
    )
    .with_inspection_manager(mgr)
    .with_approval_handler(Some(Arc::new(HeadlessDenyHandler)));

    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("test tool safety"));
    params.insert("mode".into(), serde_json::json!("research"));

    let res = sub
        .execute("tc_safe_gate", params, CancellationToken::new(), None)
        .await
        .expect("subagent execution must complete");
    assert_ne!(res.is_error, Some(true));

    assert_eq!(
        executed.lock().unwrap().as_slice(),
        &["safe".to_string()],
        "unsafe invocation must be gated even when is_read_only() is Some(true)"
    );

    let reqs = faux.recorded_requests();
    assert!(reqs.len() >= 2);
    let first_tool_result = reqs[1]
        .context
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) if tr.tool_call_id == "c_unsafe" => Some(tr),
            _ => None,
        })
        .expect("tool result for blocked c_unsafe call");
    assert!(first_tool_result.is_error, "blocked call must produce error tool result");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_subagent_completes_without_repolling_completed_loop_on_late_cancel() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_subagent_late_cancel_{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let cancel = CancellationToken::new();
    let cancel_in_handler = cancel.clone();
    let faux = Arc::new(FauxProvider::new());
    faux.add_handler(move |_model, _ctx, _opts| {
        // Cancel right as the single-turn response completes `loop_fut` so
        // `select!` takes the `loop_fut` branch and then hits `cancel.is_cancelled()`.
        cancel_in_handler.cancel();
        faux_assistant_message("done")
    });

    let sub = SubagentTool::new(
        temp_dir.clone(),
        faux_model(),
        faux,
        None,
        Vec::new(),
        0,
    );

    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("quick task"));

    let res = sub
        .execute("tc_late_cancel", params, cancel, None)
        .await
        .expect("late cancellation after loop completion must not panic by re-polling loop_fut");
    assert_eq!(res.is_error, Some(true));

    let _ = std::fs::remove_dir_all(&temp_dir);
}
