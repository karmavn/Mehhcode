//! Automatic branch summarization for session tree navigation.

use anyhow::{anyhow, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    estimate_message_tokens, serialize_conversation, SUMMARIZATION_SYSTEM_PROMPT,
};
use crate::session::{ContextEditReplacement, SessionEntry, SessionEntryKind, SessionManager};
use mehh_ai::{Context, LlmProvider, Message, Model, UserContent, UserMessage};

pub const BRANCH_SUMMARY_PREAMBLE: &str = r#"The user explored a different conversation branch before returning here.
Summary of that exploration:

"#;

pub const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

#[derive(Debug, Clone, PartialEq)]
pub struct CollectEntriesResult {
    pub entries: Vec<SessionEntry>,
    pub common_ancestor_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BranchPreparation {
    pub messages: Vec<Message>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub total_tokens: usize,
}

/// Collects entries that should be summarized when navigating from `old_leaf_id` to `target_id`.
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> CollectEntriesResult {
    let old_leaf = match old_leaf_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            return CollectEntriesResult {
                entries: Vec::new(),
                common_ancestor_id: None,
            }
        }
    };

    let mut old_path: Vec<&SessionEntry> = Vec::new();
    let mut curr = Some(old_leaf);
    let mut old_path_set: HashSet<&str> = HashSet::new();

    while let Some(id) = curr {
        if let Some(entry) = session.get_entry(id) {
            if !old_path_set.insert(entry.id.as_str()) {
                break;
            }
            old_path.push(entry);
            curr = entry.parent_id.as_deref();
        } else {
            break;
        }
    }

    let mut target_path: Vec<&SessionEntry> = Vec::new();
    let mut curr_t = Some(target_id);
    let mut target_visited: HashSet<&str> = HashSet::new();
    while let Some(id) = curr_t {
        if let Some(entry) = session.get_entry(id) {
            if !target_visited.insert(entry.id.as_str()) {
                break;
            }
            target_path.push(entry);
            curr_t = entry.parent_id.as_deref();
        } else {
            break;
        }
    }
    target_path.reverse();

    let mut common_ancestor_id: Option<String> = None;
    for entry in target_path.iter().rev() {
        if old_path_set.contains(entry.id.as_str()) {
            common_ancestor_id = Some(entry.id.clone());
            break;
        }
    }

    let mut entries = Vec::new();
    for entry in old_path {
        if Some(entry.id.as_str()) == common_ancestor_id.as_deref() {
            break;
        }
        entries.push(entry.clone());
    }
    entries.reverse();

    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

/// Prepares branch entries into messages, trims to token budget, and extracts read/modified file paths.
pub fn prepare_branch_entries(entries: &[SessionEntry], token_budget: usize) -> BranchPreparation {
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    let mut edits: HashMap<&str, &Option<ContextEditReplacement>> = HashMap::new();
    for entry in entries {
        if let SessionEntryKind::ContextEdit {
            target_id,
            replacement,
        } = &entry.kind
        {
            edits.insert(target_id.as_str(), replacement);
        }
    }

    let mut removed_result_ids: HashSet<String> = HashSet::new();
    let mut removed_call_ids: HashSet<String> = HashSet::new();
    let mut messages = Vec::new();
    let mut total_tokens = 0;

    for entry in entries.iter().rev() {
        if let Some(edit) = edits.get(entry.id.as_str()) {
            if edit.is_none() {
                if let SessionEntryKind::Message { message } = &entry.kind {
                    match message {
                        Message::ToolResult(tr) => {
                            removed_result_ids.insert(tr.tool_call_id.clone());
                        }
                        Message::Assistant(a) => {
                            for b in &a.content {
                                if let mehh_ai::AssistantContentBlock::ToolCall(tc) = b {
                                    removed_call_ids.insert(tc.id.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                continue;
            }
        }
        let replacement = edits.get(entry.id.as_str()).and_then(|r| r.as_ref());

        let entry_msg = match &entry.kind {
            SessionEntryKind::Message { message } => {
                if let Some(rep) = replacement {
                    let mut msg = message.clone();
                    match &mut msg {
                        Message::User(u) => {
                            u.content = rep.content.to_user_content();
                        }
                        Message::Assistant(a) => {
                            a.content = rep.content.to_assistant_content();
                        }
                        Message::ToolResult(tr) => {
                            tr.content = rep.content.to_tool_result_content();
                        }
                        Message::System(s) => {
                            s.content = rep.content.to_text();
                        }
                    }
                    Some(msg)
                } else {
                    Some(message.clone())
                }
            }
            SessionEntryKind::CustomMessage { content, .. } => {
                let effective_content = if let Some(rep) = replacement {
                    rep.content.to_user_content()
                } else {
                    content.clone()
                };
                let ts = crate::session::parse_iso8601_or_now(&entry.timestamp);
                Some(Message::User(UserMessage {
                    content: effective_content,
                    timestamp: ts,
                }))
            }
            SessionEntryKind::BranchSummary { summary, .. } => {
                let ts = crate::session::parse_iso8601_or_now(&entry.timestamp);
                Some(Message::User(UserMessage {
                    content: UserContent::Text(format!(
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
                    )),
                    timestamp: ts,
                }))
            }
            SessionEntryKind::Compaction { summary, .. } => {
                let ts = crate::session::parse_iso8601_or_now(&entry.timestamp);
                Some(Message::User(UserMessage {
                    content: UserContent::Text(format!(
                        "The following is a summary of earlier conversation in this branch:\n\n<summary>\n{summary}\n</summary>"
                    )),
                    timestamp: ts,
                }))
            }
            _ => None,
        };

        let Some(msg) = entry_msg else {
            continue;
        };

        let entry_tokens = estimate_message_tokens(&msg);
        let within_budget = total_tokens + entry_tokens <= token_budget;
        let allow_summary_overrun = matches!(
            &entry.kind,
            SessionEntryKind::Compaction { .. } | SessionEntryKind::BranchSummary { .. }
        ) && total_tokens < (token_budget * 9 / 10);
        let must_keep_latest = messages.is_empty();

        if !within_budget && !allow_summary_overrun && !must_keep_latest {
            break;
        }

        if let Message::Assistant(ast) = &msg {
            for b in &ast.content {
                if let mehh_ai::AssistantContentBlock::ToolCall(call) = b {
                    if let Some(path_val) = call.arguments.get("path").and_then(|v| v.as_str()) {
                        match call.name.as_str() {
                            "read" => {
                                read_files_set.insert(path_val.to_string());
                            }
                            "write" | "edit" => {
                                modified_files_set.insert(path_val.to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        messages.push(msg);
        total_tokens += entry_tokens;
        if !within_budget {
            break;
        }
    }
    messages.reverse();

    if !removed_result_ids.is_empty() || !removed_call_ids.is_empty() {
        messages.retain_mut(|msg| match msg {
            Message::ToolResult(tr) => !removed_call_ids.contains(&tr.tool_call_id),
            Message::Assistant(a) if !removed_result_ids.is_empty() => {
                a.content.retain(|block| match block {
                    mehh_ai::AssistantContentBlock::ToolCall(tc) => {
                        !removed_result_ids.contains(&tc.id)
                    }
                    _ => true,
                });
                !a.content.is_empty()
            }
            _ => true,
        });
        total_tokens = messages.iter().map(estimate_message_tokens).sum();
    }

    let read_files: Vec<String> = read_files_set
        .into_iter()
        .filter(|f| !modified_files_set.contains(f))
        .collect();
    let modified_files: Vec<String> = modified_files_set.into_iter().collect();

    BranchPreparation {
        messages,
        read_files,
        modified_files,
        total_tokens,
    }
}

/// Generates a structured branch summary using an LLM call.
pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    model: &Model,
    provider: &dyn LlmProvider,
    api_key: Option<&str>,
    custom_instructions: Option<&str>,
    reserve_tokens: Option<usize>,
    cancel: CancellationToken,
) -> Result<String> {
    let context_window = if model.context_window > 0 {
        model.context_window as usize
    } else {
        128_000
    };
    let reserve = reserve_tokens.unwrap_or(16_384);
    let token_budget = context_window.saturating_sub(reserve).max(1024);

    let prep = prepare_branch_entries(entries, token_budget);
    if prep.messages.is_empty() {
        return Ok("No content to summarize".to_string());
    }

    let mut conversation_text = serialize_conversation(&prep.messages);
    let instructions = if let Some(ci) = custom_instructions {
        if !ci.trim().is_empty() {
            format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {}", ci.trim())
        } else {
            BRANCH_SUMMARY_PROMPT.to_string()
        }
    } else {
        BRANCH_SUMMARY_PROMPT.to_string()
    };

    let total_budget_chars = context_window
        .saturating_sub(8192)
        .saturating_mul(3)
        .max(4096);
    let overhead_chars =
        SUMMARIZATION_SYSTEM_PROMPT.chars().count() + instructions.chars().count() + 64;
    let conv_budget_chars = total_budget_chars.saturating_sub(overhead_chars).max(1024);
    if conversation_text.chars().count() > conv_budget_chars {
        if let Some((byte_start, _)) = conversation_text
            .char_indices()
            .rev()
            .nth(conv_budget_chars - 1)
        {
            let newest = conversation_text[byte_start..].to_string();
            conversation_text =
                format!("[... older history truncated for summarization]\n\n{newest}");
        }
    }

    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");
    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(prompt_text),
            timestamp: mehh_ai::now_ms(),
        })],
        tools: None,
    };

    let stream_opts = mehh_ai::StreamOptions {
        api_key: api_key.map(str::to_string),
        ..Default::default()
    };

    let mut stream = provider.stream(model, &context, &stream_opts, cancel);
    let final_msg = stream.collect_final_message().await?;

    if matches!(
        final_msg.stop_reason,
        mehh_ai::StopReason::Error | mehh_ai::StopReason::Aborted
    ) {
        let err = final_msg.error_message.unwrap_or_else(|| {
            format!(
                "Branch summarization stopped with {:?}",
                final_msg.stop_reason
            )
        });
        return Err(anyhow!("Branch summarization model stream failed: {err}"));
    }

    let mut summary = final_msg.text_content().trim().to_string();
    if summary.is_empty() {
        summary = "No summary generated".to_string();
    }

    summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");

    if !prep.read_files.is_empty() {
        summary.push_str("\n\n<read-files>\n");
        for f in &prep.read_files {
            summary.push_str(&format!("- {f}\n"));
        }
        summary.push_str("</read-files>");
    }

    if !prep.modified_files.is_empty() {
        summary.push_str("\n\n<modified-files>\n");
        for f in &prep.modified_files {
            summary.push_str(&format!("- {f}\n"));
        }
        summary.push_str("</modified-files>");
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{now_ms, ToolCall};

    #[test]
    fn collect_entries_for_branch_summary_stops_at_common_ancestor() {
        let mut session = SessionManager::in_memory(std::env::temp_dir());

        // Root entry m0
        let _m0 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Root task".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Common ancestor m1
        let m1 = session
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Working on task",
            )))
            .unwrap();

        // Branch A (old branch): m2 -> m3
        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Branch A step 1".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let m3 = session
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Branch A step 2 done",
            )))
            .unwrap();

        // Switch to m1 and create Branch B (target): m4
        session.branch_from(&m1).unwrap();
        let m4 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Branch B alternative".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // When navigating from m3 (Branch A leaf) to m4 (Branch B leaf):
        // Common ancestor is m1.
        // Entries to summarize should be m2, m3 (in chronological order).
        let res = collect_entries_for_branch_summary(&session, Some(&m3), &m4);
        assert_eq!(res.common_ancestor_id.as_deref(), Some(m1.as_str()));
        assert_eq!(res.entries.len(), 2);
        assert_eq!(res.entries[0].id, m2);
        assert_eq!(res.entries[1].id, m3);
    }

    #[test]
    fn prepare_branch_entries_trims_to_budget_and_extracts_file_ops() {
        let mut entries = Vec::new();

        // Entry 1: User message
        entries.push(SessionEntry {
            id: "e1".to_string(),
            parent_id: None,
            timestamp: "2026-11-15T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Inspect files".to_string()),
                    timestamp: now_ms(),
                }),
            },
        });

        // Entry 2: Assistant reading foo.rs and modifying bar.rs
        let mut ast = mehh_ai::faux_assistant_message("Reading and editing");
        ast.content
            .push(mehh_ai::AssistantContentBlock::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "read".to_string(),
                arguments: [("path".to_string(), serde_json::json!("src/foo.rs"))]
                    .into_iter()
                    .collect(),
                arguments_error: None,
                thought_signature: None,
            }));
        ast.content
            .push(mehh_ai::AssistantContentBlock::ToolCall(ToolCall {
                id: "c2".to_string(),
                name: "edit".to_string(),
                arguments: [("path".to_string(), serde_json::json!("src/bar.rs"))]
                    .into_iter()
                    .collect(),
                arguments_error: None,
                thought_signature: None,
            }));
        entries.push(SessionEntry {
            id: "e2".to_string(),
            parent_id: Some("e1".to_string()),
            timestamp: "2026-11-15T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(ast),
            },
        });

        let prep = prepare_branch_entries(&entries, 100_000);
        assert_eq!(prep.messages.len(), 2);
        assert_eq!(prep.read_files, vec!["src/foo.rs"]);
        assert_eq!(prep.modified_files, vec!["src/bar.rs"]);
        assert!(prep.total_tokens > 0);

        // Very small budget should trim older entries
        let prep_trimmed = prepare_branch_entries(&entries, 10);
        assert!(prep_trimmed.messages.len() <= 1);
    }

    #[test]
    fn prepare_branch_entries_includes_compaction_and_skips_dropped_file_ops() {
        let mut old_ast = mehh_ai::faux_assistant_message("z".repeat(400));
        old_ast
            .content
            .push(mehh_ai::AssistantContentBlock::ToolCall(ToolCall {
                id: "c_old".to_string(),
                name: "edit".to_string(),
                arguments: [("path".to_string(), serde_json::json!("src/dropped.rs"))]
                    .into_iter()
                    .collect(),
                arguments_error: None,
                thought_signature: None,
            }));

        let entries = vec![
            SessionEntry {
                id: "e1".to_string(),
                parent_id: None,
                timestamp: "2026-11-15T10:00:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::Assistant(old_ast),
                },
            },
            SessionEntry {
                id: "c1".to_string(),
                parent_id: Some("e1".to_string()),
                timestamp: "2026-11-15T10:01:00.000Z".to_string(),
                kind: SessionEntryKind::Compaction {
                    summary: "compacted checkpoint".to_string(),
                    first_kept_entry_id: "e1".to_string(),
                    tokens_before: 500,
                },
            },
        ];

        // Budget allows c1 (with overrun allowance) but drops e1:
        // e1's file ops must NOT leak into modified_files, and c1 must be included.
        let prep = prepare_branch_entries(&entries, 50);
        assert_eq!(prep.messages.len(), 1);
        assert!(prep.modified_files.is_empty());
        let conv = serialize_conversation(&prep.messages);
        assert!(conv.contains("compacted checkpoint"));
    }

    #[test]
    fn collect_entries_for_branch_summary_terminates_on_parent_cycle() {
        let dir = std::env::temp_dir().join(format!("mehh_bs_cycle_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cyclic.jsonl");
        {
            let mut s = SessionManager::open_file(&file, &dir).unwrap();
            s.append_message(Message::User(UserMessage {
                content: UserContent::Text("one".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        }
        // Rewrite with cyclic parentId links and reopen.
        let raw = std::fs::read_to_string(&file).unwrap();
        let header_line = raw.lines().next().unwrap();
        let e1 = serde_json::json!({
            "id": "n1",
            "parentId": "n2",
            "timestamp": "2026-11-15T10:00:00.000Z",
            "type": "message",
            "message": Message::User(UserMessage { content: UserContent::Text("a".into()), timestamp: 1 })
        });
        let e2 = serde_json::json!({
            "id": "n2",
            "parentId": "n1",
            "timestamp": "2026-11-15T10:01:00.000Z",
            "type": "message",
            "message": Message::User(UserMessage { content: UserContent::Text("b".into()), timestamp: 2 })
        });
        std::fs::write(&file, format!("{header_line}\n{e1}\n{e2}\n")).unwrap();
        let session = SessionManager::open_existing_file(&file, dir.clone()).unwrap();
        let res = collect_entries_for_branch_summary(&session, Some("n1"), "n2");
        assert!(res.entries.len() <= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn generate_branch_summary_clamps_small_model_budget_and_handles_oversized_last_entry() {
        let mut model = mehh_ai::faux_model();
        model.context_window = 8192; // <= 16k: previously saturating_sub(16_384) == 0 (unlimited!)

        let old_marker = "OLD_OUT_OF_BUDGET_MARKER";
        let entries = vec![
            SessionEntry {
                id: "e1".to_string(),
                parent_id: None,
                timestamp: "2026-11-15T10:00:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text(format!("{old_marker} {}", "a".repeat(20_000))),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "e2".to_string(),
                parent_id: Some("e1".to_string()),
                timestamp: "2026-11-15T10:01:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("RECENT_MARKER".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
        ];

        let faux = mehh_ai::FauxProvider::with_responses(vec![
            mehh_ai::faux_assistant_message("## Goal\nTrimmed summary"),
        ]);
        let summary = generate_branch_summary(
            &entries,
            &model,
            &faux,
            None,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(summary.contains("Trimmed summary"));
        let req = faux.recorded_requests();
        let prompt = match &req[0].context.messages[0] {
            Message::User(u) => u.content.text(),
            _ => panic!("expected user"),
        };
        assert!(prompt.contains("RECENT_MARKER"));
        assert!(
            !prompt.contains(old_marker),
            "<= 16k model must not treat 0 budget as unlimited: {prompt}"
        );

        // Oversized single last entry must NOT return "No content to summarize"
        let oversized = vec![SessionEntry {
            id: "big".to_string(),
            parent_id: None,
            timestamp: "2026-11-15T10:02:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("x".repeat(40_000)),
                    timestamp: now_ms(),
                }),
            },
        }];
        let faux2 = mehh_ai::FauxProvider::with_responses(vec![
            mehh_ai::faux_assistant_message("## Goal\nOversized handled"),
        ]);
        let sum2 = generate_branch_summary(
            &oversized,
            &model,
            &faux2,
            None,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_ne!(sum2, "No content to summarize");
        assert!(sum2.contains("Oversized handled"));
    }

    #[test]
    fn prepare_branch_entries_applies_context_edits() {
        let entries = vec![
            SessionEntry {
                id: "u1".to_string(),
                parent_id: None,
                timestamp: "2026-11-15T10:00:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("secret hidden".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "u2".to_string(),
                parent_id: Some("u1".to_string()),
                timestamp: "2026-11-15T10:01:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("original".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "ce1".to_string(),
                parent_id: Some("u2".to_string()),
                timestamp: "2026-11-15T10:02:00.000Z".to_string(),
                kind: SessionEntryKind::ContextEdit {
                    target_id: "u1".to_string(),
                    replacement: None,
                },
            },
            SessionEntry {
                id: "ce2".to_string(),
                parent_id: Some("ce1".to_string()),
                timestamp: "2026-11-15T10:03:00.000Z".to_string(),
                kind: SessionEntryKind::ContextEdit {
                    target_id: "u2".to_string(),
                    replacement: Some(crate::session::ContextEditReplacement {
                        content: crate::session::ContextEditableContent::Text(
                            "edited replacement".to_string(),
                        ),
                    }),
                },
            },
        ];

        let prep = prepare_branch_entries(&entries, 100_000);
        assert_eq!(prep.messages.len(), 1);
        match &prep.messages[0] {
            Message::User(u) => assert_eq!(u.content.text(), "edited replacement"),
            other => panic!("expected user message, got {other:?}"),
        }
    }
}
