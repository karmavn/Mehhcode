use std::sync::Arc;
use mehh_agent::AgentTool; // required: execute() is a trait method
use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_lifecycle::agent_store_dir;

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

#[tokio::test]
async fn successful_child_writes_completed_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ok_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub.execute("tc1", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap();
    assert!(res.details.is_some(), "result must carry structured subagent details");

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
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
    let err = sub.execute("tc2", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap_err();
    assert!(err.contains("timed out"));

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "timeout");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "timed_out"));
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
    let task = tokio::spawn(async move { sub.execute("tc3", params, task_cancel, None).await });
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
    std::fs::remove_dir_all(dir).unwrap();
}
