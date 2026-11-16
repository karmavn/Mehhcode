//! Automatic branch summarization for session tree navigation.

use std::collections::{BTreeSet, HashSet};
use anyhow::{anyhow, Result};
use tokio_util::sync::CancellationToken;

use mehh_ai::{
    Context, LlmProvider, Message, Model, UserContent, UserMessage,
};
use crate::compaction::{
    estimate_message_tokens, serialize_conversation, SUMMARIZATION_SYSTEM_PROMPT,
};
use crate::session::{SessionEntry, SessionEntryKind, SessionManager};

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

    let mut old_path = Vec::new();
    let mut curr = Some(old_leaf);
    let mut old_path_set = HashSet::new();

    while let Some(id) = curr {
        if let Some(entry) = session.get_entry(id) {
            old_path.push(entry.clone());
            old_path_set.insert(id.to_string());
            curr = entry.parent_id.as_deref();
        } else {
            break;
        }
    }

    let mut target_path = Vec::new();
    let mut curr_t = Some(target_id);
    while let Some(id) = curr_t {
        if let Some(entry) = session.get_entry(id) {
            target_path.push(entry.clone());
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
        entries.push(entry);
    }
    entries.reverse();

    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

/// Prepares branch entries into messages, trims to token budget, and extracts read/modified file paths.
pub fn prepare_branch_entries(
    entries: &[SessionEntry],
    token_budget: usize,
) -> BranchPreparation {
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    let mut messages = Vec::new();
    let mut total_tokens = 0;

    for entry in entries.iter().rev() {
        let entry_msgs = match &entry.kind {
            SessionEntryKind::Message { message } => vec![message.clone()],
            SessionEntryKind::CustomMessage { content, .. } => {
                let ts = crate::session::parse_iso8601_or_now(&entry.timestamp);
                vec![Message::User(UserMessage {
                    content: content.clone(),
                    timestamp: ts,
                })]
            }
            SessionEntryKind::BranchSummary { summary, .. } => {
                let ts = crate::session::parse_iso8601_or_now(&entry.timestamp);
                vec![Message::User(UserMessage {
                    content: UserContent::Text(format!(
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
                    )),
                    timestamp: ts,
                })]
            }
            _ => Vec::new(),
        };

        if entry_msgs.is_empty() {
            continue;
        }

        let mut entry_tokens = 0;
        for msg in &entry_msgs {
            entry_tokens += estimate_message_tokens(msg);

            if let Message::Assistant(ast) = msg {
                for call in ast.tool_calls() {
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

        if token_budget > 0 && total_tokens + entry_tokens > token_budget {
            if matches!(
                &entry.kind,
                SessionEntryKind::Compaction { .. } | SessionEntryKind::BranchSummary { .. }
            ) && total_tokens < (token_budget * 9 / 10)
            {
                for msg in entry_msgs.into_iter().rev() {
                    messages.insert(0, msg);
                }
                total_tokens += entry_tokens;
            }
            break;
        }

        for msg in entry_msgs.into_iter().rev() {
            messages.insert(0, msg);
        }
        total_tokens += entry_tokens;
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
    let token_budget = context_window.saturating_sub(reserve);

    let prep = prepare_branch_entries(entries, token_budget);
    if prep.messages.is_empty() {
        return Ok("No content to summarize".to_string());
    }

    let conversation_text = serialize_conversation(&prep.messages);
    let instructions = if let Some(ci) = custom_instructions {
        if !ci.trim().is_empty() {
            format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {}", ci.trim())
        } else {
            BRANCH_SUMMARY_PROMPT.to_string()
        }
    } else {
        BRANCH_SUMMARY_PROMPT.to_string()
    };

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

    if matches!(final_msg.stop_reason, mehh_ai::StopReason::Error | mehh_ai::StopReason::Aborted) {
        let err = final_msg
            .error_message
            .unwrap_or_else(|| format!("Branch summarization stopped with {:?}", final_msg.stop_reason));
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
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message("Working on task")))
            .unwrap();

        // Branch A (old branch): m2 -> m3
        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Branch A step 1".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let m3 = session
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message("Branch A step 2 done")))
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
        ast.content.push(mehh_ai::AssistantContentBlock::ToolCall(ToolCall {
            id: "c1".to_string(),
            name: "read".to_string(),
            arguments: [("path".to_string(), serde_json::json!("src/foo.rs"))]
                .into_iter()
                .collect(),
            arguments_error: None,
            thought_signature: None,
        }));
        ast.content.push(mehh_ai::AssistantContentBlock::ToolCall(ToolCall {
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
}
