use mehh_coding_agent::permissions::{PermissionAction, PermissionRule, PermissionStore};
use mehh_coding_agent::tool_inspection::{
    InspectionVerdict, PermissionMode, ToolInspectionContext, ToolInspectionManager,
};
use std::path::PathBuf;

struct TestDir(PathBuf);

impl TestDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mehh_integ_{prefix}_{}_{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn test_security_inspector_blocks_mehh_metadata_edits() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::Auto,
        cwd.clone(),
        PermissionStore::default(),
    );

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
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::Auto,
        cwd.clone(),
        PermissionStore::default(),
    );

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
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::SmartApprove,
        cwd.clone(),
        PermissionStore::default(),
    );

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
    safe_bash.insert("command".to_string(), serde_json::json!("git status"));
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
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::Auto,
        cwd.clone(),
        PermissionStore::default(),
    );

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("git status"));

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
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::Auto,
        cwd.clone(),
        PermissionStore::default(),
    );

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

#[tokio::test]
async fn test_smart_approve_requires_confirmation_for_cargo_build_and_file_readers() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::SmartApprove,
        cwd.clone(),
        PermissionStore::default(),
    );

    for (tool, cmd) in [
        ("bash", "cargo check"),
        ("bash", "cargo clippy"),
        ("bash", "cargo test"),
        ("bash", "cat /etc/passwd"),
        ("powershell", "Select-String -Path secret.txt -Pattern key"),
    ] {
        let mut args = serde_json::Map::new();
        args.insert("command".to_string(), serde_json::json!(cmd));
        let ctx = ToolInspectionContext {
            tool_call_id: format!("call_{cmd}"),
            tool_name: tool.to_string(),
            args,
            is_read_only: Some(false),
            cwd: cwd.clone(),
            origin: None,
        };
        let verdict = mgr.inspect(&ctx).await;
        assert!(
            matches!(verdict, InspectionVerdict::RequireApproval { .. }),
            "SmartApprove must require confirmation for '{cmd}', got {verdict:?}"
        );
    }
}

#[tokio::test]
async fn test_permission_store_bom_fail_closed_and_workspace_deny_isolation() {
    let dir = TestDir::new("perm_bom_ws");
    let global_path = dir.path().join("global-permissions.json");
    let ws_path = dir.path().join("ws-permissions.json");

    // BOM-prefixed valid JSON loads cleanly, and workspace AlwaysAllow is ignored.
    std::fs::write(
        &global_path,
        "\u{feff}{\"rules\":[{\"tool_name\":\"bash\",\"action\":\"never_allow\"}]}",
    )
    .unwrap();
    std::fs::write(
        &ws_path,
        "\u{feff}{\"rules\":[{\"tool_name\":\"read\",\"action\":\"never_allow\"},{\"tool_name\":\"bash\",\"action\":\"always_allow\"}]}",
    )
    .unwrap();

    let mut store = PermissionStore::load_from_paths(&global_path, Some(&ws_path)).unwrap();
    assert_eq!(store.workspace_deny_rules().len(), 1);
    assert!(store
        .add_session_rule(PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("git status".to_string()),
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        })
        .is_ok());
    assert!(store
        .add_session_rule(PermissionRule {
            tool_name: "read".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        })
        .is_ok());

    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::Auto,
        dir.path().to_path_buf(),
        store,
    );

    let mut bash_args = serde_json::Map::new();
    bash_args.insert("command".to_string(), serde_json::json!("git status"));
    let bash_ctx = ToolInspectionContext {
        tool_call_id: "c_bash".to_string(),
        tool_name: "bash".to_string(),
        args: bash_args,
        is_read_only: Some(false),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&bash_ctx).await,
        InspectionVerdict::Deny { .. }
    ));

    let mut read_args = serde_json::Map::new();
    read_args.insert("path".to_string(), serde_json::json!("notes.txt"));
    let read_ctx = ToolInspectionContext {
        tool_call_id: "c_read".to_string(),
        tool_name: "read".to_string(),
        args: read_args,
        is_read_only: Some(true),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&read_ctx).await,
        InspectionVerdict::Deny { .. }
    ));

    // Corrupt JSON fails closed.
    std::fs::write(&global_path, "{corrupt").unwrap();
    assert!(PermissionStore::load_from_paths(&global_path, None).is_err());
}

#[tokio::test]
async fn test_permission_store_arg_prefix_dispatch_and_path_glob_scope() {
    let dir = TestDir::new("perm_arg_dispatch");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();

    let store = PermissionStore::new(
        vec![
            PermissionRule {
                tool_name: "web_search".to_string(),
                arg_prefix: Some("rust ".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            },
            PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: None,
                path_glob: Some("src/**/*.rs".to_string()),
                action: PermissionAction::AlwaysAllow,
            },
            PermissionRule {
                tool_name: "mcp__custom__tool".to_string(),
                arg_prefix: Some("secret:".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            },
        ],
        None,
    );
    let mgr = ToolInspectionManager::new_with_store(
        PermissionMode::SmartApprove,
        dir.path().to_path_buf(),
        store,
    );

    let mut ws_args = serde_json::Map::new();
    ws_args.insert("query".to_string(), serde_json::json!("rust async"));
    let ws_ctx = ToolInspectionContext {
        tool_call_id: "ws_1".to_string(),
        tool_name: "web_search".to_string(),
        args: ws_args,
        is_read_only: Some(true),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&ws_ctx).await,
        InspectionVerdict::Deny { .. }
    ));

    // Spoofed "path" on non-path-bearing bash must NOT auto-allow via path_glob.
    let mut bash_args = serde_json::Map::new();
    bash_args.insert("command".to_string(), serde_json::json!("cargo check"));
    bash_args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    let bash_ctx = ToolInspectionContext {
        tool_call_id: "b_1".to_string(),
        tool_name: "bash".to_string(),
        args: bash_args,
        is_read_only: Some(false),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&bash_ctx).await,
        InspectionVerdict::RequireApproval { .. }
    ));

    // Generic MCP tool inspects all candidate string fields.
    let mut mcp_args = serde_json::Map::new();
    mcp_args.insert("command".to_string(), serde_json::json!("safe"));
    mcp_args.insert("target".to_string(), serde_json::json!("secret:token"));
    let mcp_ctx = ToolInspectionContext {
        tool_call_id: "m_1".to_string(),
        tool_name: "mcp__custom__tool".to_string(),
        args: mcp_args,
        is_read_only: Some(false),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(matches!(
        mgr.inspect(&mcp_ctx).await,
        InspectionVerdict::Deny { .. }
    ));
}
