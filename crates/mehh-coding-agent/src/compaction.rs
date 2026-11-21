//! Context compaction and history summarization for long agent sessions.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use mehh_agent::types::tool_to_definition;
use mehh_agent::AgentTool;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use mehh_ai::{
    now_ms, AssistantContentBlock, Context, LlmProvider, Message, Model, StopReason, StreamOptions,
    UserContent, UserMessage,
};

use crate::session::{SessionEntry, SessionEntryKind};

/// Default token threshold retained at the tail of the active branch.
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;

/// Default headroom reserved before `context_window` to trigger proactive auto-compaction.
pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;

/// Maximum character limit for tool results in serialized conversation transcripts.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Budget settings controlling proactive and overflow-triggered compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: usize,
    pub keep_recent_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

impl CompactionSettings {
    /// Scales reserve and keep-recent budgets proportionally for smaller context windows.
    pub fn for_context_window(context_window: u64, enabled: bool) -> Self {
        let cw = usize::try_from(context_window).unwrap_or(usize::MAX);
        if cw == 0 {
            return Self {
                enabled,
                ..Self::default()
            };
        }
        let reserve_tokens = DEFAULT_RESERVE_TOKENS.min(cw / 5).max(1024);
        let keep_recent_tokens = DEFAULT_KEEP_RECENT_TOKENS.min(cw / 4).max(2048);
        Self {
            enabled,
            reserve_tokens,
            keep_recent_tokens,
        }
    }
}

/// Returns true when auto-compaction is enabled and `context_tokens >= context_window - reserve_tokens`.
pub fn should_auto_compact(
    context_tokens: usize,
    context_window: usize,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled
        && context_window > 0
        && context_tokens >= context_window.saturating_sub(settings.reserve_tokens)
}

/// Estimates total token usage across a slice of conversation messages.
pub fn estimate_context_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

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

struct ByteCountWriter(usize);

impl std::io::Write for ByteCountWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCountWriter(0);
    if serde_json::to_writer(&mut counter, value).is_ok() {
        counter.0
    } else {
        0
    }
}

/// Estimated token count for a message using the `chars.div_ceil(4)` heuristic.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let chars = match msg {
        Message::System(s) => s.content.chars().count(),
        Message::User(u) => match &u.content {
            UserContent::Text(t) => t.chars().count(),
            UserContent::Blocks(blocks) => {
                let mut sum = 0;
                for b in blocks {
                    match b {
                        mehh_ai::UserContentBlock::Text(t) => sum += t.text.chars().count(),
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
                    AssistantContentBlock::Text(t) => sum += t.text.chars().count(),
                    AssistantContentBlock::Thinking(th) => sum += th.thinking.chars().count(),
                    AssistantContentBlock::ToolCall(tc) => {
                        sum += tc.name.len() + json_serialized_len(&tc.arguments);
                    }
                }
            }
            sum
        }
        Message::ToolResult(tr) => {
            let mut sum = 0;
            for b in &tr.content {
                match b {
                    mehh_ai::ToolResultContentBlock::Text(t) => sum += t.text.chars().count(),
                    mehh_ai::ToolResultContentBlock::Image(_) => sum += 4800,
                }
            }
            // `details` are session metadata only; they are never serialized to the LLM.
            sum
        }
    };
    chars.div_ceil(4)
}

/// Token estimate for tool schemas, which occupy context alongside the conversation.
pub fn estimate_tool_definitions_tokens(tools: &[Arc<dyn AgentTool>]) -> usize {
    tools
        .iter()
        .map(|t| json_serialized_len(&tool_to_definition(t.as_ref())))
        .sum::<usize>()
        .div_ceil(4)
}

/// Projects the entry kinds that `SessionManager::build_context_messages` sends to the LLM.
fn messages_for_entry(entry: &SessionEntry) -> Vec<Message> {
    match &entry.kind {
        SessionEntryKind::Message { message } => vec![message.clone()],
        SessionEntryKind::CustomMessage { content, .. } => vec![Message::User(UserMessage {
            content: content.clone(),
            timestamp: now_ms(),
        })],
        SessionEntryKind::BranchSummary { summary, .. } => vec![Message::User(UserMessage {
            content: UserContent::Text(format!(
                "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
            )),
            timestamp: now_ms(),
        })],
        _ => Vec::new(),
    }
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

                let mut char_iter = content_text.char_indices();
                let truncated = if let Some((byte_end, _)) = char_iter.nth(TOOL_RESULT_MAX_CHARS) {
                    let remaining = 1 + char_iter.count();
                    format!(
                        "{}\n\n[... {remaining} more characters truncated]",
                        &content_text[..byte_end]
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
    let last_compaction_pos = entries
        .iter()
        .rposition(|e| matches!(&e.kind, SessionEntryKind::Compaction { .. }));

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
        if messages_for_entry(entry)
            .iter()
            .any(|message| is_cut_point_message(message))
        {
            cut_point_candidates.push(i);
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
        tokens_before += messages_for_entry(entry)
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
    }

    // Walk backwards from the tail, accumulating tokens until >= keep_recent_tokens
    let mut accumulated_tokens = 0;
    let mut cut_index = cut_point_candidates[0];
    let mut budget_exceeded = false;

    for i in (boundary_start..entries.len()).rev() {
        for msg in &messages_for_entry(&entries[i]) {
            accumulated_tokens += estimate_message_tokens(msg);
        }
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

    if !budget_exceeded {
        // If the entire history fits within keep_recent_tokens, cut at first message if > 1 message
        if cut_point_candidates.len() <= 1 {
            return None;
        }
        cut_index = cut_point_candidates[0];
    }

    // Check if the cut splits a turn (cut point is Assistant message).
    // Always align kept history on a User message so post-compaction context never starts with Assistant.
    let cut_entry = &entries[cut_index];
    let is_assistant_cut = matches!(
        &cut_entry.kind,
        SessionEntryKind::Message {
            message: Message::Assistant(_)
        }
    );

    let first_kept_index = if is_assistant_cut {
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
            Some(ts) if ts > boundary_start => ts,
            _ => entries[cut_index + 1..]
                .iter()
                .position(|e| {
                    matches!(
                        &e.kind,
                        SessionEntryKind::Message {
                            message: Message::User(_)
                        }
                    )
                })
                .map(|rel| cut_index + 1 + rel)?,
        }
    } else {
        cut_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &entries[boundary_start..first_kept_index] {
        messages_to_summarize.extend(messages_for_entry(entry));
    }

    let turn_prefix_messages = Vec::new();

    if messages_to_summarize.is_empty() {
        return None;
    }

    let first_kept_entry = &entries[first_kept_index];

    // Extract file operations from read, write, and edit tool calls
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    for msg in messages_to_summarize
        .iter()
        .chain(turn_prefix_messages.iter())
    {
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
        first_kept_entry_id: first_kept_entry.id.clone(),
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
    let mut serialized_conv = serialize_conversation(&prep.messages_to_summarize);
    // A reactive overflow compaction starts from a context that is already too
    // large. Clamp the summarization payload so the summary request itself
    // cannot overflow the same window.
    let budget_chars = (model.context_window as usize)
        .saturating_sub(8192)
        .saturating_mul(3)
        .max(4096);
    if serialized_conv.chars().count() > budget_chars {
        if let Some((byte_end, _)) = serialized_conv.char_indices().nth(budget_chars) {
            serialized_conv.truncate(byte_end);
            serialized_conv.push_str("\n\n[... older history truncated for summarization]");
        }
    }

    let mut user_prompt = String::new();
    if let Some(prev) = &prep.previous_summary {
        user_prompt.push_str(&format!(
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        ));
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

    if matches!(
        final_msg.stop_reason,
        StopReason::Error | StopReason::Aborted
    ) {
        let err = final_msg
            .error_message
            .unwrap_or_else(|| format!("Compaction stopped with {:?}", final_msg.stop_reason));
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
        now_ms, AssistantContentBlock, AssistantMessage, FauxProvider, ModelCost, StopReason,
        TextContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
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
            ..Default::default()
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
        read_args.insert(
            "path".to_string(),
            serde_json::Value::String("src/foo.rs".to_string()),
        );
        let mut write_args = serde_json::Map::new();
        write_args.insert(
            "path".to_string(),
            serde_json::Value::String("src/bar.rs".to_string()),
        );

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
                    duration_ms: None,
                    usage: None,
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
    async fn compact_with_provider_caps_oversized_history() {
        let mut model = make_test_model();
        model.context_window = 4096;
        let prep = CompactionPreparation {
            first_kept_entry_id: "e2".to_string(),
            messages_to_summarize: vec![Message::User(UserMessage {
                content: UserContent::Text("x".repeat(200_000)),
                timestamp: now_ms(),
            })],
            turn_prefix_messages: Vec::new(),
            tokens_before: 50_000,
            previous_summary: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        };
        let faux = FauxProvider::new();
        faux.add_response(mehh_ai::faux_assistant_message("## Goal\nSmall summary"));
        compact_with_provider(&prep, &model, &faux, None, None, CancellationToken::new())
            .await
            .unwrap();
        let requests = faux.recorded_requests();
        let request = requests.last().expect("compaction request recorded");
        let text = match &request.context.messages[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text prompt"),
            },
            _ => panic!("expected user prompt"),
        };
        assert!(
            text.len() < 20_000,
            "summarization prompt grew to {} chars",
            text.len()
        );
    }

    #[test]
    fn estimate_message_tokens_uses_chars_not_utf8_bytes() {
        let msg = Message::User(UserMessage {
            content: UserContent::Text("é".repeat(400)),
            timestamp: now_ms(),
        });
        assert_eq!(estimate_message_tokens(&msg), 100);
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
                text: "## Goal\nBuild a parser.\n\n## Progress\n### Done\n- [x] Parser built"
                    .to_string(),
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

        let result =
            compact_with_provider(&prep, &model, &faux, None, Some("Keep it brief"), cancel)
                .await
                .unwrap();

        assert!(result.contains("## Goal\nBuild a parser."));
        assert!(result.contains("<read-files>\nparser.rs\n</read-files>"));
        assert!(result.contains("<modified-files>\nmain.rs\n</modified-files>"));
    }

    #[test]
    fn test_compaction_non_ascii_truncation_and_assistant_cut_starts_with_user() {
        // Non-ASCII tool output around byte 2000 must not panic when serialized for compaction.
        let mut big = "a".repeat(TOOL_RESULT_MAX_CHARS - 2);
        big.push_str("你好世界🙂extra");
        let serialized =
            serialize_conversation(&[Message::ToolResult(mehh_ai::ToolResultMessage {
                tool_call_id: "c1".to_string(),
                tool_name: "read".to_string(),
                content: vec![mehh_ai::ToolResultContentBlock::Text(
                    mehh_ai::TextContent {
                        text: big,
                        text_signature: None,
                    },
                )],
                details: None,
                is_error: false,
                duration_ms: None,
                usage: None,
                timestamp: now_ms(),
            })]);
        assert!(serialized.contains("more characters truncated"));

        // Cutting on an assistant entry mid-turn must anchor first_kept_entry_id on that turn's User entry.
        let long_text = "word ".repeat(12_000);
        let mk_user = |id: &str, parent: Option<&str>, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: now_ms(),
                }),
            },
        };
        let mk_ast = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(mehh_ai::faux_assistant_message(text)),
            },
        };

        let entries = vec![
            mk_user("u1", None, "first turn"),
            mk_ast("a1", "u1", "first reply"),
            mk_user("u2", Some("a1"), "second turn question"),
            mk_ast("a2", "u2", &long_text),
            mk_ast("a3", "a2", &long_text),
        ];

        let prep = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS).unwrap();
        assert_eq!(prep.first_kept_entry_id, "u2");
        assert_eq!(prep.messages_to_summarize.len(), 2);
        assert!(prep.turn_prefix_messages.is_empty());
    }

    #[test]
    fn test_compaction_cut_splits_turn_advances_to_next_user_when_turn_start_is_boundary() {
        let mk_user = |id: &str, parent: Option<&str>, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: now_ms(),
                }),
            },
        };
        let mk_ast = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(mehh_ai::faux_assistant_message(text)),
            },
        };

        let long_reply = "A".repeat(4000);
        let entries = vec![
            mk_user("u1", None, "initial prompt"),
            mk_ast("a1", "u1", &long_reply),
            mk_user("u2", Some("a1"), "follow-up prompt"),
            mk_ast("a2", "u2", "short reply"),
        ];

        // Budget is small (50 tokens) so walking backwards from a2 reaches a1 and exceeds budget.
        // a1 is an assistant message whose turn_start is u1 (index 0 == boundary_start).
        // It must advance to u2 instead of returning None.
        let prep = prepare_compaction(&entries, 50)
            .expect("Compaction should succeed by advancing to next User turn");
        assert_eq!(prep.first_kept_entry_id, "u2");
        assert_eq!(prep.messages_to_summarize.len(), 2);
    }

    #[test]
    fn test_compaction_settings_and_should_auto_compact() {
        let small = CompactionSettings::for_context_window(8_000, true);
        assert!(small.reserve_tokens <= 1600);
        assert!(small.keep_recent_tokens >= 2048);

        let default_settings = CompactionSettings::default();
        assert!(default_settings.enabled);
        assert!(should_auto_compact(190_000, 200_000, &default_settings));
        assert!(!should_auto_compact(100_000, 200_000, &default_settings));
        assert!(!should_auto_compact(
            190_000,
            200_000,
            &CompactionSettings {
                enabled: false,
                ..default_settings
            }
        ));
    }
}
