use std::sync::Arc;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback}; // AgentTool: execute() is a trait method
use mehh_ai::{
    faux_assistant_message, faux_model, AssistantMessage, FauxProvider, StopReason, Usage,
};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_lifecycle::{agent_store_dir, new_manifest, write_manifest};

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
    std::fs::read_dir(agent_store_dir(cwd)).unwrap()
        .map(|e| e.unwrap().path()).collect()
}

/// Collects every structured update the tool emits through `on_update`.
fn update_collector() -> (Arc<std::sync::Mutex<Vec<AgentToolResult>>>, AgentToolUpdateCallback) {
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cb: AgentToolUpdateCallback = {
        let updates = updates.clone();
        Arc::new(move |res| updates.lock().unwrap().push(res))
    };
    (updates, cb)
}

/// After a failed/aborted/timed-out single-task child, the final structured
/// update must carry `details.subagent` with the manifest path and the
/// terminal status of that child.
fn assert_error_payload_carries_manifest(
    updates: &Arc<std::sync::Mutex<Vec<AgentToolResult>>>,
    store_files: &[std::path::PathBuf],
) {
    let updates = updates.lock().unwrap();
    let last = updates
        .last()
        .expect("error path must deliver a final structured update");
    let details = last.details.as_ref().expect("final update must carry details");
    let payload = &details["subagent"];
    let manifest_path = &store_files[0];
    assert_eq!(
        payload["manifest"].as_str(),
        Some(manifest_path.to_string_lossy().as_ref()),
        "payload must name the manifest on disk"
    );
    assert_eq!(payload["status"], "failed", "payload must carry the terminal status");
    let agent_id = manifest_path.file_stem().unwrap().to_str().unwrap();
    assert_eq!(payload["child_id"].as_str(), Some(agent_id));
}

#[tokio::test]
async fn successful_child_writes_completed_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ok_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub.execute("tc1", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap();
    let details = res.details.expect("result must carry structured subagent details");
    assert_eq!(details["subagent"]["status"], "completed", "success payload must carry terminal status");

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    assert_eq!(
        details["subagent"]["manifest"].as_str(),
        Some(files[0].to_string_lossy().as_ref()),
        "success payload must name the manifest on disk"
    );
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "completed");
    assert!(m["completed_at_ms"].is_number());
    assert_eq!(m["events"][0]["kind"], "started");
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "finished"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn timeout_writes_terminal_timed_out_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_to_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(1));
    let (updates, cb) = update_collector();
    let err = sub.execute("tc2", params, tokio_util::sync::CancellationToken::new(), Some(cb)).await.unwrap_err();
    assert!(err.contains("timed out"));

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "timeout");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "timed_out"));
    assert_error_payload_carries_manifest(&updates, &files);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn abort_writes_aborted_manifest_with_completed_at_ms() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ab_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let (updates, cb) = update_collector();
    let task = tokio::spawn(async move { sub.execute("tc3", params, task_cancel, Some(cb)).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap_err();
    assert!(err.contains("aborted") || err.contains("Aborted"), "unexpected: {err}");

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "aborted");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "aborted"));
    assert_error_payload_carries_manifest(&updates, &files);
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
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(0));
    let err = sub
        .execute("tc_clamp", params, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(err.contains("timed out after 1s"), "unexpected: {err}");
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
    let err = sub
        .execute("tc_gen", params, tokio_util::sync::CancellationToken::new(), Some(cb))
        .await
        .unwrap_err();
    assert!(err.contains("boom"));

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(
        m["events"].as_array().unwrap().iter().last().unwrap()["kind"],
        "aborted"
    );
    assert_error_payload_carries_manifest(&updates, &files);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn batch_join_error_manifest_retains_started_event() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_join_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(PanickingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("tasks".into(), serde_json::json!(["do work"]));
    let res = sub
        .execute("tc_join", params, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(res.details.is_some());
    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
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
        .execute("tc_bstat", params, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();
    let details = res.details.expect("batch details must be set");
    assert_eq!(details["subagent"]["status"], "completed");
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
        .execute("tc_wfail", params, tokio_util::sync::CancellationToken::new(), None)
        .await;
    let _ = std::fs::remove_file(&bad_cwd);
    assert!(res.is_err(), "manifest write failure must surface as execute error");
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
