use super::*;
use super::tasks::{finish_mcp_auth_signal, start_continuation_turn, start_overflow_compaction, start_turn};

pub(super) fn format_assistant_stop_notice(msg: &mehh_ai::AssistantMessage) -> String {
    let err_text = msg
        .error_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if msg.stop_reason == StopReason::Aborted {
                "Aborted"
            } else {
                "Provider stream error"
            }
        });
    if msg.stop_reason == StopReason::Aborted {
        format!("Turn aborted: {err_text}")
    } else {
        format!("Provider error: {err_text}")
    }
}

pub(super) fn push_turn_error_once(state: &mut InteractiveState, notice: String) {
    if state.last_turn_error.as_deref() == Some(notice.as_str()) {
        return;
    }
    state.last_turn_error = Some(notice.clone());
    state.items.push(TranscriptItem::System(notice));
}

/// Applies agent lifecycle and stream events to update interactive state and transcript.
pub fn apply_agent_event(state: &mut InteractiveState, event: &AgentEvent) {
    match event {
        AgentEvent::AgentStart => {
            state.is_busy = true;
        }
        AgentEvent::AgentEnd { .. } => {
            state.is_busy = false;
            state.status = None;
        }
        AgentEvent::TurnStart => {
            state.is_busy = true;
        }
        AgentEvent::TurnEnd { .. } => {}
        AgentEvent::MessageStart { message } => {
            state.is_busy = true;
            if let Message::Assistant(_) = message {
                match state.items.last() {
                    Some(TranscriptItem::Assistant { .. }) => {}
                    _ => {
                        state.items.push(TranscriptItem::Assistant {
                            thinking: String::new(),
                            text: String::new(),
                        });
                    }
                }
            }
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                if let Some(TranscriptItem::Assistant { text, .. }) = state.items.last_mut() {
                    text.push_str(delta);
                } else {
                    state.items.push(TranscriptItem::Assistant {
                        thinking: String::new(),
                        text: delta.clone(),
                    });
                }
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                if let Some(TranscriptItem::Assistant { thinking, .. }) = state.items.last_mut() {
                    thinking.push_str(delta);
                } else {
                    state.items.push(TranscriptItem::Assistant {
                        thinking: delta.clone(),
                        text: String::new(),
                    });
                }
            }
            AssistantMessageEvent::Error { error, .. } => {
                let notice = format_assistant_stop_notice(error);
                push_turn_error_once(state, notice);
            }
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            state.invalidate_context_tokens();
            if let Message::Assistant(a) = message {
                state.total_input_tokens += a.usage.input;
                state.total_output_tokens += a.usage.output;
                state.total_cost += a.usage.cost.total;
                if let Some(TranscriptItem::Assistant { text, thinking }) = state.items.last_mut() {
                    if text.is_empty() && thinking.is_empty() {
                        *text = a.text_content();
                    }
                }
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    let notice = format_assistant_stop_notice(a);
                    push_turn_error_once(state, notice);
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            state.status = Some(format!("Executing {tool_name}..."));
            if tool_name == "subagent" {
                let mode = args
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("research")
                    .to_string();
                state.items.push(TranscriptItem::Subagent {
                    id: tool_call_id.clone(),
                    mode,
                    status: "running".to_string(),
                    is_error: false,
                    children: Vec::new(),
                });
                return;
            }
            let args_summary = if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                p.to_string()
            } else if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                cmd.chars().take(40).collect::<String>()
            } else if let Some(q) = args
                .get("query")
                .or_else(|| args.get("pattern"))
                .or_else(|| args.get("url"))
                .and_then(|v| v.as_str())
            {
                q.to_string()
            } else {
                String::new()
            };
            state.items.push(TranscriptItem::ToolCall {
                id: tool_call_id.clone(),
                name: tool_name.clone(),
                status: "running".to_string(),
                is_error: false,
                args_summary,
                output_preview: Vec::new(),
                diff: None,
                duration_ms: None,
            });
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            // Structured subagent updates: upsert a child row keyed by child_id,
            // or move the group's status on terminal events.
            if let Some(sub) = partial_result
                .details
                .as_ref()
                .and_then(|d| d.get("subagent"))
            {
                if let (Some(child_id), Some(event_kind)) = (
                    sub.get("child_id").and_then(|v| v.as_str()),
                    sub.get("event_kind").and_then(|v| v.as_str()),
                ) {
                    let detail = sub
                        .get("detail")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    for item in state.items.iter_mut().rev() {
                        if let TranscriptItem::Subagent { id, children, .. } = item {
                            if id != tool_call_id {
                                continue;
                            }
                            match event_kind {
                                "started" => {
                                    if !children.iter().any(|r| r.child_id == child_id) {
                                        children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "started".to_string(),
                                            status: "running".to_string(),
                                            detail: detail.clone(),
                                        });
                                    }
                                }
                                "blocked" => {
                                    let blocker = detail.clone();
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            row.label = "blocked".to_string();
                                            row.status = "blocked".to_string();
                                            row.detail = blocker;
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "blocked".to_string(),
                                            status: "blocked".to_string(),
                                            detail: blocker,
                                        }),
                                    }
                                }
                                "tool_started" => {
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            // Replace a placeholder (started) row in place
                                            // rather than appending a duplicate.
                                            row.label = "tool".to_string();
                                            row.status = "running".to_string();
                                            row.detail = detail.clone();
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "tool".to_string(),
                                            status: "running".to_string(),
                                            detail: detail.clone(),
                                        }),
                                    }
                                }
                                "tool_finished" => {
                                    // Never insert on tool_finished; update the most recent
                                    // running row for this child_id, if any.
                                    let status_value = if detail.is_empty() {
                                        "completed".to_string()
                                    } else {
                                        detail
                                    };
                                    // Find the last running row for this child.
                                    let mut target_idx: Option<usize> = None;
                                    for (i, r) in children.iter().enumerate() {
                                        if r.child_id == child_id && r.status == "running" {
                                            target_idx = Some(i);
                                        }
                                    }
                                    if let Some(i) = target_idx {
                                        children[i].status = status_value;
                                        children[i].detail = children[i].status.clone();
                                    }
                                }
                                "finished" | "aborted" | "timed_out" => {
                                    // A child reaching a terminal state changes only its own
                                    // row. The card's aggregate status is finalized once, on
                                    // ToolExecutionEnd, so an early failure is not cleared by
                                    // a sibling finishing later.
                                    let (row_status, row_detail) = match event_kind {
                                        "finished" => ("completed", "completed"),
                                        "aborted" => ("failed", "aborted"),
                                        _ => ("failed", "timed_out"),
                                    };
                                    match children.iter_mut().find(|r| r.child_id == child_id) {
                                        Some(row) => {
                                            row.status = row_status.to_string();
                                            row.detail = row_detail.to_string();
                                        }
                                        None => children.push(SubagentChildRow {
                                            child_id: child_id.to_string(),
                                            label: "child".to_string(),
                                            status: row_status.to_string(),
                                            detail: row_detail.to_string(),
                                        }),
                                    }
                                }
                                _ => {}
                            }
                            // Same cap as the generic tool card's output preview.
                            while children.len() > 8 {
                                children.remove(0);
                            }
                            break;
                        }
                    }
                }
                // Subagent updates never target a generic tool card; stop here
                // instead of falling through into the ToolCall scan below.
                return;
            }
            for item in state.items.iter_mut().rev() {
                if let TranscriptItem::ToolCall {
                    id,
                    status,
                    output_preview,
                    ..
                } = item
                {
                    if id == tool_call_id {
                        *status = "running".to_string();
                        for block in &partial_result.content {
                            if let mehh_ai::ToolResultContentBlock::Text(ref tc) = block {
                                for line in tc.text.lines().filter(|l| !l.trim().is_empty()) {
                                    output_preview.push(line.to_string());
                                    if output_preview.len() > 8 {
                                        output_preview.remove(0);
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
            duration_ms,
            ..
        } => {
            state.status = None;
            if tool_name == "subagent" {
                for item in state.items.iter_mut().rev() {
                    if let TranscriptItem::Subagent {
                        id,
                        status,
                        is_error: err,
                        ..
                    } = item
                    {
                        if id == tool_call_id {
                            *status = if *is_error {
                                "failed".to_string()
                            } else {
                                "completed".to_string()
                            };
                            *err = *is_error;
                            break;
                        }
                    }
                }
                return;
            }
            for item in state.items.iter_mut().rev() {
                if let TranscriptItem::ToolCall {
                    id,
                    status,
                    is_error: err,
                    diff,
                    output_preview,
                    duration_ms: stored_duration,
                    ..
                } = item
                {
                    if id == tool_call_id {
                        *stored_duration = *duration_ms;
                        *status = if *is_error {
                            "failed".to_string()
                        } else {
                            "completed".to_string()
                        };
                        *err = *is_error;
                        if let Some(d) = result
                            .details
                            .as_ref()
                            .and_then(|m| m.get("diff"))
                            .and_then(|v| v.as_str())
                        {
                            *diff = Some(d.to_string());
                        }
                        if *is_error && output_preview.is_empty() {
                            for block in &result.content {
                                if let mehh_ai::ToolResultContentBlock::Text(ref tc) = block {
                                    for line in
                                        tc.text.lines().filter(|l| !l.trim().is_empty()).take(8)
                                    {
                                        output_preview.push(line.to_string());
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
}


pub(crate) struct ReadyForTurnPayload {
    pub(crate) prompt_text: String,
    pub(crate) blocks: Vec<UserContentBlock>,
    pub(crate) provider_name: String,
    pub(crate) new_api_key: Option<String>,
    pub(crate) compaction: Option<(String, String, u64)>,
    pub(crate) warning: Option<String>,
    pub(crate) cancel: CancellationToken,
}

pub(crate) struct OverflowCompactedPayload {
    pub(crate) original_messages: Vec<Message>,
    pub(crate) prep: Option<(String, u64)>,
    pub(crate) result: Result<String, String>,
    pub(crate) cancelled: bool,
    pub(crate) prefix_persisted: bool,
}

pub(crate) struct ManualCompactedPayload {
    pub(crate) prep: (String, u64),
    pub(crate) result: Result<String, String>,
    pub(crate) cancelled: bool,
}

pub(crate) struct ModelsDiscoveredPayload {
    pub(crate) batch_id: u64,
    pub(crate) provider: String,
    pub(crate) result: Result<(Vec<mehh_ai::Model>, Option<String>), String>,
}

/// Signal delivered from a running agent turn or background prep/compaction task to the UI loop.
pub(crate) enum AgentSignal {
    ReadyForTurn(Box<ReadyForTurnPayload>),
    PrepFailed(String),
    PrepAborted,
    OverflowCompacted(Box<OverflowCompactedPayload>),
    ManualCompacted(Box<ManualCompactedPayload>),
    Event(Box<AgentEvent>),
    ToolApprovalRequest(Box<PendingApproval>),
    Finished(Vec<Message>),
    Failed(String),
    SystemMessage(String),
    LoginCompleted {
        provider: String,
        result: Result<String, String>,
    },
    ModelsDiscovered(Box<ModelsDiscoveredPayload>),
    McpAuthCompleted {
        server: String,
        result: Result<(), String>,
    },
}

/// Applies a signal from the agent task to the session and the interactive state.
///
/// Returns true when the signal ends the turn, so the caller can stop routing
/// user input into the finished turn.
pub(in crate::interactive) fn apply_agent_signal(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    match signal {
        AgentSignal::ToolApprovalRequest(pending) => {
            // Discard any pending dismissable modal (and revert unconfirmed /theme preview)
            // before showing mandatory approval controls.
            state.close_modal();
            // The reason and full preview live in their own panel; never hide them
            // inside a list row where truncate_to_width would cut the command.
            let allow_color = state.theme.success.clone();
            let deny_color = state.theme.error.clone();
            let items = vec![
                SelectItem::new("allow_once", "Allow once (y/Enter)")
                    .with_description("Approve this tool call once")
                    .with_label_color(allow_color.clone()),
                SelectItem::new("allow_session", "Allow for session (a)")
                    .with_description("Auto-approve this tool until session ends")
                    .with_label_color(allow_color.clone()),
                SelectItem::new("always_allow", "Always allow (A)")
                    .with_description("Persist always-allow rule to ~/.mehh/agent/permissions.json")
                    .with_label_color(allow_color),
                SelectItem::new("deny", "Deny (n/Esc)")
                    .with_description("Block this tool call")
                    .with_label_color(deny_color),
            ];
            state.pending_approval = Some(*pending);
            state.approval_first_rendered_at = None;
            state.selector = Some(Selector {
                kind: SelectorKind::ToolApproval,
                list: SelectList::new(items, 6),
                settings_list: None,
            });
            false
        }
        AgentSignal::Event(ev) => {
            apply_agent_event(state, &ev);
            if matches!(&*ev, AgentEvent::AgentEnd { .. }) {
                // Stay busy until the finished turn has been persisted.
                state.is_busy = true;
                state.status = None;
            }
            false
        }
        AgentSignal::Finished(messages) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            state.approval_first_rendered_at = None;
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            if let Some(Message::Assistant(a)) = messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    let notice = format_assistant_stop_notice(a);
                    push_turn_error_once(state, notice);
                }
            }
            if let Err(e) = session.finish_prompt(&messages) {
                state.items.push(TranscriptItem::System(format!(
                    "Failed to persist turn: {e}"
                )));
            }
            // finish_prompt appends the turn's messages to the session. A render
            // between MessageEnd and this point may have cached the old transcript,
            // so invalidate after the mutation, not before it.
            state.invalidate_context_tokens();
            true
        }
        AgentSignal::Failed(message) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            state.approval_first_rendered_at = None;
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            state.items.push(TranscriptItem::System(format!(
                "Agent turn failed: {message}"
            )));
            true
        }
        AgentSignal::PrepFailed(message) => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            state.approval_first_rendered_at = None;
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            state.items.push(TranscriptItem::System(message));
            true
        }
        AgentSignal::PrepAborted => {
            if let Some(pending) = state.pending_approval.take() {
                let _ = pending.reply_tx.send(false);
            }
            state.approval_first_rendered_at = None;
            if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                state.selector = None;
            }
            state.is_busy = false;
            state.status = None;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            state
                .items
                .push(TranscriptItem::System("Turn aborted.".to_string()));
            true
        }
        AgentSignal::SystemMessage(msg) => {
            state.items.push(TranscriptItem::System(msg));
            false
        }
        AgentSignal::ReadyForTurn { .. }
        | AgentSignal::OverflowCompacted { .. }
        | AgentSignal::ManualCompacted { .. }
        | AgentSignal::LoginCompleted { .. }
        | AgentSignal::ModelsDiscovered { .. }
        | AgentSignal::McpAuthCompleted { .. } => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::interactive) enum SignalOutcome {
    Continuing,
    RetryStarted,
    TurnEnded,
    BackgroundEnded,
}

pub(in crate::interactive) async fn handle_agent_signal_async(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> SignalOutcome {
    match signal {
        AgentSignal::ReadyForTurn(payload) => {
            let ReadyForTurnPayload {
                prompt_text,
                blocks,
                provider_name,
                new_api_key,
                compaction,
                warning,
                cancel,
            } = *payload;
            if cancel.is_cancelled() {
                *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
                state.is_busy = false;
                state.status = None;
                state
                    .items
                    .push(TranscriptItem::System("Turn aborted.".to_string()));
                return SignalOutcome::TurnEnded;
            }
            if let Some(key) = new_api_key {
                if session.model.provider.eq_ignore_ascii_case(&provider_name)
                    && !session.has_explicit_api_key()
                    && session.api_key.as_deref() != Some(key.as_str())
                {
                    session.set_api_key(Some(key));
                }
            }
            if let Some((summary, first_kept_id, tokens_before)) = compaction {
                if let Err(e) =
                    session.apply_prepared_compaction(summary, first_kept_id, tokens_before)
                {
                    state.items.push(TranscriptItem::System(format!(
                        "Warning: failed to persist proactive auto-compaction: {e}"
                    )));
                } else {
                    state.invalidate_context_tokens();
                }
            }
            if let Some(w) = warning {
                state.items.push(TranscriptItem::System(format!(
                    "Warning: proactive auto-compaction failed: {w}"
                )));
            }
            start_turn(session, state, &prompt_text, blocks, agent_tx);
            SignalOutcome::RetryStarted
        }
        AgentSignal::Finished(messages) => {
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            if !state.overflow_retried && session.is_overflow_turn(&messages) {
                state.overflow_retried = true;
                if crate::can_retry_overflow_from_scratch(&messages) {
                    if let Some(prep) = session.prepare_overflow_compaction() {
                        state.is_busy = true;
                        state.status = Some("Context overflow — compacting history...".to_string());
                        start_overflow_compaction(session, prep, messages, false, agent_tx);
                        return SignalOutcome::RetryStarted;
                    }
                } else if messages.len() >= 2
                    && matches!(messages.last(), Some(Message::Assistant(_)))
                {
                    // Case B: Mid-turn overflow after tools, multiple turns, or drained queued user messages.
                    // Persist completed prefix BEFORE preparing compaction so prior turns
                    // and current-turn tool results/steering are visible to compaction.
                    let prefix = &messages[..messages.len() - 1];
                    match session.finish_prompt(prefix) {
                        Ok(()) => {
                            state.invalidate_context_tokens();
                            if let Some(prep) = session.prepare_overflow_compaction() {
                                state.is_busy = true;
                                state.status = Some(
                                    "Context overflow — compacting history...".to_string(),
                                );
                                start_overflow_compaction(
                                    session,
                                    prep,
                                    messages,
                                    true,
                                    agent_tx,
                                );
                                return SignalOutcome::RetryStarted;
                            }
                            let trailing = messages[messages.len() - 1..].to_vec();
                            return if apply_agent_signal(
                                AgentSignal::Finished(trailing),
                                session,
                                state,
                            ) {
                                SignalOutcome::TurnEnded
                            } else {
                                SignalOutcome::Continuing
                            };
                        }
                        Err(e) => {
                            state.items.push(TranscriptItem::System(format!(
                                "Failed to persist completed tool prefix: {e}"
                            )));
                        }
                    }
                }
            }
            if apply_agent_signal(AgentSignal::Finished(messages), session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
        AgentSignal::OverflowCompacted(payload) => {
            let OverflowCompactedPayload {
                original_messages,
                prep,
                result,
                cancelled,
                prefix_persisted,
            } = *payload;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            if !cancelled {
                if prefix_persisted {
                    // Case B: Mid-turn overflow after side-effecting tools already ran.
                    // Completed prefix is already persisted; compact and continue without
                    // re-sending the initial UserMessage or re-executing tools.
                    if let (Ok(summary), Some((first_kept_id, tokens_before))) =
                        (result.as_ref(), prep)
                    {
                        match session.apply_prepared_compaction(
                            summary.clone(),
                            first_kept_id,
                            tokens_before,
                        ) {
                            Ok(()) => {
                                state.invalidate_context_tokens();
                                state.last_turn_error = None;
                                state.items.push(TranscriptItem::System(
                                    "Context overflow detected; auto-compacted history and continuing turn..."
                                        .to_string(),
                                ));
                                start_continuation_turn(session, state, agent_tx);
                                return SignalOutcome::RetryStarted;
                            }
                            Err(e) => {
                                state.items.push(TranscriptItem::System(format!(
                                    "Failed to persist overflow auto-compaction: {e}"
                                )));
                            }
                        }
                    } else if let Err(err) = result {
                        state.items.push(TranscriptItem::System(format!(
                            "Overflow auto-compaction failed: {err}"
                        )));
                    }
                } else {
                    // Case A: Turn-0 overflow (no tools ran yet) -> safe to compact & retry prompt.
                    let retry_content = state.last_prompt_content.clone().or_else(|| {
                        state
                            .last_prompt_text
                            .clone()
                            .map(|text| (text, Vec::new()))
                    });
                    if let (
                        Ok(summary),
                        Some((first_kept_id, tokens_before)),
                        Some((retry_text, retry_blocks)),
                    ) = (result.as_ref(), prep, retry_content)
                    {
                        match session.apply_prepared_compaction(
                            summary.clone(),
                            first_kept_id,
                            tokens_before,
                        ) {
                            Ok(()) => {
                                state.invalidate_context_tokens();
                                if let Some(start_idx) = state.turn_start_idx {
                                    state.items.truncate(start_idx);
                                    mehh_ai::lock_recover(&state.transcript_cache).truncate(start_idx);
                                }
                                state.last_turn_error = None;
                                state.items.push(TranscriptItem::System(
                                    "Context overflow detected; auto-compacted history and retrying turn..."
                                        .to_string(),
                                ));
                                start_turn(session, state, &retry_text, retry_blocks, agent_tx);
                                return SignalOutcome::RetryStarted;
                            }
                            Err(e) => {
                                state.items.push(TranscriptItem::System(format!(
                                    "Failed to persist overflow auto-compaction: {e}"
                                )));
                            }
                        }
                    } else if let Err(err) = result {
                        state.items.push(TranscriptItem::System(format!(
                            "Overflow auto-compaction failed: {err}"
                        )));
                    }
                }
            } else {
                state.items.push(TranscriptItem::System(
                    "Overflow auto-compaction aborted.".to_string(),
                ));
            }
            let finalize_messages = if prefix_persisted {
                original_messages.last().cloned().into_iter().collect()
            } else {
                original_messages
            };
            if apply_agent_signal(AgentSignal::Finished(finalize_messages), session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
        AgentSignal::ManualCompacted(payload) => {
            let ManualCompactedPayload {
                prep: (first_kept_id, tokens_before),
                result,
                cancelled,
            } = *payload;
            *mehh_ai::lock_recover(&session.active_cancel_slot()) = None;
            state.is_busy = false;
            state.status = None;
            state.invalidate_context_tokens();
            if cancelled {
                state
                    .items
                    .push(TranscriptItem::System("Compaction aborted.".to_string()));
            } else {
                match result {
                    Ok(summary) => {
                        match session.apply_prepared_compaction(
                            summary.clone(),
                            first_kept_id,
                            tokens_before,
                        ) {
                            Ok(()) => {
                                state.invalidate_context_tokens();
                                state.items.push(TranscriptItem::System(format!(
                                    "Compaction complete: {summary}"
                                )));
                            }
                            Err(e) => {
                                state.items.push(TranscriptItem::System(format!(
                                    "Failed to persist compaction: {e}"
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        state.items.push(TranscriptItem::System(format!(
                            "Compaction error: {e}"
                        )));
                    }
                }
            }
            SignalOutcome::BackgroundEnded
        }
        AgentSignal::LoginCompleted { provider, result } => {
            match result {
                Ok(msg) => {
                    match refresh_session_api_key(session, &provider, state.auth_file.as_deref())
                        .await
                    {
                        Ok(()) => state.items.push(TranscriptItem::System(msg)),
                        Err(e) => state.items.push(TranscriptItem::System(format!(
                            "{msg} (could not refresh the session key: {e})"
                        ))),
                    }
                    refresh_connected_state(session, state);
                    trigger_model_refresh(&provider, session, state, true, agent_tx).await;
                }
                Err(e) => state
                    .items
                    .push(TranscriptItem::System(format!("Login failed: {e}"))),
            }
            SignalOutcome::Continuing
        }
        AgentSignal::ModelsDiscovered(payload) => {
            let ModelsDiscoveredPayload {
                batch_id,
                provider,
                result,
            } = *payload;
            if batch_id != state.discovery_batch_id {
                return SignalOutcome::Continuing;
            }
            state.discovery_pending = state.discovery_pending.saturating_sub(1);
            if state.discovery_pending == 0 {
                state.discovery_status = None;
                state.discovery_handles.clear();
            }

            let (reg, _) = load_interactive_registry(state);
            let (connected, _) = connected_providers_with_session(session, state, &reg);
            state.connected_provider_count = connected
                .iter()
                .filter(|p| !p.provider_id.eq_ignore_ascii_case("faux"))
                .count();
            let active_connected = connected
                .iter()
                .any(|c| c.provider_id.eq_ignore_ascii_case(&session.model.provider));
            let active_has_endpoint = reg.provider_has_explicit_endpoint(&session.model.provider);
            let is_faux = session.model.provider.eq_ignore_ascii_case("faux");
            let needs_auto_switch =
                !is_faux && (!active_connected || (session.api_key.is_none() && !active_has_endpoint));

            match result {
                Ok((models, cache_warn)) => {
                    if let Some(warn) = cache_warn {
                        state.items.push(TranscriptItem::System(warn));
                    }
                    state.items.push(TranscriptItem::System(format!(
                        "Loaded {} model{} for {}.",
                        models.len(),
                        if models.len() == 1 { "" } else { "s" },
                        provider
                    )));
                    if needs_auto_switch {
                        if let Some(first) = models.first() {
                            let target = format!("{}/{}", provider, first.id);
                            apply_interactive_model_switch(&target, session, state).await;
                        }
                    }
                }
                Err(e) => {
                    state.items.push(TranscriptItem::System(format!(
                        "Could not fetch live models for {provider}: {e} (using built-in catalog)"
                    )));
                    if needs_auto_switch {
                        if let Some(fallback) = reg
                            .models_for_connected_providers(&connected)
                            .into_iter()
                            .find(|m| m.provider.eq_ignore_ascii_case(&provider))
                        {
                            let target = format!("{}/{}", fallback.provider, fallback.id);
                            apply_interactive_model_switch(&target, session, state).await;
                        }
                    }
                }
            }

            if state.discovery_pending == 0
                && std::mem::take(&mut state.discovery_open_picker)
                && state.selector.is_none()
                && !state.has_tool_approval()
            {
                open_model_selector(session, state);
            }
            SignalOutcome::Continuing
        }
        other => {
            if apply_agent_signal(other, session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
    }
}

/// Applies every signal that is already queued. Returns true when a turn ended without retry.
#[cfg(test)]
pub(in crate::interactive) fn drain_agent_signals(
    agent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentSignal>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    let mut turn_ended = false;
    while let Ok(signal) = agent_rx.try_recv() {
        if apply_agent_signal(signal, session, state) {
            turn_ended = true;
        }
    }
    turn_ended
}

/// Applies a [`SignalOutcome`] transition consistently across both the main event
/// loop and signal draining. Returns `true` when turn running state or timestamps changed.
pub(in crate::interactive) fn apply_signal_outcome(
    outcome: SignalOutcome,
    state: &mut InteractiveState,
    turn_running: &mut bool,
) -> bool {
    match outcome {
        SignalOutcome::TurnEnded => {
            *turn_running = false;
            let finished = std::time::Instant::now();
            *mehh_ai::lock_recover(&state.turn_completed_at) = Some(finished);
            if let (Some(started), Some(idx)) = (
                *mehh_ai::lock_recover(&state.turn_started_at),
                state
                    .items
                    .iter()
                    .rposition(|it| matches!(it, TranscriptItem::Assistant { .. })),
            ) {
                mehh_ai::lock_recover(&state.thinking_elapsed).insert(
                    idx,
                    finished.saturating_duration_since(started).as_millis() as u64,
                );
            }
            true
        }
        SignalOutcome::BackgroundEnded => {
            *turn_running = false;
            *mehh_ai::lock_recover(&state.turn_completed_at) = Some(std::time::Instant::now());
            true
        }
        SignalOutcome::RetryStarted => {
            *turn_running = true;
            *mehh_ai::lock_recover(&state.turn_started_at) = Some(std::time::Instant::now());
            *mehh_ai::lock_recover(&state.turn_completed_at) = None;
            true
        }
        SignalOutcome::Continuing => false,
    }
}

/// Applies every queued signal in interactive mode, handling automatic overflow retries.
/// Returns `Some(is_still_running)` when turn state changed.
pub(in crate::interactive) async fn drain_agent_signals_async(
    agent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentSignal>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
    mut mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> Option<bool> {
    let mut next_running = None;
    let mut running = false;
    while let Ok(signal) = agent_rx.try_recv() {
        if let AgentSignal::McpAuthCompleted { server, result } = signal {
            finish_mcp_auth_signal(
                server,
                result,
                mcp_manager.as_deref_mut(),
                session,
                state,
                codemode_enabled,
            )
            .await;
            continue;
        }
        let outcome = handle_agent_signal_async(signal, session, state, agent_tx).await;
        if apply_signal_outcome(outcome, state, &mut running) {
            next_running = Some(running);
        }
    }
    next_running
}
