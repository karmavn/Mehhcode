use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback}; // AgentTool: execute() is a trait method
use mehh_ai::{
    faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, AssistantMessage,
    FauxProvider, StopReason, Usage,
};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_lifecycle::{agent_store_dir, new_manifest, write_manifest};
use std::sync::Arc;

struct HangingProvider;

#[async_trait::async_trait]
impl mehh_ai::LlmProvider for HangingProvider {
    fn stream(
        &self,
        _model: &mehh_ai::Model,
        _context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = mehh_ai::create_event_stream();
        std::mem::forget(tx); // channel stays open forever, so the loop hangs until timeout
        rx
    }
}

fn manifest_files(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(agent_store_dir(cwd))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

/// Collects every structured update the tool emits through `on_update`.
fn update_collector() -> (
    Arc<std::sync::Mutex<Vec<AgentToolResult>>>,
    AgentToolUpdateCallback,
) {
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cb: AgentToolUpdateCallback = {
        let updates = updates.clone();
        Arc::new(move |res| updates.lock().unwrap().push(res))
    };
    (updates, cb)
}

fn result_text(res: &AgentToolResult) -> String {
    match &res.content[0] {
        mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
        _ => panic!("Expected text output"),
    }
}

/// After a failed/aborted/timed-out single-task child, the returned
/// ToolResult must carry `is_error: true` and `details.subagent` with the
/// manifest path and the terminal status of that child, and `on_update` must
/// still receive the terminal lifecycle event that drives the nested-group
/// display.
fn assert_failure_result_carries_manifest(
    res: &AgentToolResult,
    updates: &Arc<std::sync::Mutex<Vec<AgentToolResult>>>,
    store_files: &[std::path::PathBuf],
    terminal_kind: &str,
) {
    assert_eq!(
        res.is_error,
        Some(true),
        "domain failure must surface as an is_error result, not Err(String)"
    );
    let details = res
        .details
        .as_ref()
        .expect("failure result must carry details");
    let payload = &details["subagent"];
    let manifest_path = &store_files[0];
    assert_eq!(
        payload["manifest"].as_str(),
        Some(manifest_path.to_string_lossy().as_ref()),
        "payload must name the manifest on disk"
    );
    assert_eq!(
        payload["status"], "failed",
        "payload must carry the terminal status"
    );
    let agent_id = manifest_path.file_stem().unwrap().to_str().unwrap();
    assert_eq!(payload["child_id"].as_str(), Some(agent_id));

    let updates = updates.lock().unwrap();
    assert!(
        updates.iter().any(|u| {
            u.details
                .as_ref()
                .and_then(|d| d["subagent"]["event_kind"].as_str())
                == Some(terminal_kind)
        }),
        "on_update must still deliver the {terminal_kind} terminal event"
    );
}

#[tokio::test]
async fn successful_child_writes_completed_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ok_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub
        .execute(
            "tc1",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let details = res
        .details
        .expect("result must carry structured subagent details");
    assert_eq!(
        details["subagent"]["status"], "completed",
        "success payload must carry terminal status"
    );

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    assert_eq!(
        details["subagent"]["manifest"].as_str(),
        Some(files[0].to_string_lossy().as_ref()),
        "success payload must name the manifest on disk"
    );
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "completed");
    assert!(m["completed_at_ms"].is_number());
    assert_eq!(m["events"][0]["kind"], "started");
    assert!(m["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "finished"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn timeout_writes_terminal_timed_out_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_to_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(HangingProvider),
        None,
        vec![],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(1));
    let (updates, cb) = update_collector();
    let res = sub
        .execute(
            "tc2",
            params,
            tokio_util::sync::CancellationToken::new(),
            Some(cb),
        )
        .await
        .unwrap();
    assert!(
        result_text(&res).contains("timed out"),
        "timeout must stay in the result text, got: {:?}",
        result_text(&res)
    );

    let files = manifest_files(&dir);
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "timeout");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "timed_out"));
    assert_failure_result_carries_manifest(&res, &updates, &files, "timed_out");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn abort_writes_aborted_manifest_with_completed_at_ms() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ab_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(HangingProvider),
        None,
        vec![],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let (updates, cb) = update_collector();
    let task = tokio::spawn(async move { sub.execute("tc3", params, task_cancel, Some(cb)).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let res = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = result_text(&res);
    assert!(
        text.contains("aborted") || text.contains("Aborted"),
        "unexpected: {text}"
    );

    let files = manifest_files(&dir);
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "aborted");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "aborted"));
    assert_failure_result_carries_manifest(&res, &updates, &files, "aborted");
    std::fs::remove_dir_all(dir).unwrap();
}

struct PanickingProvider;

#[async_trait::async_trait]
impl mehh_ai::LlmProvider for PanickingProvider {
    fn stream(
        &self,
        _model: &mehh_ai::Model,
        _context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        panic!("boom");
    }
}

#[tokio::test]
async fn timeout_secs_is_clamped_to_schema_minimum() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_clamp_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(HangingProvider),
        None,
        vec![],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(0));
    let res = sub
        .execute(
            "tc_clamp",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "timeout must be an is_error result"
    );
    let text = result_text(&res);
    assert!(text.contains("timed out after 1s"), "unexpected: {text}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn generic_failure_finalizes_with_aborted_terminal_event() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_gen_{}", uuid::Uuid::now_v7()));
    let provider = Arc::new(FauxProvider::new());
    provider.add_response(AssistantMessage {
        content: Vec::new(),
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("boom".to_string()),
        timestamp: mehh_ai::now_ms(),
    });
    let sub = SubagentTool::new(dir.clone(), faux_model(), provider, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let (updates, cb) = update_collector();
    let res = sub
        .execute(
            "tc_gen",
            params,
            tokio_util::sync::CancellationToken::new(),
            Some(cb),
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "model failure must be an is_error result"
    );
    assert!(result_text(&res).contains("boom"));

    let files = manifest_files(&dir);
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(
        m["events"].as_array().unwrap().iter().last().unwrap()["kind"],
        "aborted"
    );
    assert_failure_result_carries_manifest(&res, &updates, &files, "aborted");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn batch_join_error_manifest_retains_started_event() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_join_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(PanickingProvider),
        None,
        vec![],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("tasks".into(), serde_json::json!(["do work"]));
    let res = sub
        .execute(
            "tc_join",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "a panicked batch child must mark the batch as failed"
    );
    let details = res.details.expect("batch details must be set");
    assert_eq!(details["subagent"]["status"], "failed");
    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["events"][0]["kind"], "started");
    assert_eq!(
        m["events"].as_array().unwrap().iter().last().unwrap()["kind"],
        "aborted"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn batch_result_details_status_is_terminal() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_bstat_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("tasks".into(), serde_json::json!(["do work"]));
    let res = sub
        .execute(
            "tc_bstat",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let details = res.details.expect("batch details must be set");
    assert_eq!(details["subagent"]["status"], "completed");
    std::fs::remove_dir_all(dir).unwrap();
}

// A mixed batch must report an aggregate, not the last-writer value: one
// success plus one failure aggregates to "failed" regardless of which child
// happened to be processed last.
#[tokio::test]
async fn mixed_batch_details_status_is_aggregate_failed() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_mixed_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("ok"));
    faux.add_response(AssistantMessage {
        content: Vec::new(),
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("child blew up".to_string()),
        timestamp: mehh_ai::now_ms(),
    });
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("tasks".into(), serde_json::json!(["task one", "task two"]));
    let res = sub
        .execute(
            "tc_mixed",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let text = result_text(&res);
    assert!(
        text.contains("ok"),
        "successful child output must be reported, got: {text}"
    );
    assert!(
        text.contains("child blew up"),
        "failed child error must be reported, got: {text}"
    );
    assert_eq!(
        res.is_error,
        Some(true),
        "a batch containing a failed child must be an is_error result"
    );
    let details = res.details.expect("batch details must be set");
    assert_eq!(
        details["subagent"]["status"], "failed",
        "mixed batch must aggregate to failed regardless of child order"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Tool that panics inside the child task so the parent has to finalize the
/// manifest from the shared event log instead of the task's dropped state.
struct PanicTool;

#[async_trait::async_trait]
impl AgentTool for PanicTool {
    fn name(&self) -> &str {
        "boom"
    }

    fn description(&self) -> &str {
        "Panics on purpose"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Map<String, serde_json::Value>,
        _cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        panic!("boom tool");
    }
}

// The child records `started` and `tool_started` before its task panics; the
// parent must seed the finalized manifest from the shared event log so those
// in-task events are not lost with the task's stack.
#[tokio::test]
async fn panicking_child_manifest_retains_started_and_tool_started_events() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_panic_{}", uuid::Uuid::now_v7()));
    let provider = Arc::new(FauxProvider::new());
    provider.add_response(faux_tool_call_message(vec![faux_tool_call(
        "c1",
        "boom",
        serde_json::json!({}),
    )]));
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        provider,
        None,
        vec![Arc::new(PanicTool)],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("tasks".into(), serde_json::json!(["do work"]));
    // coding mode keeps custom tools; research mode would filter `boom` out.
    params.insert("mode".into(), serde_json::json!("coding"));
    let res = sub
        .execute(
            "tc_ptool",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let details = res.details.expect("batch details must be set");
    assert_eq!(details["subagent"]["status"], "failed");

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    let kinds: Vec<&str> = m["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert_eq!(kinds.first(), Some(&"started"), "got {kinds:?}");
    assert!(
        kinds.contains(&"tool_started"),
        "in-task events must survive a child panic, got {kinds:?}"
    );
    assert_eq!(kinds.last(), Some(&"aborted"), "got {kinds:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn manifest_write_failure_is_surfaced() {
    let bad_cwd = std::env::temp_dir().join(format!("mehh_sub_bad_cwd_{}", uuid::Uuid::now_v7()));
    std::fs::write(&bad_cwd, "not a directory").unwrap();
    let sub = SubagentTool::new(
        bad_cwd.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        vec![],
        0,
    );
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub
        .execute(
            "tc_wfail",
            params,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let _ = std::fs::remove_file(&bad_cwd);
    assert_eq!(
        res.is_error,
        Some(true),
        "manifest write failure must surface as an is_error result"
    );
    let text = result_text(&res);
    assert!(
        text.contains("manifest write failed"),
        "error must name the write failure, got: {text}"
    );
    assert!(
        res.details
            .as_ref()
            .and_then(|d| d.get("subagent"))
            .is_some(),
        "failure result must keep details.subagent"
    );
}

// The brief's MEHH_AGENT_STORE env-var variant is unsafe here: tests in this
// binary run on parallel threads and sibling tests resolve the store dir through
// the same process-global env var. Per the brief's fallback, call write_manifest
// directly with a store path whose parent is a regular file.
#[test]
fn write_manifest_with_invalid_store_dir_errors_naming_the_path() {
    let base = std::env::temp_dir().join(format!("mehh_sub_store_file_{}", uuid::Uuid::now_v7()));
    std::fs::write(&base, "not a directory").unwrap();
    let store = base.join("agents");
    let m = new_manifest("sub-invalid".into(), "research".into(), "m".into(), 1000);
    let err = write_manifest(&store, &m).unwrap_err();
    let _ = std::fs::remove_file(&base);
    assert!(
        err.contains(&store.display().to_string()),
        "error must name the store path, got: {err}"
    );
}
