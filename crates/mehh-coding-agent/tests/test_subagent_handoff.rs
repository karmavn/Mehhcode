use std::sync::Arc;

use mehh_agent::AgentTool;
use mehh_ai::{faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, FauxProvider};
use mehh_coding_agent::subagent::SubagentTool;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

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
async fn test_max_turns_tool_only_final_gets_finalization_summary() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    // Turn 1 (the whole budget): a tool call with no concluding text.
    faux.add_response(faux_tool_call_message(vec![faux_tool_call(
        "c1",
        "ls",
        json!({ "path": cwd.to_string_lossy() }),
    )]));
    // The finalization call (tools stripped) must produce the summary.
    faux.add_response(faux_assistant_message("FINAL SUMMARY: inspected directory"));

    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux.clone(), None, tools, 0);

    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("Inspect the directory".to_string()));
    params.insert("mode".to_string(), Value::String("research".to_string()));
    params.insert("max_turns".to_string(), json!(1));

    let res = sub
        .execute("tc_fin", params, CancellationToken::new(), None)
        .await
        .expect("subagent should succeed");
    let text = result_text(&res);
    assert!(
        text.contains("FINAL SUMMARY: inspected directory"),
        "expected finalization summary, got: {text:?}"
    );

    // The finalization request must not offer any tools.
    let requests = faux.recorded_requests();
    assert_eq!(requests.len(), 2, "expected loop turn + finalization call");
    assert!(
        requests[1].context.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true),
        "finalization call must have no tools"
    );

    let sc = res.structured_content.as_ref().expect("structured handoff metadata");
    assert_eq!(sc["status"], "budget_exhausted");
    assert_eq!(sc["turns_used"], 2, "loop turn + finalization turn");
}

#[tokio::test]
async fn test_soft_landing_note_injected_two_turns_before_budget() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    for i in 0..3 {
        faux.add_response(faux_tool_call_message(vec![faux_tool_call(
            format!("c{i}"),
            "ls",
            json!({ "path": cwd.to_string_lossy() }),
        )]));
    }
    faux.add_response(faux_assistant_message("final answer"));

    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux.clone(), None, tools, 0);
    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("Research task".to_string()));
    params.insert("max_turns".to_string(), json!(4));

    sub.execute("tc_soft", params, CancellationToken::new(), None)
        .await
        .unwrap();

    let requests = faux.recorded_requests();
    assert_eq!(requests.len(), 4);
    // Turn 3 (index 2) must carry the soft-landing steering note.
    let turn3_texts: Vec<String> = requests[2]
        .context
        .messages
        .iter()
        .filter_map(|m| match m {
            mehh_ai::Message::User(u) => Some(u.content.text()),
            _ => None,
        })
        .collect();
    assert!(
        turn3_texts.iter().any(|t| t.contains("2 assistant turns left")),
        "expected soft-landing note before turn 3, got user texts: {turn3_texts:?}"
    );
}

#[tokio::test]
async fn test_completed_subagent_reports_usage_and_turns() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    let mut answer = faux_assistant_message("done: found the answer");
    answer.usage = mehh_ai::Usage {
        input: 100,
        output: 50,
        cache_read: 10,
        cache_write: 5,
        total_tokens: 165,
        cost: mehh_ai::UsageCost { total: 0.01, ..Default::default() },
    };
    faux.add_response(answer);

    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("Find the answer".to_string()));

    let res = sub
        .execute("tc_usage", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(result_text(&res).contains("done: found the answer"));
    let usage = res.usage.as_ref().expect("aggregated usage on tool result");
    assert_eq!(usage.input, 100);
    assert_eq!(usage.output, 50);
    assert_eq!(usage.total_tokens, 165);
    let sc = res.structured_content.as_ref().expect("structured handoff metadata");
    assert_eq!(sc["status"], "completed");
    assert_eq!(sc["turns_used"], 1);
    assert_eq!(sc["usage"]["input"], 100);
    assert_eq!(sc["usage"]["cost"]["total"], 0.01);
}

#[tokio::test]
async fn test_coding_subagent_reports_files_touched() {
    let dir = std::env::temp_dir().join(format!(
        "mehh_subagent_touch_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("created.txt");
    let target_str = target.to_string_lossy().to_string();

    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_tool_call_message(vec![faux_tool_call(
        "w1",
        "write",
        json!({ "path": target_str, "content": "hello" }),
    )]));
    faux.add_response(faux_assistant_message("created the file"));

    let tools = mehh_tools::create_coding_tools(dir.clone());
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert("prompt".to_string(), Value::String("Create a file".to_string()));
    params.insert("mode".to_string(), Value::String("coding".to_string()));

    let res = sub
        .execute("tc_touch", params, CancellationToken::new(), None)
        .await
        .unwrap();
    let sc = res.structured_content.as_ref().expect("structured handoff metadata");
    let touched = sc["files_touched"].as_array().expect("files_touched array");
    assert_eq!(touched.len(), 1);
    assert_eq!(touched[0].as_str().unwrap(), target_str);
    assert!(target.exists(), "write tool should actually have run");
    let _ = std::fs::remove_dir_all(&dir);
}
