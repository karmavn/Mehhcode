//! Context compaction and history summarization for long agent sessions.

use std::collections::{BTreeSet, HashMap};
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

use crate::session::{ContextEditReplacement, SessionEntry, SessionEntryKind};

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

/// Projects the entry kinds that `SessionManager::build_context_messages` sends to the LLM,
/// applying any active-path `ContextEdit` omissions or replacements.
fn messages_for_entry_with_edits(
    entry: &SessionEntry,
    edits: &HashMap<&str, &Option<ContextEditReplacement>>,
) -> Option<Message> {
    if let Some(edit) = edits.get(entry.id.as_str()) {
        if edit.is_none() {
            return None;
        }
    }
    let replacement = edits.get(entry.id.as_str()).and_then(|r| r.as_ref());

    match &entry.kind {
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
            Some(Message::User(UserMessage {
                content: effective_content,
                timestamp: now_ms(),
            }))
        }
        SessionEntryKind::BranchSummary { summary, .. } => Some(Message::User(UserMessage {
            content: UserContent::Text(format!(
                "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
            )),
            timestamp: now_ms(),
        })),
        _ => None,
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
    let mut out = String::new();
    let push_sep = |out: &mut String| {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
    };

    for msg in messages {
        match msg {
            Message::User(u) => {
                let text = u.content.text();
                if !text.is_empty() {
                    push_sep(&mut out);
                    out.push_str("[User]: ");
                    out.push_str(&text);
                }
            }
            Message::Assistant(a) => {
                let text = a.text_content();
                if !text.is_empty() {
                    push_sep(&mut out);
                    out.push_str("[Assistant]: ");
                    out.push_str(&text);
                }

                let mut tc_idx = 0usize;
                for b in &a.content {
                    if let AssistantContentBlock::ToolCall(tc) = b {
                        if tc_idx == 0 {
                            push_sep(&mut out);
                            out.push_str("[Assistant tool calls]: ");
                        } else {
                            out.push_str("; ");
                        }
                        tc_idx += 1;
                        out.push_str(&tc.name);
                        out.push('(');
                        for (arg_idx, (k, v)) in tc.arguments.iter().enumerate() {
                            if arg_idx > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(k);
                            out.push('=');
                            struct StringUtf8Writer<'a>(&'a mut String);
                            impl std::io::Write for StringUtf8Writer<'_> {
                                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                                    let s = std::str::from_utf8(buf).map_err(|e| {
                                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                                    })?;
                                    self.0.push_str(s);
                                    Ok(buf.len())
                                }
                                fn flush(&mut self) -> std::io::Result<()> {
                                    Ok(())
                                }
                            }
                            let _ = serde_json::to_writer(StringUtf8Writer(&mut out), v);
                        }
                        out.push(')');
                    }
                }
            }
            Message::ToolResult(tr) => {
                use std::fmt::Write;
                let mut written_chars = 0usize;
                let mut remaining = 0usize;
                let mut any_text = false;
                let mut header_written = false;

                for b in &tr.content {
                    let mehh_ai::ToolResultContentBlock::Text(t) = b else {
                        continue;
                    };
                    if any_text {
                        if written_chars < TOOL_RESULT_MAX_CHARS {
                            out.push('\n');
                            written_chars += 1;
                        } else {
                            remaining += 1;
                        }
                    } else if !t.text.is_empty() {
                        push_sep(&mut out);
                        out.push_str("[Tool result]: ");
                        header_written = true;
                    }
                    any_text = true;

                    if written_chars >= TOOL_RESULT_MAX_CHARS {
                        remaining += t.text.chars().count();
                        continue;
                    }
                    let budget = TOOL_RESULT_MAX_CHARS - written_chars;
                    let mut char_iter = t.text.char_indices();
                    if let Some((byte_end, _)) = char_iter.nth(budget) {
                        out.push_str(&t.text[..byte_end]);
                        written_chars = TOOL_RESULT_MAX_CHARS;
                        remaining += 1 + char_iter.count();
                    } else {
                        out.push_str(&t.text);
                        written_chars += t.text.chars().count();
                    }
                }
                if header_written && remaining > 0 {
                    let _ = write!(out, "\n\n[... {remaining} more characters truncated]");
                }
            }
            Message::System(_) => {}
        }
    }

    out
}

fn entry_stats_with_edits(
    entry: &SessionEntry,
    edits: &HashMap<&str, &Option<ContextEditReplacement>>,
) -> (usize, bool, bool, bool) {
    if let Some(edit) = edits.get(entry.id.as_str()) {
        if edit.is_none() {
            return (0, false, false, false);
        }
        if edit.is_some() {
            if let Some(msg) = messages_for_entry_with_edits(entry, edits) {
                return (
                    estimate_message_tokens(&msg),
                    is_cut_point_message(&msg),
                    matches!(msg, Message::User(_)),
                    matches!(msg, Message::Assistant(_)),
                );
            }
            return (0, false, false, false);
        }
    }
    match &entry.kind {
        SessionEntryKind::Message { message } => (
            estimate_message_tokens(message),
            is_cut_point_message(message),
            matches!(message, Message::User(_)),
            matches!(message, Message::Assistant(_)),
        ),
        SessionEntryKind::CustomMessage { content, .. } => {
            let chars = match content {
                UserContent::Text(t) => t.chars().count(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        mehh_ai::UserContentBlock::Text(t) => t.text.chars().count(),
                        mehh_ai::UserContentBlock::Image(_) => 4800,
                    })
                    .sum(),
            };
            (chars.div_ceil(4), true, true, false)
        }
        SessionEntryKind::BranchSummary { summary, .. } => {
            const PREFIX_CHARS: usize = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n".len();
            const SUFFIX_CHARS: usize = "\n</summary>".len();
            let tokens = (PREFIX_CHARS + summary.chars().count() + SUFFIX_CHARS).div_ceil(4);
            (tokens, true, true, false)
        }
        _ => (0, false, false, false),
    }
}

/// Evaluates active session entries and prepares a compaction cut point if history exceeds the budget.
pub fn prepare_compaction(
    entries: &[SessionEntry],
    keep_recent_tokens: usize,
) -> Option<CompactionPreparation> {
    prepare_compaction_with(entries, keep_recent_tokens)
}

/// Borrow-based variant of `prepare_compaction` avoiding cloning `Vec<SessionEntry>` from `active_path_entries()`.
pub fn prepare_compaction_refs(
    entries: &[&SessionEntry],
    keep_recent_tokens: usize,
) -> Option<CompactionPreparation> {
    prepare_compaction_with(entries, keep_recent_tokens)
}

fn prepare_compaction_with<E: std::borrow::Borrow<SessionEntry>>(
    entries: &[E],
    keep_recent_tokens: usize,
) -> Option<CompactionPreparation> {
    if entries.is_empty() {
        return None;
    }

    // Locate latest compaction boundary
    let last_compaction_pos = entries
        .iter()
        .rposition(|e| matches!(&e.borrow().kind, SessionEntryKind::Compaction { .. }));

    let (boundary_start, previous_summary) = match last_compaction_pos {
        Some(pos) => {
            if let SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } = &entries[pos].borrow().kind
            {
                // Entries kept by the previous compaction are still live context; the
                // next summary must cover them too. Match `SessionManager::build_context_messages()`
                // fallback to `0` when `first_kept_entry_id` cannot be found.
                let prev_kept_pos = entries[..pos]
                    .iter()
                    .position(|e| e.borrow().id == *first_kept_entry_id)
                    .unwrap_or(0);
                (prev_kept_pos, Some(summary.clone()))
            } else {
                (0, None)
            }
        }
        None => (0, None),
    };

    if boundary_start >= entries.len() {
        return None;
    }

    // Collect context edits across the active branch so hidden/replaced entries
    // are accurately reflected in token estimates, cut points, and summaries.
    let mut edits: HashMap<&str, &Option<ContextEditReplacement>> = HashMap::new();
    for entry in entries {
        let entry = entry.borrow();
        if let SessionEntryKind::ContextEdit {
            target_id,
            replacement,
        } = &entry.kind
        {
            edits.insert(target_id.as_str(), replacement);
        }
    }

    // Precompute per-entry token counts and message roles in one borrow pass without cloning Messages.
    let tail_len = entries.len() - boundary_start;
    let mut stats: Vec<(usize, bool, bool, bool)> = Vec::with_capacity(tail_len);
    let mut cut_point_candidates = Vec::new();
    let mut tokens_before: usize = previous_summary
        .as_ref()
        .map_or(0, |prev| prev.len().div_ceil(4));

    for (rel_idx, entry) in entries[boundary_start..].iter().enumerate() {
        let st = entry_stats_with_edits(entry.borrow(), &edits);
        tokens_before += st.0;
        if st.1 {
            cut_point_candidates.push(boundary_start + rel_idx);
        }
        stats.push(st);
    }

    if cut_point_candidates.is_empty() {
        return None;
    }

    // Walk backwards from the tail, accumulating tokens until >= keep_recent_tokens
    let mut accumulated_tokens = 0;
    let mut cut_index = cut_point_candidates[0];
    let mut budget_exceeded = false;

    for i in (boundary_start..entries.len()).rev() {
        accumulated_tokens += stats[i - boundary_start].0;
        if accumulated_tokens >= keep_recent_tokens {
            budget_exceeded = true;
            // Snap to the closest candidate at or after index i
            cut_index = cut_point_candidates
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(*cut_point_candidates.last()?);
            break;
        }
    }

    if !budget_exceeded {
        // Explicit `/compact` under token budget: compact earlier messages while
        // keeping the most recent candidate when at least 2 candidates exist.
        if cut_point_candidates.len() <= 1 {
            return None;
        }
        cut_index = *cut_point_candidates.last()?;
    }

    // Check if the cut splits a turn (cut point is Assistant message).
    // Prefer aligning kept history on a User message so post-compaction context
    // starts cleanly; for single-turn tool-loop overflows with no other User turn,
    // keep from the assistant cut point rather than failing compaction.
    let is_assistant_cut = stats[cut_index - boundary_start].3;

    let mut first_kept_index = if is_assistant_cut {
        let mut turn_start = None;
        for i in (boundary_start..=cut_index).rev() {
            if stats[i - boundary_start].2 {
                turn_start = Some(i);
                break;
            }
        }
        let can_rewind_to_turn_start = turn_start.is_some_and(|ts| {
            if ts <= boundary_start {
                return false;
            }
            let rewind_extra_tokens: usize = stats[(ts - boundary_start)..(cut_index - boundary_start)]
                .iter()
                .map(|st| st.0)
                .sum();
            rewind_extra_tokens <= keep_recent_tokens
        });
        if can_rewind_to_turn_start {
            turn_start.expect("turn_start checked above")
        } else {
            stats[(cut_index + 1 - boundary_start)..]
                .iter()
                .position(|st| st.2)
                .map(|rel| cut_index + 1 + rel)
                .unwrap_or(cut_index)
        }
    } else {
        cut_index
    };

    if first_kept_index <= boundary_start {
        first_kept_index = cut_point_candidates
            .iter()
            .copied()
            .find(|&c| c > boundary_start)?;
    }

    let mut messages_to_summarize = Vec::new();
    for entry in &entries[boundary_start..first_kept_index] {
        if let Some(m) = messages_for_entry_with_edits(entry.borrow(), &edits) {
            messages_to_summarize.push(m);
        }
    }

    let turn_prefix_messages = Vec::new();

    if messages_to_summarize.is_empty() {
        return None;
    }

    let first_kept_entry = entries[first_kept_index].borrow();

    // Extract file operations from read, write, and edit tool calls
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    for msg in messages_to_summarize
        .iter()
        .chain(turn_prefix_messages.iter())
    {
        if let Message::Assistant(ast) = msg {
            for b in &ast.content {
                if let AssistantContentBlock::ToolCall(call) = b {
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
    // large. Clamp the summarization payload (including previous summary and
    // custom instructions) so the summary request itself cannot overflow.
    let total_budget_chars = (model.context_window as usize)
        .saturating_sub(8192)
        .saturating_mul(3)
        .max(4096);

    let prev_summary_capped: Option<String> = prep.previous_summary.as_ref().map(|prev| {
        let max_prev_chars = total_budget_chars.saturating_mul(2) / 5;
        if prev.chars().count() > max_prev_chars {
            let mut clipped = prev
                .char_indices()
                .nth(max_prev_chars)
                .map(|(idx, _)| prev[..idx].to_string())
                .unwrap_or_else(|| prev.clone());
            clipped.push_str("\n[... previous summary truncated]");
            clipped
        } else {
            prev.clone()
        }
    });

    let instructions_text = custom_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let prompt_template = if prev_summary_capped.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };
    let overhead_chars = SUMMARIZATION_SYSTEM_PROMPT.chars().count()
        + prompt_template.chars().count()
        + prev_summary_capped
            .as_ref()
            .map_or(0, |s| s.chars().count() + 48)
        + instructions_text.map_or(0, |s| s.chars().count() + 32);
    let conv_budget_chars = total_budget_chars.saturating_sub(overhead_chars).max(1024);

    if serialized_conv.chars().count() > conv_budget_chars {
        // Keep the newest tail: a chronological transcript truncated at the head
        // would drop the most recent context while claiming to drop the oldest.
        if let Some((byte_start, _)) = serialized_conv.char_indices().rev().nth(conv_budget_chars - 1) {
            let newest = serialized_conv[byte_start..].to_string();
            serialized_conv =
                format!("[... older history truncated for summarization]\n\n{newest}");
        }
    }

    let mut user_prompt = String::new();
    if let Some(prev) = &prev_summary_capped {
        user_prompt.push_str(&format!(
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        ));
    }

    user_prompt.push_str(&serialized_conv);
    user_prompt.push_str("\n\n");
    user_prompt.push_str(prompt_template);

    if let Some(ci) = instructions_text {
        user_prompt.push_str(&format!("\n\nCustom Instructions:\n{ci}"));
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
    if summary.is_empty() {
        return Err(anyhow!("Compaction model returned an empty summary"));
    }

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

    #[tokio::test]
    async fn compact_with_provider_keeps_newest_history_when_clamped() {
        let mut model = make_test_model();
        model.context_window = 4096;
        let prep = CompactionPreparation {
            first_kept_entry_id: "e2".to_string(),
            messages_to_summarize: vec![
                Message::User(UserMessage {
                    content: UserContent::Text(format!("OLDEST_MARKER{}", "x".repeat(6_000))),
                    timestamp: now_ms(),
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("NEWEST_MARKER".to_string()),
                    timestamp: now_ms(),
                }),
            ],
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
        let text = match &requests.last().expect("compaction request recorded").context.messages[0]
        {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text prompt"),
            },
            _ => panic!("expected user prompt"),
        };
        assert!(text.contains("NEWEST_MARKER"), "newest history must be kept");
        assert!(
            !text.contains("OLDEST_MARKER"),
            "oldest history must be dropped when clamping"
        );
        assert!(text.contains("older history truncated for summarization"));
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
        big.push_str("\u{4F60}\u{597D}\u{4E16}\u{754C}\u{1F642}extra");
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
    fn second_compaction_preserves_messages_kept_by_first_compaction() {
        let long = "word ".repeat(20_000);
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

        let u2_marker = "KEEP_U2_MARKER";
        let a2_text = format!("KEEP_A2_MARKER {long}");
        let mut entries = vec![
            mk_user("u1", None, "first prompt"),
            mk_ast("a1", "u1", "first reply"),
            mk_user("u2", Some("a1"), u2_marker),
            mk_ast("a2", "u2", &a2_text),
        ];

        let first = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS).unwrap();
        assert_eq!(first.first_kept_entry_id, "u2");

        entries.push(SessionEntry {
            id: "c1".to_string(),
            parent_id: Some("a2".to_string()),
            timestamp: "2026-11-12T10:05:00.000Z".to_string(),
            kind: SessionEntryKind::Compaction {
                summary: "summary of first compaction".to_string(),
                first_kept_entry_id: first.first_kept_entry_id.clone(),
                tokens_before: first.tokens_before as u64,
            },
        });
        entries.push(mk_user("u3", Some("c1"), "third prompt"));
        entries.push(mk_ast("a3", "u3", &long));
        entries.push(mk_user("u4", Some("a3"), "fourth prompt"));
        entries.push(mk_ast("a4", "u4", "fourth reply"));

        let second = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS).unwrap();
        let summarized = serialize_conversation(&second.messages_to_summarize);
        assert!(summarized.contains(u2_marker), "u2 kept by the first compaction was dropped");
        assert!(summarized.contains("KEEP_A2_MARKER"), "a2 kept by the first compaction was dropped");
        assert_eq!(second.first_kept_entry_id, "u3");
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

    #[test]
    fn prepare_compaction_handles_single_turn_tool_loop_overflow() {
        let mk_user = |id: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: None,
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
        let mk_tr = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:02:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: format!("call_{id}"),
                    tool_name: "read".to_string(),
                    content: vec![mehh_ai::ToolResultContentBlock::Text(TextContent {
                        text: text.to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    duration_ms: None,
                    usage: None,
                    timestamp: now_ms(),
                }),
            },
        };

        let big = "x".repeat(4000);
        let entries = vec![
            mk_user("u1", "solve single-turn loop"),
            mk_ast("a1", "u1", &big),
            mk_tr("t1", "a1", &big),
            mk_ast("a2", "t1", &big),
            mk_tr("t2", "a2", &big),
            mk_ast("a3", "t2", "recent step"),
            mk_tr("t3", "a3", "recent output"),
        ];

        let prep = prepare_compaction(&entries, 200)
            .expect("single-turn tool-loop overflow must still prepare compaction");
        assert!(prep.first_kept_entry_id == "a2" || prep.first_kept_entry_id == "a3");
        assert!(!prep.messages_to_summarize.is_empty());
        let summarized = serialize_conversation(&prep.messages_to_summarize);
        assert!(summarized.contains("solve single-turn loop"));
    }

    #[test]
    fn prepare_compaction_allows_explicit_compaction_under_token_budget() {
        let entries = vec![
            SessionEntry {
                id: "u1".to_string(),
                parent_id: None,
                timestamp: "2026-11-12T10:00:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("hello".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "a1".to_string(),
                parent_id: Some("u1".to_string()),
                timestamp: "2026-11-12T10:01:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::Assistant(mehh_ai::faux_assistant_message("hi there")),
                },
            },
        ];

        let prep = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS)
            .expect("explicit compaction under budget must prepare when > 1 candidate exists");
        assert_eq!(prep.first_kept_entry_id, "a1");
        assert_eq!(prep.messages_to_summarize.len(), 1);
    }

    #[test]
    fn prepare_compaction_respects_context_edits() {
        let big = "y".repeat(4000);
        let entries = vec![
            SessionEntry {
                id: "u1".to_string(),
                parent_id: None,
                timestamp: "2026-11-12T10:00:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("first".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "a1".to_string(),
                parent_id: Some("u1".to_string()),
                timestamp: "2026-11-12T10:01:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::Assistant(mehh_ai::faux_assistant_message(&big)),
                },
            },
            SessionEntry {
                id: "u2".to_string(),
                parent_id: Some("a1".to_string()),
                timestamp: "2026-11-12T10:02:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::User(UserMessage {
                        content: UserContent::Text("second".to_string()),
                        timestamp: now_ms(),
                    }),
                },
            },
            SessionEntry {
                id: "a2".to_string(),
                parent_id: Some("u2".to_string()),
                timestamp: "2026-11-12T10:03:00.000Z".to_string(),
                kind: SessionEntryKind::Message {
                    message: Message::Assistant(mehh_ai::faux_assistant_message("done")),
                },
            },
            // Hide a1 and replace u1
            SessionEntry {
                id: "ce1".to_string(),
                parent_id: Some("a2".to_string()),
                timestamp: "2026-11-12T10:04:00.000Z".to_string(),
                kind: SessionEntryKind::ContextEdit {
                    target_id: "a1".to_string(),
                    replacement: None,
                },
            },
            SessionEntry {
                id: "ce2".to_string(),
                parent_id: Some("ce1".to_string()),
                timestamp: "2026-11-12T10:05:00.000Z".to_string(),
                kind: SessionEntryKind::ContextEdit {
                    target_id: "u1".to_string(),
                    replacement: Some(crate::session::ContextEditReplacement {
                        content: crate::session::ContextEditableContent::Text("edited first".to_string()),
                    }),
                },
            },
        ];

        let prep = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS).unwrap();
        assert_eq!(prep.first_kept_entry_id, "u2");
        let summarized = serialize_conversation(&prep.messages_to_summarize);
        assert!(summarized.contains("edited first"));
        assert!(!summarized.contains(&big));
        assert!(prep.tokens_before < 200);
    }

    #[tokio::test]
    async fn compact_with_provider_rejects_empty_summary() {
        let prep = CompactionPreparation {
            first_kept_entry_id: "e2".to_string(),
            messages_to_summarize: vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: now_ms(),
            })],
            turn_prefix_messages: Vec::new(),
            tokens_before: 10,
            previous_summary: None,
            read_files: vec!["foo.rs".to_string()],
            modified_files: Vec::new(),
        };
        let faux = FauxProvider::new();
        faux.add_response(mehh_ai::faux_assistant_message("   \n  "));

        let err = compact_with_provider(
            &prep,
            &make_test_model(),
            &faux,
            None,
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("empty LLM summary must be rejected");
        assert!(err.to_string().contains("empty summary"));
    }

    #[test]
    fn test_compaction_mid_turn_tool_loop_overflow_does_not_rewind_to_turn_start() {
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
        let mk_tr = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:02:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: format!("call_{id}"),
                    tool_name: "read".to_string(),
                    content: vec![mehh_ai::ToolResultContentBlock::Text(TextContent {
                        text: text.to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    duration_ms: None,
                    usage: None,
                    timestamp: now_ms(),
                }),
            },
        };

        let big = "x".repeat(4_000); // 1,000 tokens
        let entries = vec![
            mk_user("u1", None, "first turn"),
            mk_ast("a1", "u1", "first reply"),
            mk_user("u2", Some("a1"), "second turn tool loop"),
            mk_ast("a2_1", "u2", &format!("EARLY_AST_1 {big}")),
            mk_tr("t2_1", "a2_1", &format!("EARLY_TR_1 {big}")),
            mk_ast("a2_2", "t2_1", &big),
            mk_tr("t2_2", "a2_2", &big),
            mk_ast("a2_3", "t2_2", &big),
            mk_tr("t2_3", "a2_3", &big),
            mk_ast("a2_4", "t2_3", "recent step"),
            mk_tr("t2_4", "a2_4", "recent output"),
        ];

        let keep_recent = 2_500;
        let prep = prepare_compaction(&entries, keep_recent)
            .expect("multi-step mid-turn overflow must prepare compaction");
        assert_ne!(
            prep.first_kept_entry_id, "u2",
            "must not rewind to turn_start u2 when rewind_extra_tokens exceeds keep_recent_tokens"
        );
        let summarized = serialize_conversation(&prep.messages_to_summarize);
        assert!(summarized.contains("second turn tool loop"));
        assert!(summarized.contains("EARLY_AST_1"));
        assert!(summarized.contains("EARLY_TR_1"));

        let first_kept_pos = entries
            .iter()
            .position(|e| e.id == prep.first_kept_entry_id)
            .unwrap();
        let kept_tokens: usize = entries[first_kept_pos..]
            .iter()
            .filter_map(|e| match &e.kind {
                SessionEntryKind::Message { message } => Some(estimate_message_tokens(message)),
                _ => None,
            })
            .sum();
        assert!(
            kept_tokens <= keep_recent + 1_000,
            "kept tokens ({kept_tokens}) should stay bounded near keep_recent ({keep_recent})"
        );
    }
}
