use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_coding_agent::tool_inspection::{
    ApprovalHandler, InspectionVerdict, PermissionMode, ToolInspectionContext,
    ToolInspectionManager,
};
use serde_json::json;

struct RecordingHandler {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    reasons: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl ApprovalHandler for RecordingHandler {
    async fn request_approval(
        &self,
        _ctx: &ToolInspectionContext,
        reason: &str,
        _preview: Option<&str>,
    ) -> bool {
        let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(cur, Ordering::SeqCst);
        self.reasons.lock().unwrap().push(reason.to_string());
        tokio::time::sleep(Duration::from_millis(60)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        true
    }
}

fn edit_ctx(cwd: &std::path::Path, id: &str, origin: Option<String>) -> ToolInspectionContext {
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), json!("src/lib.rs"));
    ToolInspectionContext {
        tool_call_id: id.to_string(),
        tool_name: "edit".to_string(),
        args,
        is_read_only: Some(false),
        cwd: cwd.to_path_buf(),
        origin,
    }
}

#[tokio::test]
async fn test_concurrent_approvals_are_serialized() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = Arc::new(ToolInspectionManager::new(
        PermissionMode::Approve,
        cwd.clone(),
    ));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let reasons = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler = Arc::new(RecordingHandler {
        active: active.clone(),
        peak: peak.clone(),
        reasons: reasons.clone(),
    });

    let ctx1 = edit_ctx(&cwd, "c1", Some("subagent:#1:coding".to_string()));
    let ctx2 = edit_ctx(&cwd, "c2", Some("subagent:#2:coding".to_string()));
    let mgr1 = mgr.clone();
    let mgr2 = mgr.clone();
    let h1 = handler.clone();
    let h2 = handler.clone();
    tokio::join!(
        async move { mgr1.inspect_and_authorize(&ctx1, Some(h1.as_ref())).await },
        async move { mgr2.inspect_and_authorize(&ctx2, Some(h2.as_ref())).await },
    );

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "approval prompts must be serialized, never shown concurrently"
    );
    assert_eq!(reasons.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn test_approval_reason_carries_subagent_origin() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Approve, cwd.clone());
    let reasons = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler = RecordingHandler {
        active: Arc::new(AtomicUsize::new(0)),
        peak: Arc::new(AtomicUsize::new(0)),
        reasons: reasons.clone(),
    };
    let ctx = edit_ctx(&cwd, "c1", Some("subagent:#2:coding".to_string()));
    mgr.inspect_and_authorize(&ctx, Some(&handler)).await;
    let logged = reasons.lock().unwrap();
    assert!(
        logged.iter().any(|r| r.contains("[subagent:#2:coding]")),
        "approval reason must identify the requesting subagent, got: {logged:?}"
    );
}

#[tokio::test]
async fn test_repetition_breaker_is_scoped_per_origin() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), json!("src/lib.rs"));

    // Two parallel subagents legitimately read the same file in alternation.
    // Each origin repeats only 3 times (the breaker threshold), so none may deny.
    for i in 0..6 {
        let origin = if i % 2 == 0 {
            "subagent:#1:research"
        } else {
            "subagent:#2:research"
        };
        let ctx = ToolInspectionContext {
            tool_call_id: format!("r{i}"),
            tool_name: "read".to_string(),
            args: args.clone(),
            is_read_only: Some(true),
            cwd: cwd.clone(),
            origin: Some(origin.to_string()),
        };
        let verdict = mgr.inspect(&ctx).await;
        assert!(
            matches!(verdict, InspectionVerdict::Allow),
            "call {i} (origin {origin}) must be allowed, got: {verdict:?}"
        );
    }

    // A single origin repeating past the threshold must still be denied.
    for i in 0..4 {
        let ctx = ToolInspectionContext {
            tool_call_id: format!("s{i}"),
            tool_name: "read".to_string(),
            args: args.clone(),
            is_read_only: Some(true),
            cwd: cwd.clone(),
            origin: Some("subagent:#3:research".to_string()),
        };
        let verdict = mgr.inspect(&ctx).await;
        if i < 3 {
            assert!(matches!(verdict, InspectionVerdict::Allow));
        } else {
            assert!(matches!(verdict, InspectionVerdict::Deny { .. }));
        }
    }
}
