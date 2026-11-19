use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use mehh_ai::{
    create_event_stream, faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message,
    Context, FauxProvider, LlmProvider, Message, Model, StreamOptions,
};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
use tokio_util::sync::CancellationToken;

struct HangingProvider;

#[async_trait]
impl LlmProvider for HangingProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
        _cancel: CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = create_event_stream();
        std::mem::forget(tx);
        rx
    }
}

struct HangTool {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl AgentTool for HangTool {
    fn name(&self) -> &str {
        "hang"
    }

    fn description(&self) -> &str {
        "Hangs until released"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Map<String, serde_json::Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        self.release.notified().await;
        Ok(AgentToolResult::text("released"))
    }
}

fn context_mentions_user_text(context: &Context, needle: &str) -> bool {
    context.messages.iter().any(|m| match m {
        Message::User(u) => u.content.text().contains(needle),
        _ => false,
    })
}

#[tokio::test]
async fn global_budget_is_shared_and_limited() {
    let control = Arc::new(SubagentControl::new(2));
    let _a = control.acquire().await;
    let _b = control.acquire().await;
    let blocked = tokio::time::timeout(Duration::from_millis(50), control.clone().acquire()).await;
    assert!(blocked.is_err(), "third acquire must block while both permits are held");
    assert_eq!(DEFAULT_MAX_CONCURRENT_SUBAGENTS, 4);
}

#[tokio::test]
async fn two_overlapping_subagent_calls_share_one_budget() {
    let control = Arc::new(SubagentControl::new(2));
    let cwd = std::env::temp_dir().join(format!("mehh_budget_{}", uuid::Uuid::now_v7()));
    let mk = || {
        SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
            .with_control(control.clone())
    };
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let c1 = tokio_util::sync::CancellationToken::new();
    let c2 = tokio_util::sync::CancellationToken::new();
    let c3 = tokio_util::sync::CancellationToken::new();
    let t1 = {
        let sub = mk();
        let p = params.clone();
        let cancel = c1.clone();
        tokio::spawn(async move { sub.execute("a", p, cancel, None).await })
    };
    let t2 = {
        let sub = mk();
        let p = params.clone();
        let cancel = c2.clone();
        tokio::spawn(async move { sub.execute("b", p, cancel, None).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(control.list().len(), 2, "two children from two separate calls");
    let t3 = {
        let sub = mk();
        let p = params;
        let cancel = c3.clone();
        tokio::spawn(async move { sub.execute("c", p, cancel, None).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), control.clone().acquire())
            .await
            .is_err(),
        "third child must be blocked by the shared budget"
    );
    c1.cancel();
    c2.cancel();
    c3.cancel();
    for t in [t1, t2, t3] {
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
    }
    std::fs::remove_dir_all(&cwd).unwrap();
}

#[tokio::test]
async fn kill_targets_one_child_and_steer_reaches_only_that_child() {
    let control = Arc::new(SubagentControl::new(4));
    let parent = tokio_util::sync::CancellationToken::new();
    let a = control.register("sub-a".into(), "coding".into(), parent.child_token());
    let b = control.register("sub-b".into(), "research".into(), parent.child_token());

    assert!(control.steer("sub-a", "new instruction"));
    assert_eq!(a.steering_queue.lock().unwrap().len(), 1);
    assert_eq!(b.steering_queue.lock().unwrap().len(), 0);
    assert!(!control.steer("missing", "x"));

    assert!(control.kill("sub-a"));
    assert!(a.cancel.is_cancelled());
    assert!(!b.cancel.is_cancelled());
    assert!(!parent.is_cancelled(), "parent turn must survive child kill");
    assert!(!control.kill("sub-a"), "already-unregistered id returns false");
    control.unregister("sub-a");
    assert_eq!(control.list().len(), 1);
}

#[tokio::test]
async fn kill_stops_a_real_child_loop() {
    let control = Arc::new(SubagentControl::new(4));
    let cwd = std::env::temp_dir().join(format!("mehh_kill_{}", uuid::Uuid::now_v7()));
    let (tx, mut rx) = tokio::sync::watch::channel("sub-0".to_string());
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let seen_clone = seen.clone();
    let on_update: AgentToolUpdateCallback = Arc::new(move |res| {
        if let Some(id) = res
            .details
            .as_ref()
            .and_then(|d| d.get("subagent"))
            .and_then(|s| s.get("child_id"))
            .and_then(|v| v.as_str())
        {
            if seen_clone.lock().unwrap().is_none() {
                *seen_clone.lock().unwrap() = Some(id.to_string());
                let _ = tx.send_modify(|v| *v = id.to_string());
            }
        }
    });
    let sub = SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
        .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let token = tokio_util::sync::CancellationToken::new();
    let task_cancel = token.clone();
    let task = tokio::spawn(async move { sub.execute("tc", params, task_cancel, Some(on_update)).await });
    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .expect("timed out waiting for child id")
        .expect("child id channel closed");
    let id = (*rx.borrow_and_update()).clone();
    assert!(control.kill(&id), "registered child must be killable");
    let result = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    let res = result.expect("killed child must not surface as a transport error");
    assert_eq!(res.is_error, Some(true), "killed child must be an is_error result");
    let details = res
        .details
        .expect("killed child result must carry details.subagent");
    assert_eq!(details["subagent"]["status"], "failed");
    std::fs::remove_dir_all(&cwd).unwrap();
}

// Standalone tools (built without `with_control`) must share the one
// process-wide budget instead of each getting a fresh per-call registry.
#[test]
fn standalone_tools_share_the_process_wide_control() {
    let cwd = std::env::temp_dir().join(format!("mehh_global_ctl_{}", uuid::Uuid::now_v7()));
    let mk = || {
        SubagentTool::new(
            cwd.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            None,
            vec![],
            0,
        )
    };
    let a = mk();
    let b = mk();
    assert!(
        Arc::ptr_eq(&a.control(), &b.control()),
        "standalone tools must share one control"
    );
    assert!(
        Arc::ptr_eq(&a.control(), mehh_coding_agent::subagent_control::global_control()),
        "standalone tools must fall back to the process-wide default"
    );
}

#[tokio::test]
async fn steer_message_reaches_child_context() {
    let control = Arc::new(SubagentControl::new(4));
    let cwd = std::env::temp_dir().join(format!("mehh_steer_{}", uuid::Uuid::now_v7()));
    let provider = Arc::new(FauxProvider::new());
    provider.add_response(faux_tool_call_message(vec![faux_tool_call("c1", "hang", serde_json::json!({}))]));
    provider.add_response(faux_assistant_message("done"));

    let hang_tool = Arc::new(HangTool {
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        provider.clone(),
        None,
        vec![hang_tool.clone()],
        0,
    )
    .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("mode".into(), serde_json::json!("coding"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let token = tokio_util::sync::CancellationToken::new();
    let task_cancel = token.clone();
    let (tx, mut rx) = tokio::sync::watch::channel("sub-0".to_string());
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let seen_clone = seen.clone();
    let on_update: AgentToolUpdateCallback = Arc::new(move |res| {
        if let Some(id) = res
            .details
            .as_ref()
            .and_then(|d| d.get("subagent"))
            .and_then(|s| s.get("child_id"))
            .and_then(|v| v.as_str())
        {
            if seen_clone.lock().unwrap().is_none() {
                *seen_clone.lock().unwrap() = Some(id.to_string());
                let _ = tx.send_modify(|v| *v = id.to_string());
            }
        }
    });
    let task = tokio::spawn(async move { sub.execute("ts", params, task_cancel, Some(on_update)).await });
    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .expect("timed out waiting for child id")
        .expect("child id channel closed");
    let id = (*rx.borrow_and_update()).clone();
    assert!(control.steer(&id, "ROUTE_ME"));
    hang_tool.release.notify_one();
    let _result = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap();

    let mut observed = false;
    for _ in 0..100 {
        if provider
            .recorded_requests()
            .iter()
            .any(|r| context_mentions_user_text(&r.context, "ROUTE_ME"))
        {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(observed, "steered message must reach the child context");
    std::fs::remove_dir_all(&cwd).unwrap();
}
