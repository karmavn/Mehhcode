use std::path::PathBuf;
use mehh_tools::AnalyzeTool;
use mehh_agent::AgentTool;

fn extract_text(res: &mehh_agent::AgentToolResult) -> String {
    match &res.content[0] {
        mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text block"),
    }
}

#[tokio::test]
async fn test_analyze_rust_symbols() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = AnalyzeTool::new(cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    args.insert("mode".to_string(), serde_json::json!("symbols"));

    let res = tool
        .execute("call_1", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute analyze");
    let text = extract_text(&res);
    assert!(text.contains("create_coding_tools"), "expected symbol in output: {}", text);
}

#[tokio::test]
async fn test_analyze_directory_structure() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = AnalyzeTool::new(cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src"));
    args.insert("mode".to_string(), serde_json::json!("structure"));
    args.insert("depth".to_string(), serde_json::json!(2));

    let res = tool
        .execute("call_2", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute structure analyze");
    let text = extract_text(&res);
    assert!(text.contains("lib.rs"), "expected lib.rs in structure output: {}", text);
    assert!(text.contains("files"), "expected file summary: {}", text);
}

#[tokio::test]
async fn test_analyze_focus_symbol() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = AnalyzeTool::new(cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    args.insert("mode".to_string(), serde_json::json!("focus"));
    args.insert("symbol".to_string(), serde_json::json!("create_coding_tools"));

    let res = tool
        .execute("call_3", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute focus analyze");
    let text = extract_text(&res);
    assert!(text.contains("pub fn create_coding_tools"), "expected full function signature: {}", text);
}
