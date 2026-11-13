//! Context compaction and history summarization for long agent sessions.

use std::collections::BTreeSet;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use mehh_ai::{
    now_ms, AssistantContentBlock, Context, LlmProvider, Message, Model,
    StopReason, StreamOptions, UserContent, UserMessage,
};

use crate::session::{SessionEntry, SessionEntryKind};

/// Default token threshold retained at the tail of the active branch.
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;

/// Maximum character limit for tool results in serialized conversation transcripts.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// System prompt used for the summarization LLM call.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// Prompt used when generating a fresh summary.
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Prompt used when updating an existing checkpoint summary.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Estimated token count for a message using the `chars.div_ceil(4)` heuristic.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let chars = match msg {
        Message::System(s) => s.content.len(),
        Message::User(u) => match &u.content {
            UserContent::Text(t) => t.len(),
            UserContent::Blocks(blocks) => {
                let mut sum = 0;
                for b in blocks {
                    match b {
                        mehh_ai::UserContentBlock::Text(t) => sum += t.text.len(),
                        mehh_ai::UserContentBlock::Image(_) => sum += 4800,
                    }
                }
                sum
            }
        },
        Message::Assistant(a) => {
            let mut sum = 0;
            for b in &a.content {
                match b {
                    AssistantContentBlock::Text(t) => sum += t.text.len(),
                    AssistantContentBlock::Thinking(th) => sum += th.thinking.len(),
                    AssistantContentBlock::ToolCall(tc) => {
                        sum += tc.name.len();
                        if let Ok(s) = serde_json::to_string(&tc.arguments) {
                            sum += s.len();
                        }
                    }
                }
            }
            sum
        }
        Message::ToolResult(tr) => {
            let mut sum = 0;
            for b in &tr.content {
                match b {
                    mehh_ai::ToolResultContentBlock::Text(t) => sum += t.text.len(),
                    mehh_ai::ToolResultContentBlock::Image(_) => sum += 4800,
                }
            }
            if let Some(details) = &tr.details {
                if let Ok(s) = serde_json::to_string(details) {
                    sum += s.len();
                }
            }
            sum
        }
    };
    chars.div_ceil(4)
}

/// Prepared compaction payload containing the cut point, messages to summarize,
/// token estimates, and touched file lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<Message>,
    pub turn_prefix_messages: Vec<Message>,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Identifies whether a message is an eligible cut point boundary (User or Assistant, never ToolResult).
fn is_cut_point_message(msg: &Message) -> bool {
    matches!(msg, Message::User(_) | Message::Assistant(_))
}

/// Formats a conversation into a serialized text transcript for summarization.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();

    for msg in messages {
        match msg {
            Message::User(u) => {
                let text = u.content.text();
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(a) => {
                let text = a.text_content();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }

                let tool_calls = a.tool_calls();
                if !tool_calls.is_empty() {
                    let calls_str = tool_calls
                        .iter()
                        .map(|tc| {
                            let args_str = tc
                                .arguments
                                .iter()
                                .map(|(k, v)| {
                                    format!("{k}={}", serde_json::to_string(v).unwrap_or_default())
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{}({})", tc.name, args_str)
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    parts.push(format!("[Assistant tool calls]: {calls_str}"));
                }
            }
            Message::ToolResult(tr) => {
                let content_text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        mehh_ai::ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                        mehh_ai::ToolResultContentBlock::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let truncated = if content_text.len() > TOOL_RESULT_MAX_CHARS {
                    let remaining = content_text.len() - TOOL_RESULT_MAX_CHARS;
                    format!(
                        "{}\n\n[... {remaining} more characters truncated]",
                        &content_text[..TOOL_RESULT_MAX_CHARS]
                    )
                } else {
                    content_text
                };

                if !truncated.is_empty() {
                    parts.push(format!("[Tool result]: {truncated}"));
                }
            }
            Message::System(_) => {}
        }
    }

    parts.join("\n\n")
}

/// Evaluates active session entries and prepares a compaction cut point if history exceeds the budget.
pub fn prepare_compaction(
    entries: &[SessionEntry],
    keep_recent_tokens: usize,
) -> Option<CompactionPreparation> {
    if entries.is_empty() {
        return None;
    }

    // Locate latest compaction boundary
    let last_compaction_pos = entries.iter().rposition(|e| {
        matches!(&e.kind, SessionEntryKind::Compaction { .. })
    });

    let (boundary_start, previous_summary) = match last_compaction_pos {
        Some(pos) => {
            if let SessionEntryKind::Compaction { summary, .. } = &entries[pos].kind {
                (pos + 1, Some(summary.clone()))
            } else {
                (0, None)
            }
        }
        None => (0, None),
    };

    if boundary_start >= entries.len() {
        return None;
    }

    // Collect valid cut point candidate indices: Message::User or Message::Assistant
    let mut cut_point_candidates = Vec::new();
    for (i, entry) in entries.iter().enumerate().skip(boundary_start) {
        if let SessionEntryKind::Message { message } = &entry.kind {
            if is_cut_point_message(message) {
                cut_point_candidates.push(i);
            }
        }
    }

    if cut_point_candidates.is_empty() {
        return None;
    }

    // Calculate total tokens before compaction
    let mut tokens_before: usize = 0;
    if let Some(prev) = &previous_summary {
        tokens_before += prev.len().div_ceil(4);
    }
    for entry in &entries[boundary_start..] {
        if let SessionEntryKind::Message { message } = &entry.kind {
            tokens_before += estimate_message_tokens(message);
        }
    }

    // Walk backwards from the tail, accumulating tokens until >= keep_recent_tokens
    let mut accumulated_tokens = 0;
    let mut cut_index = cut_point_candidates[0];
    let mut budget_exceeded = false;

    for i in (boundary_start..entries.len()).rev() {
        if let SessionEntryKind::Message { message } = &entries[i].kind {
            accumulated_tokens += estimate_message_tokens(message);
            if accumulated_tokens >= keep_recent_tokens {
                budget_exceeded = true;
                // Snap to the closest candidate at or after index i
                cut_index = cut_point_candidates
                    .iter()
                    .copied()
                    .find(|&c| c >= i)
                    .unwrap_or(*cut_point_candidates.last().unwrap());
                break;
            }
        }
    }

    if !budget_exceeded {
        // If the entire history fits within keep_recent_tokens, cut at first message if > 1 message
        if cut_point_candidates.len() <= 1 {
            return None;
        }
        cut_index = cut_point_candidates[0];
    }

    // Check if the cut splits a turn (cut point is Assistant message)
    let cut_entry = &entries[cut_index];
    let is_assistant_cut = matches!(
        &cut_entry.kind,
        SessionEntryKind::Message {
            message: Message::Assistant(_)
        }
    );

    let (history_end, turn_start_index) = if is_assistant_cut {
        // Find the user message starting this turn
        let mut turn_start = None;
        for i in (boundary_start..=cut_index).rev() {
            if let SessionEntryKind::Message {
                message: Message::User(_),
            } = &entries[i].kind
            {
                turn_start = Some(i);
                break;
            }
        }
        match turn_start {
            Some(ts) => (ts, Some(ts)),
            None => (cut_index, None),
        }
    } else {
        (cut_index, None)
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &entries[boundary_start..history_end] {
        if let SessionEntryKind::Message { message } = &entry.kind {
            messages_to_summarize.push(message.clone());
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if let Some(ts) = turn_start_index {
        for entry in &entries[ts..cut_index] {
            if let SessionEntryKind::Message { message } = &entry.kind {
                turn_prefix_messages.push(message.clone());
            }
        }
    }

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    // Extract file operations from read, write, and edit tool calls
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    for msg in messages_to_summarize.iter().chain(turn_prefix_messages.iter()) {
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

    let read_files: Vec<String> = read_files_set
        .into_iter()
        .filter(|f| !modified_files_set.contains(f))
        .collect();
    let modified_files: Vec<String> = modified_files_set.into_iter().collect();

    Some(CompactionPreparation {
        first_kept_entry_id: cut_entry.id.clone(),
        messages_to_summarize,
        turn_prefix_messages,
        tokens_before: tokens_before as u64,
        previous_summary,
        read_files,
        modified_files,
    })
}

/// Calls an LLM provider to produce a structured summary of the prepared messages,
/// appending `<read-files>` and `<modified-files>` metadata blocks.
pub async fn compact_with_provider(
    prep: &CompactionPreparation,
    model: &Model,
    provider: &dyn LlmProvider,
    api_key: Option<&str>,
    custom_instructions: Option<&str>,
    cancel: CancellationToken,
) -> Result<String> {
    let serialized_conv = serialize_conversation(&prep.messages_to_summarize);

    let mut user_prompt = String::new();
    if let Some(prev) = &prep.previous_summary {
        user_prompt.push_str(&format!("<previous-summary>\n{prev}\n</previous-summary>\n\n"));
    }

    user_prompt.push_str(&serialized_conv);
    user_prompt.push_str("\n\n");

    if prep.previous_summary.is_some() {
        user_prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        user_prompt.push_str(SUMMARIZATION_PROMPT);
    }

    if let Some(ci) = custom_instructions {
        if !ci.trim().is_empty() {
            user_prompt.push_str(&format!("\n\nCustom Instructions:\n{}", ci.trim()));
        }
    }

    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(user_prompt),
            timestamp: now_ms(),
        })],
        tools: None,
    };

    let options = StreamOptions {
        api_key: api_key.map(|s| s.to_string()),
        ..Default::default()
    };

    let mut stream = provider.stream(model, &context, &options, cancel);
    let final_msg = stream.collect_final_message().await?;

    if final_msg.stop_reason == StopReason::Error {
        let err = final_msg
            .error_message
            .unwrap_or_else(|| "Unknown error from model".to_string());
        return Err(anyhow!("Compaction model stream failed: {err}"));
    }

    let mut summary = final_msg.text_content().trim().to_string();

    if !prep.read_files.is_empty() {
        summary.push_str("\n\n<read-files>\n");
        summary.push_str(&prep.read_files.join("\n"));
        summary.push_str("\n</read-files>");
    }

    if !prep.modified_files.is_empty() {
        summary.push_str("\n\n<modified-files>\n");
        summary.push_str(&prep.modified_files.join("\n"));
        summary.push_str("\n</modified-files>");
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{
        now_ms, AssistantContentBlock, AssistantMessage, FauxProvider, ModelCost,
        StopReason, TextContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
    };

    fn make_test_model() -> Model {
        Model {
            id: "faux-test".to_string(),
            name: "Faux Test".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
        }
    }

    #[test]
    fn test_prepare_compaction_cut_point_and_file_ops() {
        let mut entries = Vec::new();

        // 1. User message
        entries.push(SessionEntry {
            id: "e1".to_string(),
            parent_id: None,
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Initial user prompt".to_string()),
                    timestamp: now_ms(),
                }),
            },
        });

        // 2. Assistant tool call (read foo.rs and write bar.rs)
        let mut read_args = serde_json::Map::new();
        read_args.insert("path".to_string(), serde_json::Value::String("src/foo.rs".to_string()));
        let mut write_args = serde_json::Map::new();
        write_args.insert("path".to_string(), serde_json::Value::String("src/bar.rs".to_string()));

        entries.push(SessionEntry {
            id: "e2".to_string(),
            parent_id: Some("e1".to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_1".to_string(),
                            name: "read".to_string(),
                            arguments: read_args,
                            arguments_error: None,
                            thought_signature: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_2".to_string(),
                            name: "write".to_string(),
                            arguments: write_args,
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            },
        });

        // 3. Tool results (must never be chosen as cut point)
        entries.push(SessionEntry {
            id: "e3".to_string(),
            parent_id: Some("e2".to_string()),
            timestamp: "2026-11-12T10:02:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![mehh_ai::ToolResultContentBlock::Text(TextContent {
                        text: "Contents of foo.rs".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: now_ms(),
                }),
            },
        });

        // 4. User follow-up
        entries.push(SessionEntry {
            id: "e4".to_string(),
            parent_id: Some("e3".to_string()),
            timestamp: "2026-11-12T10:03:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Second turn user question".to_string()),
                    timestamp: now_ms(),
                }),
            },
        });

        // 5. Assistant reply
        entries.push(SessionEntry {
            id: "e5".to_string(),
            parent_id: Some("e4".to_string()),
            timestamp: "2026-11-12T10:04:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Done with the second turn.".to_string(),
                        text_signature: None,
                    })],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            },
        });

        // Prepare compaction with small keep_recent_tokens so it cuts before e4
        let prep = prepare_compaction(&entries, 10).expect("Should prepare compaction");
        assert_eq!(prep.first_kept_entry_id, "e4");
        assert_eq!(prep.read_files, vec!["src/foo.rs"]);
        assert_eq!(prep.modified_files, vec!["src/bar.rs"]);
        assert!(prep.messages_to_summarize.len() >= 2);
    }

    #[tokio::test]
    async fn test_compact_with_provider_faux() {
        let prep = CompactionPreparation {
            first_kept_entry_id: "entry_123".to_string(),
            messages_to_summarize: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Build me a parser".to_string()),
                    timestamp: now_ms(),
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "I built the parser".to_string(),
                        text_signature: None,
                    })],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            ],
            turn_prefix_messages: vec![],
            tokens_before: 500,
            previous_summary: None,
            read_files: vec!["parser.rs".to_string()],
            modified_files: vec!["main.rs".to_string()],
        };

        let faux = FauxProvider::new();
        faux.add_response(AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "## Goal\nBuild a parser.\n\n## Progress\n### Done\n- [x] Parser built".to_string(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-test".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        });

        let model = make_test_model();
        let cancel = CancellationToken::new();

        let result = compact_with_provider(
            &prep,
            &model,
            &faux,
            None,
            Some("Keep it brief"),
            cancel,
        )
        .await
        .unwrap();

        assert!(result.contains("## Goal\nBuild a parser."));
        assert!(result.contains("<read-files>\nparser.rs\n</read-files>"));
        assert!(result.contains("<modified-files>\nmain.rs\n</modified-files>"));
    }
}
