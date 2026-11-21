use mehh_agent::AgentTool;
use mehh_coding_agent::todo::{TodoState, TodoTool};
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn test_todo_tool_updates_shared_state() {
    let state = Arc::new(RwLock::new(TodoState::default()));
    let tool = TodoTool::new(state.clone());

    let mut args = serde_json::Map::new();
    args.insert("action".to_string(), serde_json::json!("add"));
    args.insert(
        "content".to_string(),
        serde_json::json!("Verify test suite"),
    );

    tool.execute(
        "call_1",
        args,
        tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await
    .expect("execute todo add");

    let read = state.read().await;
    assert_eq!(read.items.len(), 1);
    assert_eq!(read.items[0].content, "Verify test suite");
}

#[tokio::test]
async fn test_todo_moim_block_generation() {
    let mut state = TodoState::default();
    state.add("Run regression tests".to_string());
    state.update(
        1,
        Some(mehh_coding_agent::todo::TodoStatus::InProgress),
        None,
    );

    let temp = std::env::temp_dir();
    let block = state.format_moim_block(&temp, true).expect("moim block");
    assert!(block.contains("<working_checklist>"));
    assert!(block.contains("[/] #1 Run regression tests (in_progress)"));
    assert!(block.contains("</working_checklist>"));
}

#[tokio::test]
async fn test_moim_untrusted_ignores_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_moim_untrusted_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mehh_dir = temp_dir.join(".mehh");
    std::fs::create_dir_all(&mehh_dir).unwrap();
    std::fs::write(mehh_dir.join("MOIM.md"), "malicious prompt injection").unwrap();

    let state = TodoState::default();
    // In untrusted mode with no active items, format_moim_block must return None
    let block = state.format_moim_block(&temp_dir, false);
    assert!(block.is_none());

    // In trusted mode, it reads the file
    let block_trusted = state
        .format_moim_block(&temp_dir, true)
        .expect("trusted read");
    assert!(block_trusted.contains("malicious prompt injection"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}
