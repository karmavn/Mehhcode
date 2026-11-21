use mehh_coding_agent::tool_inspection::{
    InspectionVerdict, PermissionMode, ToolInspectionContext, ToolInspectionManager,
};

#[tokio::test]
async fn test_security_inspector_blocks_mehh_metadata_edits() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(".mehh/settings.json"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_1".to_string(),
        tool_name: "edit".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
        origin: None,
    };

    let verdict = mgr.inspect(&ctx).await;
    assert!(matches!(verdict, InspectionVerdict::Deny { .. }));
}

#[tokio::test]
async fn test_security_inspector_blocks_dangerous_bash_commands() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("rm -rf /"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_2".to_string(),
        tool_name: "bash".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
        origin: None,
    };

    let verdict = mgr.inspect(&ctx).await;
    assert!(matches!(verdict, InspectionVerdict::Deny { .. }));
}

#[tokio::test]
async fn test_permission_inspector_smart_approve_allows_safe_and_requires_approval_on_edit() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::SmartApprove, cwd.clone());

    // read tool -> Allow
    let mut read_args = serde_json::Map::new();
    read_args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    let read_ctx = ToolInspectionContext {
        tool_call_id: "call_read".to_string(),
        tool_name: "read".to_string(),
        args: read_args,
        is_read_only: Some(true),
        cwd: cwd.clone(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&read_ctx).await,
        InspectionVerdict::Allow
    ));

    // edit tool -> RequireApproval
    let mut edit_args = serde_json::Map::new();
    edit_args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    let edit_ctx = ToolInspectionContext {
        tool_call_id: "call_edit".to_string(),
        tool_name: "edit".to_string(),
        args: edit_args,
        is_read_only: Some(false),
        cwd: cwd.clone(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&edit_ctx).await,
        InspectionVerdict::RequireApproval { .. }
    ));

    // safe command -> Allow
    let mut safe_bash = serde_json::Map::new();
    safe_bash.insert("command".to_string(), serde_json::json!("cargo check"));
    let bash_ctx = ToolInspectionContext {
        tool_call_id: "call_bash".to_string(),
        tool_name: "bash".to_string(),
        args: safe_bash,
        is_read_only: Some(false),
        cwd,
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&bash_ctx).await,
        InspectionVerdict::Allow
    ));
}

#[tokio::test]
async fn test_repetition_inspector_triggers_after_three_repeats() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("cargo test"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_repeat".to_string(),
        tool_name: "bash".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
        origin: None,
    };

    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(
        mgr.inspect(&ctx).await,
        InspectionVerdict::Deny { .. }
    ));
}

#[tokio::test]
async fn test_security_inspector_outside_cwd_requires_approval_even_in_auto_mode() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    // Inside cwd in Auto mode -> Allow
    let mut inside_args = serde_json::Map::new();
    inside_args.insert("path".to_string(), serde_json::json!("src/main.rs"));
    let inside_ctx = ToolInspectionContext {
        tool_call_id: "call_inside".to_string(),
        tool_name: "edit".to_string(),
        args: inside_args,
        is_read_only: Some(false),
        cwd: cwd.clone(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&inside_ctx).await,
        InspectionVerdict::Allow
    ));

    // Outside cwd in Auto mode -> RequireApproval
    let mut outside_args = serde_json::Map::new();
    outside_args.insert(
        "path".to_string(),
        serde_json::json!("../outside_secret.txt"),
    );
    let outside_ctx = ToolInspectionContext {
        tool_call_id: "call_outside".to_string(),
        tool_name: "edit".to_string(),
        args: outside_args,
        is_read_only: Some(false),
        cwd,
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&outside_ctx).await,
        InspectionVerdict::RequireApproval { .. }
    ));
}
