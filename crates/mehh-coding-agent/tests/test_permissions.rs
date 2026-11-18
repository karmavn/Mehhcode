use std::path::PathBuf;
use mehh_coding_agent::permissions::{PermissionAction, PermissionRule, PermissionStore};
use mehh_coding_agent::tool_inspection::{InspectionVerdict, ToolInspectionContext};

#[test]
fn test_permission_store_evaluates_exact_and_prefix_rules() {
    let rules = vec![
        PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("git status".to_string()),
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        },
        PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("rm ".to_string()),
            path_glob: None,
            action: PermissionAction::NeverAllow,
        },
        PermissionRule {
            tool_name: "edit".to_string(),
            arg_prefix: None,
            path_glob: Some("*.rs".to_string()),
            action: PermissionAction::AskBefore,
        },
    ];

    let store = PermissionStore::new(rules, None);

    // Test git status -> AlwaysAllow
    let mut args1 = serde_json::Map::new();
    args1.insert("command".to_string(), serde_json::json!("git status -s"));
    let ctx1 = ToolInspectionContext {
        tool_call_id: "c1".to_string(),
        tool_name: "bash".to_string(),
        args: args1,
        is_read_only: Some(false),
        cwd: PathBuf::from("."),
    };
    assert_eq!(store.evaluate(&ctx1), Some(InspectionVerdict::Allow));

    // Test rm -> NeverAllow
    let mut args2 = serde_json::Map::new();
    args2.insert("command".to_string(), serde_json::json!("rm -f dummy.txt"));
    let ctx2 = ToolInspectionContext {
        tool_call_id: "c2".to_string(),
        tool_name: "bash".to_string(),
        args: args2,
        is_read_only: Some(false),
        cwd: PathBuf::from("."),
    };
    assert!(matches!(store.evaluate(&ctx2), Some(InspectionVerdict::Deny { .. })));

    // Test edit on .rs -> AskBefore
    let mut args3 = serde_json::Map::new();
    args3.insert("path".to_string(), serde_json::json!("src/main.rs"));
    let ctx3 = ToolInspectionContext {
        tool_call_id: "c3".to_string(),
        tool_name: "edit".to_string(),
        args: args3,
        is_read_only: Some(false),
        cwd: PathBuf::from("."),
    };
    assert!(matches!(store.evaluate(&ctx3), Some(InspectionVerdict::RequireApproval { .. })));

    // Test unmatching tool -> None (falls through)
    let ctx4 = ToolInspectionContext {
        tool_call_id: "c4".to_string(),
        tool_name: "read".to_string(),
        args: serde_json::Map::new(),
        is_read_only: Some(true),
        cwd: PathBuf::from("."),
    };
    assert_eq!(store.evaluate(&ctx4), None);
}

#[tokio::test]
async fn test_permission_store_add_rule_persists_to_file() {
    let temp_dir = std::env::temp_dir().join(format!(
        "mehh_perm_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let perm_file = temp_dir.join("permissions.json");

    let mut store = PermissionStore::new(Vec::new(), Some(perm_file.clone()));
    let new_rule = PermissionRule {
        tool_name: "custom_tool".to_string(),
        arg_prefix: None,
        path_glob: None,
        action: PermissionAction::AlwaysAllow,
    };

    store.add_rule(new_rule.clone()).unwrap();
    assert_eq!(store.rules().len(), 1);
    assert!(perm_file.exists());

    let _ = std::fs::remove_dir_all(&temp_dir);
}
