use std::sync::Arc;

use mehh_agent::AgentTool;
use mehh_ai::{
    faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, FauxProvider,
    Message, ToolResultContentBlock,
};
use mehh_coding_agent::subagent::SubagentTool;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_batch_tasks_share_scratchpad_notes() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    // Stateless handlers: the "publish" task writes a note on turn 1; the
    // "consume" task reads it on turn 1 and echoes the tool result on turn 2.
    // Coding mode serializes the batch, so publish runs before consume.
    for _ in 0..4 {
        faux.add_handler(|_m, ctx, _o| {
            let prompt = ctx
                .messages
                .iter()
                .find_map(|m| match m {
                    Message::User(u) => Some(u.content.text()),
                    _ => None,
                })
                .unwrap_or_default();
            let has_assistant = ctx
                .messages
                .iter()
                .any(|m| matches!(m, Message::Assistant(_)));
            if prompt.contains("publish") {
                if has_assistant {
                    faux_assistant_message("published")
                } else {
                    faux_tool_call_message(vec![faux_tool_call(
                        "nw",
                        "note_write",
                        json!({"key": "finding", "value": "shared-secret-42"}),
                    )])
                }
            } else if has_assistant {
                let last_tool_text = ctx
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::ToolResult(t) => Some(
                            t.content
                                .iter()
                                .map(|b| match b {
                                    ToolResultContentBlock::Text(x) => x.text.clone(),
                                    _ => String::new(),
                                })
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                faux_assistant_message(format!("got: {last_tool_text}"))
            } else {
                faux_tool_call_message(vec![faux_tool_call(
                    "nr",
                    "note_read",
                    json!({"key": "finding"}),
                )])
            }
        });
    }

    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert("mode".to_string(), Value::String("coding".to_string()));
    params.insert(
        "tasks".to_string(),
        json!(["publish the finding", "consume the finding"]),
    );

    let res = sub
        .execute("tc_pad", params, CancellationToken::new(), None)
        .await
        .unwrap();
    let text: String = res
        .content
        .iter()
        .map(|b| match b {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(
        text.contains("got: shared-secret-42"),
        "second task must read the first task's note, got report:\n{text}"
    );
}
