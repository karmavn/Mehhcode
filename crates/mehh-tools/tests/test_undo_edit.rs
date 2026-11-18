use std::sync::Arc;
use mehh_tools::{EditTool, EditUndoStore, UndoEditTool};
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_undo_edit_preserves_bom_and_crlf() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_test_undo_edit_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("crlf_bom.txt");

    // Original file with UTF-8 BOM and CRLF
    let original_bytes = b"\xEF\xBB\xBFline1\r\nline2\r\n";
    std::fs::write(&file_path, original_bytes).unwrap();

    let undo_store = Arc::new(EditUndoStore::new());
    let edit_tool = EditTool::new(temp_dir.clone()).with_undo_store(undo_store.clone());
    let undo_tool = UndoEditTool::new(temp_dir.clone(), undo_store);

    // Apply edit
    let mut edit_args = serde_json::Map::new();
    edit_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    edit_args.insert("oldText".to_string(), serde_json::json!("line2"));
    edit_args.insert("newText".to_string(), serde_json::json!("line2_edited"));
    edit_tool
        .execute(
            "call_edit",
            edit_args,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    // Rollback with undo_edit
    let mut undo_args = serde_json::Map::new();
    undo_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    undo_tool
        .execute(
            "call_undo",
            undo_args,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let restored_bytes = std::fs::read(&file_path).unwrap();
    assert_eq!(
        restored_bytes, original_bytes,
        "exact BOM and CRLF must be restored"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_undo_edit_reverts_newly_created_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_test_undo_create_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("created.txt");

    let undo_store = Arc::new(EditUndoStore::new());
    let write_tool = mehh_tools::WriteTool::new(temp_dir.clone()).with_undo_store(undo_store.clone());
    let undo_tool = UndoEditTool::new(temp_dir.clone(), undo_store);

    assert!(!file_path.exists());

    // Create file via write_tool
    let mut write_args = serde_json::Map::new();
    write_args.insert("path".to_string(), serde_json::json!("created.txt"));
    write_args.insert("content".to_string(), serde_json::json!("brand new content"));
    write_tool
        .execute(
            "call_write",
            write_args,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(file_path.exists());

    // Revert creation via undo_edit
    let mut undo_args = serde_json::Map::new();
    undo_args.insert("path".to_string(), serde_json::json!("created.txt"));
    undo_tool
        .execute(
            "call_undo",
            undo_args,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(!file_path.exists(), "newly created file should be deleted on rollback");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_undo_edit_order_sync_and_cleanup() {
    let store = EditUndoStore::new();
    let path = std::path::Path::new("dummy.txt");

    assert!(!store.has_snapshot(path));

    store.push_snapshot(
        path,
        mehh_tools::undo_edit::FileSnapshot::Existed {
            bytes: b"v1".to_vec(),
            timestamp_ms: 1000,
        },
    );
    assert!(store.has_snapshot(path));

    let popped = store.pop_snapshot(path);
    assert!(popped.is_some());
    assert!(!store.has_snapshot(path));
}
