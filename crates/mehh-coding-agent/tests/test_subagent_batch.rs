use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use mehh_ai::{
    faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, FauxProvider,
};
use mehh_coding_agent::subagent::SubagentTool;
use serde_json::{json, Map, Value};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_batch_updates_carry_task_identity() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    // Stateless per-request handlers: each child does one ls call, then answers.
    for _ in 0..4 {
        let cwd = cwd.clone();
        faux.add_handler(move |_m, ctx, _o| {
            let has_assistant = ctx
                .messages
                .iter()
                .any(|m| matches!(m, mehh_ai::Message::Assistant(_)));
            if has_assistant {
                faux_assistant_message("answer")
            } else {
                faux_tool_call_message(vec![faux_tool_call(
                    "c",
                    "ls",
                    json!({ "path": cwd.to_string_lossy() }),
                )])
            }
        });
    }

    let updates: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = updates.clone();
    let cb: AgentToolUpdateCallback = Arc::new(move |res: AgentToolResult| {
        let text: String = res
            .content
            .iter()
            .map(|b| match b {
                mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
                _ => String::new(),
            })
            .collect();
        if let Ok(mut guard) = sink.lock() {
            guard.push(text);
        }
    });

    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert(
        "tasks".to_string(),
        json!(["first research task", "second research task"]),
    );

    sub.execute("tc_batch", params, CancellationToken::new(), Some(cb))
        .await
        .unwrap();

    let logged = updates.lock().unwrap();
    assert!(
        logged.iter().any(|s| s.contains("[subagent:#1:")),
        "expected task #1 identity in updates, got: {logged:?}"
    );
    assert!(
        logged.iter().any(|s| s.contains("[subagent:#2:")),
        "expected task #2 identity in updates, got: {logged:?}"
    );
}

/// Tool that records peak concurrency while it runs.
struct TrackTool {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for TrackTool {
    fn name(&self) -> &str {
        "track"
    }
    fn description(&self) -> &str {
        "Test tool recording peak concurrency"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object" })
    }
    fn is_read_only(&self) -> Option<bool> {
        Some(false)
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(cur, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("tracked"))
    }
}

#[tokio::test]
async fn test_parallel_coding_tasks_are_serialized_on_shared_cwd() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let faux = Arc::new(FauxProvider::new());
    // Each child: one track call on its first turn, then a text answer.
    for _ in 0..4 {
        faux.add_handler(|_m, ctx, _o| {
            let has_assistant = ctx
                .messages
                .iter()
                .any(|m| matches!(m, mehh_ai::Message::Assistant(_)));
            if has_assistant {
                faux_assistant_message("done")
            } else {
                faux_tool_call_message(vec![faux_tool_call("t", "track", json!({}))])
            }
        });
    }

    let cwd = std::env::temp_dir();
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(TrackTool {
        active: active.clone(),
        peak: peak.clone(),
    })];
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert("mode".to_string(), Value::String("coding".to_string()));
    params.insert(
        "tasks".to_string(),
        json!(["coding task one", "coding task two"]),
    );

    sub.execute("tc_ser", params, CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "coding tasks sharing one working tree must not run concurrently"
    );
}
