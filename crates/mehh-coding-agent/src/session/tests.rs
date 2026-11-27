use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::Path;

use super::*;
use mehh_ai::{now_ms, UserContent, UserMessage};
use std::sync::mpsc;
use std::time::Duration;

fn run_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(1))
        .expect("operation hung on a parentId cycle")
}

fn cyclic_session() -> SessionManager {
    let mut s = SessionManager::in_memory(std::env::temp_dir());
    let mk = |id: &str, parent: Option<&str>| SessionEntry {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        kind: SessionEntryKind::Message {
            message: Message::User(UserMessage {
                content: UserContent::Text(id.to_string()),
                timestamp: now_ms(),
            }),
        },
    };
    // A.parentId = B and B.parentId = A; the active leaf is A.
    s.entries = vec![mk("A", Some("B")), mk("B", Some("A"))];
    s.by_id.insert("A".to_string(), 0);
    s.by_id.insert("B".to_string(), 1);
    s.leaf_id = Some("A".to_string());
    s
}

#[test]
fn test_default_session_path() {
    let cwd = std::env::current_dir().unwrap();
    let path = default_session_path(&cwd).unwrap();
    let path_str = path.to_string_lossy();
    assert!(path_str.contains(".mehh"));
    assert!(path_str.contains("sessions"));
    assert!(path_str.ends_with(".jsonl"));
    // Check Windows safe filename: no colons in filename
    let file_name = path.file_name().unwrap().to_string_lossy();
    assert!(!file_name.contains(':'));
}

#[test]
fn test_new_session_and_active_path_entries() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_session_test_{}", Uuid::now_v7()));
    create_dir_all(&temp_dir).unwrap();
    let session_file = temp_dir.join("test_session.jsonl");

    let mut session = SessionManager::open_file(&session_file, &temp_dir).unwrap();
    let initial_id = session.session_id().to_string();

    let m1 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Hello 1".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let m2 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Hello 2".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let active = session.active_path_entries();
    assert_eq!(active.len(), 2);
    assert_eq!(active[0].id, m1);
    assert_eq!(active[1].id, m2);

    // Start new session
    session.new_session().unwrap();
    assert_ne!(session.session_id(), initial_id);
    assert_eq!(session.entries().len(), 0);
    assert!(session.leaf_id().is_none());
    assert_eq!(session.active_path_entries().len(), 0);

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_build_tree_and_fork_and_clone() {
    let temp_dir = std::env::temp_dir().join(format!("mehh_tree_test_{}", Uuid::now_v7()));
    create_dir_all(&temp_dir).unwrap();
    let session_file = temp_dir.join("main_session.jsonl");

    let mut session = SessionManager::open_file(&session_file, &temp_dir).unwrap();

    // Entry 1: User prompt
    let m1 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Build a web server".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    // Entry 2: Assistant reply
    let m2 = session
        .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
            "Here is code",
        )))
        .unwrap();

    // Entry 3: Branch A follow-up
    let m3 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Add database connection".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    // Branch from m2 into Branch B
    session.branch_from(&m2).unwrap();
    let m4 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Use in-memory storage instead".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    // Verify build_tree
    let tree = session.build_tree();
    assert_eq!(tree.len(), 4);
    assert_eq!(tree[0].entry_id, m1);
    assert_eq!(tree[0].depth, 0);
    assert!(tree[0].is_active);
    assert!(!tree[0].is_leaf);
    assert!(tree[0].is_user_or_branch);
    assert_eq!(tree[0].label, "User: Build a web server");

    assert_eq!(tree[1].entry_id, m2);
    assert_eq!(tree[1].depth, 1);
    assert!(tree[1].is_active);
    assert!(!tree[1].is_leaf);
    assert!(!tree[1].is_user_or_branch);

    // Children of m2: m3 and m4
    assert_eq!(tree[2].entry_id, m3);
    assert_eq!(tree[2].depth, 2);
    assert!(!tree[2].is_active); // m3 is inactive branch A
    assert!(!tree[2].is_leaf);

    assert_eq!(tree[3].entry_id, m4);
    assert_eq!(tree[3].depth, 2);
    assert!(tree[3].is_active); // m4 is active leaf branch B
    assert!(tree[3].is_leaf);

    // Test fork_at m3 (Branch A)
    let forked = session.fork_at(Some(&m3)).unwrap();
    assert_eq!(forked.entries().len(), 3); // m1, m2, m3
    assert_eq!(forked.leaf_id(), Some(m3.as_str()));
    assert_eq!(
        forked.header().parent_session.as_deref(),
        Some(session_file.display().to_string().as_str())
    );
    let forked_active = forked.active_path_entries();
    assert_eq!(forked_active.len(), 3);
    assert_eq!(forked_active[0].id, m1);
    assert_eq!(forked_active[1].id, m2);
    assert_eq!(forked_active[2].id, m3);

    // Test clone_at_leaf
    let cloned = session.clone_at_leaf().unwrap();
    assert_eq!(cloned.entries().len(), 3); // m1, m2, m4
    assert_eq!(cloned.leaf_id(), Some(m4.as_str()));
    assert_eq!(cloned.header().parent_session, None);
    assert_ne!(cloned.session_id(), session.session_id());

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_list_sessions() {
    let temp_dir =
        std::env::temp_dir().join(format!("mehh_list_sessions_test_{}", Uuid::now_v7()));
    create_dir_all(&temp_dir).unwrap();

    // Initially empty
    let empty_list = list_sessions(&temp_dir).unwrap();
    assert!(empty_list.is_empty());

    // Create a default session in that directory
    let sdir = session_dir_for_cwd(&temp_dir).unwrap();
    create_dir_all(&sdir).unwrap();

    let sfile1 = sdir.join("2026-11-10T10-00-00.000Z_session1.jsonl");
    let mut session1 = SessionManager::open_file(&sfile1, &temp_dir).unwrap();
    session1
        .append_entry(SessionEntryKind::SessionInfo {
            name: Some("First Named Session".to_string()),
        })
        .unwrap();
    session1
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Initial prompt for session 1".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let sfile2 = sdir.join("2026-11-12T12-00-00.000Z_session2.jsonl");
    let mut session2 = SessionManager::open_file(&sfile2, &temp_dir).unwrap();
    session2
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Second session prompt".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let sessions = list_sessions(&temp_dir).unwrap();
    assert_eq!(sessions.len(), 2);
    // Newest timestamp first: session2 then session1
    assert_eq!(
        sessions[0].first_prompt.as_deref(),
        Some("Second session prompt")
    );
    assert_eq!(sessions[0].name, None);
    assert_eq!(sessions[1].name.as_deref(), Some("First Named Session"));
    assert_eq!(
        sessions[1].first_prompt.as_deref(),
        Some("Initial prompt for session 1")
    );

    let _ = std::fs::remove_dir_all(temp_dir);
    let _ = std::fs::remove_dir_all(sdir);
}

#[test]
fn test_session_persists_branch_selection_and_recovers_from_corrupt_line() {
    let dir = std::env::temp_dir().join(format!("mehh_session_recover_{}", Uuid::now_v7()));
    create_dir_all(&dir).unwrap();
    let file = dir.join("branch_recover.jsonl");

    let (m1, m3) = {
        let mut s = SessionManager::open_file(&file, &dir).unwrap();
        let m1 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("root".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let _m2 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("branch A".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        s.branch_from(&m1).unwrap();
        let m3 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("branch B".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        // Switch active branch back to m1 WITHOUT appending another message
        s.branch_from(&m1).unwrap();
        (m1, m3)
    };

    // Append a trailing corrupted JSONL line
    {
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, "{{corrupt-trailing-line").unwrap();
    }

    let reopened = SessionManager::open_file(&file, &dir).unwrap();
    assert_eq!(reopened.leaf_id(), Some(m1.as_str()));
    assert_eq!(reopened.build_context_messages().len(), 1);
    assert!(reopened.entries().iter().any(|e| e.id == m3));

    // Non-ASCII bytes inside timestamp fractional field must not panic
    let ts = parse_iso8601_or_now("2026-11-12T10:00:00.äöüZ");
    assert!(ts > 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn export_jsonl_preserves_branch_selection_and_replaces_destination() {
    let temp = crate::TempTestDir::new("export_branch_selection");
    let source = temp.path().join("source.jsonl");
    let backup = temp.path().join("backup.jsonl");

    let (m1, m3) = {
        let mut s = SessionManager::open_file(&source, temp.path()).unwrap();
        let m1 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("root".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let _m2 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("abandoned branch".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        s.branch_from(&m1).unwrap();
        let m3 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("selected branch".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        // Switch the leaf back to the older message without a later entry.
        s.branch_from(&m1).unwrap();
        (m1, m3)
    };

    // A pre-existing destination must be replaced, not partially rewritten.
    std::fs::write(&backup, b"stale backup content\n").unwrap();
    SessionManager::open_file(&source, temp.path())
        .unwrap()
        .export_jsonl(&backup)
        .unwrap();

    let exported = std::fs::read_to_string(&backup).unwrap();
    assert!(
        exported
            .lines()
            .any(|line| PersistedBranchSelection::parse(line).as_deref() == Some(m1.as_str())),
        "export must emit the persisted branch selection: {exported}"
    );
    assert!(
        !exported.contains("stale backup content"),
        "export must replace the destination with a complete file"
    );

    let reopened = SessionManager::open_file(&backup, temp.path()).unwrap();
    assert_eq!(reopened.leaf_id(), Some(m1.as_str()));
    assert_eq!(reopened.build_context_messages().len(), 1);
    assert!(reopened.entries().iter().any(|e| e.id == m3));
}

#[test]
fn branch_from_does_not_move_leaf_when_persist_fails() {
    let temp = crate::TempTestDir::new("branch_write_order");
    let file = temp.path().join("branch.jsonl");
    let mut session = SessionManager::open_file(&file, temp.path()).unwrap();
    let m1 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("first".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();
    let m2 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("second".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();
    assert_eq!(session.leaf_id(), Some(m2.as_str()));

    // Turn the file path into a directory so appending the branch
    // selection fails after the file has been closed.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();

    assert!(
        session.branch_from(&m1).is_err(),
        "persisting a branch selection into a directory must fail"
    );
    assert_eq!(
        session.leaf_id(),
        Some(m2.as_str()),
        "failed persist must not change the in-memory leaf"
    );
}

#[test]
fn open_file_reports_skipped_malformed_entry() {
    let temp = crate::TempTestDir::new("skipped_entry_count");
    let file = temp.path().join("skipped.jsonl");
    let (m1, m2) = {
        let mut s = SessionManager::open_file(&file, temp.path()).unwrap();
        let a = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("one".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let b = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("two".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        (a, b)
    };

    // Header, m1, corrupt line, m2
    let raw = std::fs::read_to_string(&file).unwrap();
    let mut lines: Vec<&str> = raw.lines().collect();
    lines.insert(2, "{not-json");
    std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();

    let reopened = SessionManager::open_file(&file, temp.path()).unwrap();
    assert_eq!(
        reopened.skipped_entries(),
        1,
        "malformed mid-file line must be surfaced, not silently dropped"
    );
    let ids: Vec<&str> = reopened.entries().iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec![m1.as_str(), m2.as_str()]);
}

#[test]
fn usage_custom_message_context_edit_and_label_roundtrip() {
    let usage = mehh_ai::Usage {
        input: 100,
        output: 50,
        cache_read: 20,
        cache_write: 10,
        total_tokens: 150,
        cost: Default::default(),
    };
    let usage_entry = SessionEntry {
        id: "entry_usage".to_string(),
        parent_id: None,
        timestamp: "2026-11-15T12:00:00.000Z".to_string(),
        kind: SessionEntryKind::Usage {
            kind: Some("cache_warm".to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-3-5-sonnet".to_string()),
            usage: usage.clone(),
            note: Some("prewarm".to_string()),
        },
    };
    let json_usage = serde_json::to_string(&usage_entry).unwrap();
    assert!(json_usage.contains(r#""type":"usage""#));
    assert!(json_usage.contains(r#""kind":"cache_warm""#));
    let de_usage: SessionEntry = serde_json::from_str(&json_usage).unwrap();
    assert_eq!(de_usage.kind, usage_entry.kind);

    let custom_msg_entry = SessionEntry {
        id: "entry_custom".to_string(),
        parent_id: Some("entry_usage".to_string()),
        timestamp: "2026-11-15T12:01:00.000Z".to_string(),
        kind: SessionEntryKind::CustomMessage {
            display: Some(true),
            custom_type: "notification".to_string(),
            content: UserContent::Text("Build finished".to_string()),
            details: Some(serde_json::json!({ "buildId": "123" })),
        },
    };
    let json_custom = serde_json::to_string(&custom_msg_entry).unwrap();
    assert!(json_custom.contains(r#""type":"custom_message""#));
    assert!(json_custom.contains(r#""customType":"notification""#));
    let de_custom: SessionEntry = serde_json::from_str(&json_custom).unwrap();
    assert_eq!(de_custom.kind, custom_msg_entry.kind);

    let context_edit_entry = SessionEntry {
        id: "entry_edit".to_string(),
        parent_id: Some("entry_custom".to_string()),
        timestamp: "2026-11-15T12:02:00.000Z".to_string(),
        kind: SessionEntryKind::ContextEdit {
            target_id: "target_123".to_string(),
            replacement: Some(ContextEditReplacement {
                content: ContextEditableContent::Text("Edited message content".to_string()),
            }),
        },
    };
    let json_edit = serde_json::to_string(&context_edit_entry).unwrap();
    assert!(json_edit.contains(r#""type":"context_edit""#));
    assert!(json_edit.contains(r#""targetId":"target_123""#));
    let de_edit: SessionEntry = serde_json::from_str(&json_edit).unwrap();
    assert_eq!(de_edit.kind, context_edit_entry.kind);

    let label_entry = SessionEntry {
        id: "entry_label".to_string(),
        parent_id: Some("entry_edit".to_string()),
        timestamp: "2026-11-15T12:03:00.000Z".to_string(),
        kind: SessionEntryKind::Label {
            target_id: "target_123".to_string(),
            label: Some("checkpoint-1".to_string()),
        },
    };
    let json_label = serde_json::to_string(&label_entry).unwrap();
    assert!(json_label.contains(r#""type":"label""#));
    assert!(json_label.contains(r#""targetId":"target_123""#));
    assert!(json_label.contains(r#""label":"checkpoint-1""#));
    let de_label: SessionEntry = serde_json::from_str(&json_label).unwrap();
    assert_eq!(de_label.kind, label_entry.kind);
}

#[test]
fn context_edit_hides_and_replaces_without_mutating_tree() {
    let mut session = SessionManager::in_memory(std::env::temp_dir());

    let m1 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Message 1 to hide".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let m2 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Message 2 to replace".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let _m3 = session
        .append_custom_message(
            "custom_ext",
            UserContent::Text("Custom 3 to keep".to_string()),
            Some(true),
            None,
        )
        .unwrap();

    // 3 messages visible before edits
    let msgs_before = session.build_context_messages();
    assert_eq!(msgs_before.len(), 3);

    // Append ContextEdit hiding m1 (replacement = None)
    session.append_context_edit(&m1, None).unwrap();

    // Append ContextEdit replacing m2 content
    session
        .append_context_edit(
            &m2,
            Some(ContextEditReplacement {
                content: ContextEditableContent::Text("Replaced Message 2".to_string()),
            }),
        )
        .unwrap();

    // Check context messages
    let msgs_after = session.build_context_messages();
    assert_eq!(msgs_after.len(), 2);
    // m1 is hidden, so first message is m2 with replaced content
    match &msgs_after[0] {
        Message::User(u) => assert_eq!(u.content.text(), "Replaced Message 2"),
        _ => panic!("Expected user message"),
    }
    // second message is m3 (custom message projected as User)
    match &msgs_after[1] {
        Message::User(u) => assert_eq!(u.content.text(), "Custom 3 to keep"),
        _ => panic!("Expected user message from custom message"),
    }

    // Tree is intact (contains all entries: m1, m2, m3, edit1, edit2)
    assert_eq!(session.entries().len(), 5);
}

#[test]
fn label_entries_track_latest_bookmark_per_target() {
    let mut session = SessionManager::in_memory(std::env::temp_dir());

    let m1 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Task 1".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let m2 = session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("Task 2".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    // Set label on m1
    session.append_label(&m1, Some("v1.0".to_string())).unwrap();

    let rpc_tree = session.build_rpc_tree();
    assert_eq!(rpc_tree.len(), 1);
    let root = &rpc_tree[0];
    assert_eq!(root.id, m1);
    assert_eq!(root.labels, vec!["v1.0".to_string()]);

    // Overwrite label on m1 and set label on m2
    session.append_label(&m1, Some("v2.0".to_string())).unwrap();
    session
        .append_label(&m2, Some("feature".to_string()))
        .unwrap();

    let rpc_tree2 = session.build_rpc_tree();
    assert_eq!(rpc_tree2[0].labels, vec!["v2.0".to_string()]);

    // Clear label on m1 (None)
    session.append_label(&m1, None).unwrap();
    let rpc_tree3 = session.build_rpc_tree();
    assert!(rpc_tree3[0].labels.is_empty());
}

#[test]
fn test_fork_at_rejects_unknown_target_entry_id() {
    let temp = crate::TempTestDir::new("fork_reject");
    let session = SessionManager::create_default_file(temp.path()).unwrap();
    let res = session.fork_at(Some("non_existent_id"));
    match res {
        Err(e) => assert!(e
            .to_string()
            .contains("Target entry ID not found in session: non_existent_id")),
        Ok(_) => panic!("Expected fork_at to fail for non_existent_id"),
    }
}

#[test]
fn test_list_sessions_reflects_latest_renamed_session_name() {
    let temp = crate::TempTestDir::new("list_rename");
    let mut session = SessionManager::create_default_file(temp.path()).unwrap();
    session
        .set_session_name(Some("Original Name".to_string()))
        .unwrap();
    session
        .set_session_name(Some("Renamed Name".to_string()))
        .unwrap();

    let list = list_sessions(temp.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.as_deref(), Some("Renamed Name"));
}

#[test]
fn list_sessions_parses_escaped_quotes_and_backslashes_in_session_name() {
    let temp = crate::TempTestDir::new("list_escaped_name");
    let mut session = SessionManager::create_default_file(temp.path()).unwrap();

    session
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("initial prompt".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let name = "Fix \"auth\" bug in D:\\Mehhcode";
    session.set_session_name(Some(name.to_string())).unwrap();

    let list = list_sessions(temp.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.as_deref(), Some(name));
    assert_eq!(list[0].entry_count, 2);
}

#[test]
fn parse_iso8601_fractional_seconds_pads_right() {
    assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.5Z"), 500);
    assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.25Z"), 250);
    assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.125Z"), 125);
    assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.1234Z"), 123);
}

#[test]
fn branch_from_after_truncated_tail_survives_reopen() {
    let temp = crate::TempTestDir::new("branch_truncated_tail");
    let file = temp.path().join("branch_truncated.jsonl");

    let (m1, m2) = {
        let mut s = SessionManager::open_file(&file, temp.path()).unwrap();
        let m1 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("first".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let m2 = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("second".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        (m1, m2)
    };

    {
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        write!(f, "{{\"id\":\"fragment\"").unwrap();
    }

    let mut reopened = SessionManager::open_file(&file, temp.path()).unwrap();
    assert_eq!(reopened.leaf_id(), Some(m2.as_str()));
    assert_eq!(reopened.build_context_messages().len(), 2);

    reopened.branch_from(&m1).unwrap();

    let final_session = SessionManager::open_file(&file, temp.path()).unwrap();
    assert_eq!(final_session.leaf_id(), Some(m1.as_str()));
    assert_eq!(final_session.build_context_messages().len(), 1);
}

#[test]
fn test_list_sessions_skips_empty_user_message_for_first_prompt_and_trims_whitespace_name() {
    let temp = crate::TempTestDir::new("list_empty_prompt");
    let mut session = SessionManager::create_default_file(temp.path()).unwrap();
    session.set_session_name(Some("   ".to_string())).unwrap();

    // First user message is whitespace
    session
        .append_entry(SessionEntryKind::Message {
            message: Message::User(UserMessage {
                content: UserContent::Text("   \n  ".to_string()),
                timestamp: now_ms(),
            }),
        })
        .unwrap();

    // Second user message has real text
    session
        .append_entry(SessionEntryKind::Message {
            message: Message::User(UserMessage {
                content: UserContent::Text("Actual user goal here".to_string()),
                timestamp: now_ms(),
            }),
        })
        .unwrap();

    let list = list_sessions(temp.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, None);
    assert_eq!(
        list[0].first_prompt.as_deref(),
        Some("Actual user goal here")
    );
}

#[test]
fn truncated_tail_does_not_corrupt_appended_entry() {
    let dir = std::env::temp_dir().join(format!("mehh_trunc_tail_{}", Uuid::now_v7()));
    create_dir_all(&dir).unwrap();
    let file = dir.join("truncated.jsonl");

    // A valid session with one real entry.
    let real_id = {
        let mut s = SessionManager::open_file(&file, &dir).unwrap();
        s.append_message(Message::User(UserMessage {
            content: UserContent::Text("real entry".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap()
    };

    // Simulate a crash-truncated final line: valid JSON prefix, no newline.
    {
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        write!(f, "{{\"id\":\"fragment\"").unwrap();
    }

    // Reopen: the fragment is skipped, the real entry survives.
    let mut reopened = SessionManager::open_file(&file, &dir).unwrap();
    assert!(reopened.get_entry(&real_id).is_some());
    assert_eq!(reopened.entries().len(), 1);

    // Append a new entry. Without the fix this concatenates onto the
    // unterminated fragment, so BOTH the fragment line and the new entry
    // become unparseable on the next open.
    let new_id = reopened
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("appended after truncation".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    let final_session = SessionManager::open_file(&file, &dir).unwrap();
    assert!(
        final_session.get_entry(&new_id).is_some(),
        "the appended entry must survive reopening a file with a truncated tail"
    );
    assert!(
        final_session.get_entry(&real_id).is_some(),
        "the earlier valid entry must also survive"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cyclic_parent_ids_do_not_hang_context_projection() {
    let msgs = run_with_timeout(|| cyclic_session().build_context_messages());
    assert!(
        msgs.len() <= 2,
        "a parentId cycle must be traversed at most once per id, got {} messages",
        msgs.len()
    );
}

#[test]
fn cyclic_parent_ids_do_not_overflow_tree_builders() {
    let (tree, rpc_tree) = run_with_timeout(|| {
        let s = cyclic_session();
        (s.build_tree(), s.build_rpc_tree())
    });
    assert!(
        tree.len() <= 2,
        "tree traversal must terminate on a parentId cycle, got {} nodes",
        tree.len()
    );
    assert!(rpc_tree.len() <= 2);
}

#[test]
fn compaction_falls_back_to_full_history_when_first_kept_id_missing() {
    let mut s = SessionManager::in_memory(std::env::temp_dir());

    let _earlier_id = s
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("earlier message".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap();

    // A Compaction referencing an id that does not exist on the active path.
    s.append_entry(SessionEntryKind::Compaction {
        summary: "summary of history".to_string(),
        first_kept_entry_id: "does_not_exist".to_string(),
        tokens_before: 100,
    })
    .unwrap();

    let msgs = s.build_context_messages();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            Message::User(u) if u.content.text() == "earlier message"
        )),
        "earlier messages must stay in context when firstKeptEntryId cannot be found"
    );
}

#[test]
fn invalid_calendar_timestamps_fall_back_to_now() {
    let before = now_ms();
    for bad in [
        "2026-13-01T10:00:00.000Z", // month > 12
        "2026-00-01T10:00:00.000Z", // month 0
        "2026-02-30T10:00:00.000Z", // day out of range for February
        "2026-04-31T10:00:00.000Z", // day out of range for April
        "2026-01-00T10:00:00.000Z", // day 0
        "2025-02-29T10:00:00.000Z", // Feb 29 in a non-leap year
        "2026-01-01T24:00:00.000Z", // hour 24
        "2026-01-01T10:60:00.000Z", // minute 60
        "2026-01-01T10:00:60.000Z", // second 60
    ] {
        let got = parse_iso8601_or_now(bad);
        assert!(
            got >= before && got <= now_ms(),
            "invalid timestamp {bad} should fall back to now_ms(), got {got}"
        );
    }
}

#[test]
fn valid_calendar_timestamp_parses_exactly() {
    assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.000Z"), 0);
    assert_eq!(
        parse_iso8601_or_now("2026-11-12T13:17:17.000Z"),
        1_794_489_437_000
    );
    assert_eq!(
        parse_iso8601_or_now("2024-02-29T10:00:00.000Z"),
        1_709_200_800_000
    );
}

#[test]
fn open_existing_file_refuses_missing_path_without_creating_it() {
    let temp = crate::TempTestDir::new("open_existing");
    let missing = temp.path().join("does_not_exist.jsonl");
    let err = SessionManager::open_existing_file(&missing, temp.path().to_path_buf());
    assert!(err.is_err(), "open_existing_file must fail on missing file");
    assert!(!missing.exists(), "open_existing_file must not create missing file");
}

#[test]
fn open_file_skips_duplicate_entry_ids() {
    let temp = crate::TempTestDir::new("dup_ids");
    let file = temp.path().join("dup.jsonl");
    let m1 = {
        let mut s = SessionManager::open_file(&file, temp.path()).unwrap();
        s.append_message(Message::User(UserMessage {
            content: UserContent::Text("first".to_string()),
            timestamp: now_ms(),
        }))
        .unwrap()
    };
    let raw = std::fs::read_to_string(&file).unwrap();
    let entry_line = raw.lines().nth(1).unwrap();
    std::fs::write(&file, format!("{raw}{entry_line}\n")).unwrap();

    let reopened = SessionManager::open_file(&file, temp.path()).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    assert_eq!(reopened.skipped_entries(), 1);
    assert_eq!(reopened.entries()[0].id, m1);
}

#[test]
fn build_tree_and_rpc_tree_handle_rooted_cycles_and_deep_chains() {
    let mut s = SessionManager::in_memory(std::env::temp_dir());
    let mk = |id: &str, parent: Option<&str>| SessionEntry {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        kind: SessionEntryKind::Message {
            message: Message::User(UserMessage {
                content: UserContent::Text(id.to_string()),
                timestamp: now_ms(),
            }),
        },
    };
    // Root R -> A -> B -> A (cycle attached to a root)
    s.entries = vec![
        mk("R", None),
        mk("A", Some("R")),
        mk("B", Some("A")),
    ];
    // Point A's parentId into a cycle? Actually R -> A, B -> A, and add C -> B with A also parented or B -> R:
    // Wait: if R.parentId = None, A.parentId = Some("R"), and another node C has parentId = Some("D") and D has parentId = Some("C"),
    // plus we test deep linear chain (5000 entries) without stack overflow!
    s.by_id.insert("R".into(), 0);
    s.by_id.insert("A".into(), 1);
    s.by_id.insert("B".into(), 2);
    s.leaf_id = Some("B".into());
    let (tree, rpc) = run_with_timeout(move || (s.build_tree(), s.build_rpc_tree()));
    assert_eq!(tree.len(), 3);
    assert_eq!(rpc.len(), 1);

    let mut deep = SessionManager::in_memory(std::env::temp_dir());
    for i in 0..5000usize {
        let id = format!("e{i}");
        let parent = if i == 0 {
            None
        } else {
            Some(format!("e{}", i - 1))
        };
        deep.by_id.insert(id.clone(), i);
        deep.entries.push(mk(&id, parent.as_deref()));
    }
    deep.leaf_id = Some("e4999".into());
    let (deep_tree, deep_rpc) =
        run_with_timeout(move || (deep.build_tree(), deep.build_rpc_tree()));
    assert_eq!(deep_tree.len(), 5000);
    assert_eq!(deep_rpc.len(), 1);
}

#[test]
fn context_edit_cleans_up_orphaned_tool_calls_and_tool_results() {
    let mut s = SessionManager::in_memory(std::env::temp_dir());
    s.append_message(Message::User(UserMessage {
        content: UserContent::Text("run tools".into()),
        timestamp: now_ms(),
    }))
    .unwrap();

    let ast_id = s
        .append_message(Message::Assistant(mehh_ai::faux_tool_call_message(vec![
            mehh_ai::faux_tool_call("c1", "read", serde_json::json!({"path": "a.rs"})),
            mehh_ai::faux_tool_call("c2", "read", serde_json::json!({"path": "b.rs"})),
        ])))
        .unwrap();
    let tr1_id = s
        .append_message(Message::ToolResult(mehh_ai::ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: vec![mehh_ai::ToolResultContentBlock::Text(mehh_ai::TextContent {
                text: "a".into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            duration_ms: None,
            usage: None,
            timestamp: now_ms(),
        }))
        .unwrap();
    let _tr2_id = s
        .append_message(Message::ToolResult(mehh_ai::ToolResultMessage {
            tool_call_id: "c2".into(),
            tool_name: "read".into(),
            content: vec![mehh_ai::ToolResultContentBlock::Text(mehh_ai::TextContent {
                text: "b".into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            duration_ms: None,
            usage: None,
            timestamp: now_ms(),
        }))
        .unwrap();

    // Hide ToolResult c1 -> Assistant must drop ToolCall c1 while keeping ToolCall c2 + ToolResult c2.
    s.append_context_edit(&tr1_id, None).unwrap();
    let msgs = s.build_context_messages();
    assert_eq!(msgs.len(), 3); // User, Assistant(c2), ToolResult(c2)
    match &msgs[1] {
        Message::Assistant(a) => {
            let calls = a.tool_calls();
            let ids: Vec<&str> = calls.iter().map(|tc| tc.id.as_str()).collect();
            assert_eq!(ids, vec!["c2"]);
        }
        other => panic!("expected assistant, got {other:?}"),
    }

    // Now hide the Assistant message too -> ToolResult c2 must also be removed as orphaned.
    s.append_context_edit(&ast_id, None).unwrap();
    let msgs2 = s.build_context_messages();
    assert_eq!(msgs2.len(), 1); // Only User remains
}

#[test]
fn open_existing_file_and_export_session_resolve_relative_to_cwd_without_creating_missing() {
    let dir = crate::TempTestDir::new("session_rel_cwd");
    let existing_path = dir.path().join("saved.jsonl");
    let mut sm = SessionManager::open_file(&existing_path, dir.path()).unwrap();
    sm.append_message(Message::User(UserMessage {
        content: UserContent::Text("hello".into()),
        timestamp: now_ms(),
    }))
    .unwrap();

    let opened = SessionManager::open_existing_file(Path::new("saved.jsonl"), dir.path().to_path_buf())
        .expect("relative existing session should resolve against cwd");
    assert_eq!(opened.session_id(), sm.session_id());

    export_session(
        Path::new("saved.jsonl"),
        Some(Path::new("exported.jsonl")),
        dir.path(),
    )
    .expect("export_session should resolve both input and output relative to cwd");
    assert!(dir.path().join("exported.jsonl").exists());

    let missing = Path::new("does_not_exist.jsonl");
    assert!(
        export_session(missing, Some(Path::new("out.jsonl")), dir.path()).is_err(),
        "export_session must fail on missing input"
    );
    assert!(
        !dir.path().join(missing).exists(),
        "export_session must not create missing input file"
    );
}

#[test]
fn build_context_messages_inserts_user_bridge_when_compacted_kept_starts_with_assistant() {
    let mut s = SessionManager::in_memory(std::env::temp_dir());
    let _u1 = s
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("initial prompt".into()),
            timestamp: now_ms(),
        }))
        .unwrap();
    let a1 = s
        .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
            "assistant first kept",
        )))
        .unwrap();
    s.append_entry(SessionEntryKind::Compaction {
        summary: "summary".into(),
        first_kept_entry_id: a1,
        tokens_before: 1000,
    })
    .unwrap();

    let msgs = s.build_context_messages();
    assert_eq!(msgs.len(), 3);
    assert!(matches!(msgs[0], Message::System(_)));
    assert!(
        matches!(msgs[1], Message::User(_)),
        "expected leading User bridge before Assistant message after compaction, got {msgs:?}"
    );
    assert!(matches!(msgs[2], Message::Assistant(_)));
}

#[test]
fn create_file_with_id_writes_exact_header_id_and_preserves_open_file() {
    let dir = crate::TempTestDir::new("create_file_with_id");
    let file = dir.path().join("custom_id_session.jsonl");
    let mut sm =
        SessionManager::create_file_with_id(&file, dir.path(), "custom-session-123").unwrap();
    assert_eq!(sm.session_id(), "custom-session-123");
    sm.append_message(Message::User(UserMessage {
        content: UserContent::Text("hello".into()),
        timestamp: now_ms(),
    }))
    .unwrap();

    let reopened = SessionManager::open_file(&file, dir.path()).unwrap();
    assert_eq!(reopened.session_id(), "custom-session-123");
    assert_eq!(reopened.entries().len(), 1);
}

#[test]
fn append_entries_chains_parent_ids_and_is_atomic_on_error() {
    let dir = crate::TempTestDir::new("append_entries_atomic");
    let file = dir.path().join("batch_session.jsonl");
    let mut sm = SessionManager::open_file(&file, dir.path()).unwrap();

    let ids = sm
        .append_entries([
            SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("u1".into()),
                    timestamp: now_ms(),
                }),
            },
            SessionEntryKind::Message {
                message: Message::Assistant(mehh_ai::faux_assistant_message("a1")),
            },
            SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("u2".into()),
                    timestamp: now_ms(),
                }),
            },
        ])
        .unwrap();

    assert_eq!(ids.len(), 3);
    assert_eq!(sm.entries().len(), 3);
    assert_eq!(sm.leaf_id(), Some(ids[2].as_str()));
    assert_eq!(sm.entries()[0].parent_id, None);
    assert_eq!(sm.entries()[1].parent_id.as_deref(), Some(ids[0].as_str()));
    assert_eq!(sm.entries()[2].parent_id.as_deref(), Some(ids[1].as_str()));

    let reopened = SessionManager::open_file(&file, dir.path()).unwrap();
    assert_eq!(reopened.entries().len(), 3);
    assert_eq!(reopened.leaf_id(), Some(ids[2].as_str()));

    // Point file_path to an existing directory so open fails; memory must remain untouched.
    let mut failing = SessionManager::open_file(&file, dir.path()).unwrap();
    let blocked_dir = dir.path().join("blocked_dir");
    std::fs::create_dir_all(&blocked_dir).unwrap();
    failing.file_path = Some(blocked_dir);
    let err = failing.append_entries([SessionEntryKind::Message {
        message: Message::User(UserMessage {
            content: UserContent::Text("fail".into()),
            timestamp: now_ms(),
        }),
    }]);
    assert!(err.is_err());
    assert_eq!(failing.entries().len(), 3);
    assert_eq!(failing.leaf_id(), Some(ids[2].as_str()));
}
