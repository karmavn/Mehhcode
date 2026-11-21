use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use mehh_ai::{faux_model, faux_tool_call, faux_tool_call_message, FauxProvider};
use mehh_coding_agent::subagent::SubagentTool;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

/// Tool that hangs until its cancellation token fires, then records cleanup.
/// Mirrors a long-running bash/MCP call that listens for cancellation.
struct HangTool {
    cleaned: Arc<AtomicBool>,
}

#[async_trait]
impl AgentTool for HangTool {
    fn name(&self) -> &str {
        "hang"
    }
    fn description(&self) -> &str {
        "Test tool that hangs until cancelled"
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
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        cancel.cancelled().await;
        self.cleaned.store(true, Ordering::SeqCst);
        Err("hang tool aborted".to_string())
    }
}

#[tokio::test]
async fn test_timeout_cancels_child_tools_before_dropping_them() {
    let cleaned = Arc::new(AtomicBool::new(false));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_tool_call_message(vec![faux_tool_call(
        "h1",
        "hang",
        json!({}),
    )]));

    let cwd = std::env::temp_dir();
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(HangTool {
        cleaned: cleaned.clone(),
    })];
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0)
        .with_timeout(Duration::from_millis(300));

    let mut params = Map::new();
    params.insert(
        "prompt".to_string(),
        Value::String("Hang forever".to_string()),
    );
    params.insert("mode".to_string(), Value::String("coding".to_string()));

    let start = std::time::Instant::now();
    // Upstream contract: domain failures (timeout/abort) are is_error results
    // carrying lifecycle details, not transport Err values.
    let res = sub
        .execute("tc_timeout", params, CancellationToken::new(), None)
        .await
        .expect("timeout must be returned as a result");
    assert_eq!(
        res.is_error,
        Some(true),
        "timeout result must be flagged as error"
    );
    let text: String = res
        .content
        .iter()
        .map(|b| match b {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(text.contains("timed out"), "unexpected result text: {text}");
    assert!(
        cleaned.load(Ordering::SeqCst),
        "child tool must observe cancellation (cleanup ran) before its future is dropped"
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "timeout + cleanup grace must stay bounded, took {:?}",
        start.elapsed()
    );
}
