use std::sync::Arc;

use mehh_agent::AgentTool;
use mehh_ai::{faux_assistant_message, faux_model, FauxProvider, Model, StopReason};
use mehh_coding_agent::lead_worker::LeadWorkerManager;
use mehh_coding_agent::subagent::SubagentTool;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

fn template(
    lead_turns: usize,
) -> (LeadWorkerManager, Arc<FauxProvider>, Arc<FauxProvider>, Model, Model) {
    let mut lead = faux_model();
    lead.id = "lead-x".to_string();
    let mut worker = faux_model();
    worker.id = "worker-x".to_string();
    let lp = Arc::new(FauxProvider::new());
    let wp = Arc::new(FauxProvider::new());
    let mgr = LeadWorkerManager::new(
        lead.clone(),
        lp.clone(),
        Some(worker.clone()),
        Some(wp.clone()),
        lead_turns,
    );
    (mgr, lp, wp, lead, worker)
}

fn result_text(res: &mehh_agent::AgentToolResult) -> String {
    res.content
        .iter()
        .map(|b| match b {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn test_research_subagent_starts_on_worker() {
    let (mgr, lp, wp, lead, _worker) = template(1);
    wp.add_response(faux_assistant_message("worker answer"));

    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, lead, lp.clone(), None, tools, 0)
        .with_lead_worker(Arc::new(std::sync::Mutex::new(mgr)));

    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("research task".to_string()));
    params.insert("mode".to_string(), Value::String("research".to_string()));

    let res = sub
        .execute("tc_wf", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(
        result_text(&res).contains("worker answer"),
        "research must run worker-first, got: {}",
        result_text(&res)
    );
    assert!(
        lp.recorded_requests().is_empty(),
        "lead model must not be used for a healthy research run"
    );
}

#[tokio::test]
async fn test_worker_error_rescues_via_lead_then_returns() {
    let (mgr, lp, wp, lead, _worker) = template(1);
    let mut boom = faux_assistant_message("");
    boom.stop_reason = StopReason::Error;
    boom.error_message = Some("worker provider failed".to_string());
    wp.add_response(boom);
    lp.add_response(faux_assistant_message("rescued by lead"));

    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, lead, lp.clone(), None, tools, 0)
        .with_lead_worker(Arc::new(std::sync::Mutex::new(mgr)));

    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("research task".to_string()));
    params.insert("mode".to_string(), Value::String("research".to_string()));

    let res = sub
        .execute("tc_rescue", params, CancellationToken::new(), None)
        .await
        .expect("lead rescue must recover the run instead of failing");
    assert!(result_text(&res).contains("rescued by lead"));
    assert_eq!(wp.recorded_requests().len(), 1);
    assert_eq!(lp.recorded_requests().len(), 1);
}

#[tokio::test]
async fn test_model_tier_lead_overrides_mode_routing() {
    let (mgr, lp, wp, lead, _worker) = template(1);
    lp.add_response(faux_assistant_message("lead answer"));

    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, lead, lp.clone(), None, tools, 0)
        .with_lead_worker(Arc::new(std::sync::Mutex::new(mgr)));

    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("research task".to_string()));
    params.insert("mode".to_string(), Value::String("research".to_string()));
    params.insert("model_tier".to_string(), Value::String("lead".to_string()));

    let res = sub
        .execute("tc_tier", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(result_text(&res).contains("lead answer"));
    assert!(wp.recorded_requests().is_empty());
}
