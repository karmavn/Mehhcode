use super::*;

fn format_assistant_stop_notice(msg: &mehh_ai::AssistantMessage) -> String {
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

fn push_turn_error_once(state: &mut InteractiveState, notice: String) {
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


/// Signal delivered from a running agent turn or background prep/compaction task to the UI loop.
pub(crate) enum AgentSignal {
    ReadyForTurn {
        prompt_text: String,
        blocks: Vec<UserContentBlock>,
        new_api_key: Option<String>,
        compaction: Option<(String, String, u64)>,
        warning: Option<String>,
        cancel: CancellationToken,
    },
    PrepFailed(String),
    PrepAborted,
    OverflowCompacted {
        original_messages: Vec<Message>,
        prep: Option<(String, u64)>,
        result: Result<String, String>,
        cancelled: bool,
        prefix_persisted: bool,
    },
    Event(Box<AgentEvent>),
    ToolApprovalRequest(PendingApproval),
    Finished(Vec<Message>),
    Failed(String),
    SystemMessage(String),
    LoginCompleted {
        provider: String,
        result: Result<String, String>,
    },
    McpAuthCompleted {
        server: String,
        result: Result<(), String>,
    },
}

/// Applies a signal from the agent task to the session and the interactive state.
///
/// Returns true when the signal ends the turn, so the caller can stop routing
/// user input into the finished turn.
pub(super) fn apply_agent_signal(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) -> bool {
    match signal {
        AgentSignal::ToolApprovalRequest(pending) => {
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
            state.pending_approval = Some(pending);
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
        | AgentSignal::LoginCompleted { .. }
        | AgentSignal::McpAuthCompleted { .. } => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SignalOutcome {
    Continuing,
    RetryStarted,
    TurnEnded,
}

pub(super) async fn handle_agent_signal_async(
    signal: AgentSignal,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> SignalOutcome {
    match signal {
        AgentSignal::ReadyForTurn {
            prompt_text,
            blocks,
            new_api_key,
            compaction,
            warning,
            cancel,
        } => {
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
                if !session.has_explicit_api_key()
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
                let has_completed_tool = messages
                    .iter()
                    .any(|m| matches!(m, Message::ToolResult(_)));
                if has_completed_tool {
                    if messages.len() >= 2
                        && matches!(messages.last(), Some(Message::Assistant(_)))
                    {
                        // Case B: Mid-turn overflow after tools already ran.
                        // Persist completed prefix BEFORE preparing compaction so Turn-1
                        // and current-turn tool results are visible to compaction.
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
                } else if let Some(prep) = session.prepare_overflow_compaction() {
                    state.is_busy = true;
                    state.status = Some("Context overflow — compacting history...".to_string());
                    start_overflow_compaction(session, prep, messages, false, agent_tx);
                    return SignalOutcome::RetryStarted;
                }
            }
            if apply_agent_signal(AgentSignal::Finished(messages), session, state) {
                SignalOutcome::TurnEnded
            } else {
                SignalOutcome::Continuing
            }
        }
        AgentSignal::OverflowCompacted {
            original_messages,
            prep,
            result,
            cancelled,
            prefix_persisted,
        } => {
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
                }
                Err(e) => state
                    .items
                    .push(TranscriptItem::System(format!("Login failed: {e}"))),
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
pub(super) fn drain_agent_signals(
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

/// Applies every queued signal in interactive mode, handling automatic overflow retries.
/// Returns `Some(is_still_running)` when turn state changed.
async fn drain_agent_signals_async(
    agent_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentSignal>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
    mut mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> Option<bool> {
    let mut next_running = None;
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
        match handle_agent_signal_async(signal, session, state, agent_tx).await {
            SignalOutcome::TurnEnded => next_running = Some(false),
            SignalOutcome::RetryStarted => next_running = Some(true),
            SignalOutcome::Continuing => {}
        }
    }
    next_running
}

/// Splits a submitted TUI prompt into arguments. Quotes group text so
/// `@"path with spaces"` stays one token, and newlines survive the round trip
/// through `process_file_arguments` (which rejoins args with spaces).
fn split_prompt_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            has_token = true;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                has_token = true;
            }
            '\n' | '\r' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
                args.push("\n".to_string());
            }
            c if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Expands `@path` tokens in a submitted TUI prompt into file/image blocks.
pub(super) fn expand_prompt_with_files(
    cwd: &Path,
    text: &str,
) -> anyhow::Result<(String, Vec<UserContentBlock>)> {
    let args = split_prompt_args(text);
    if !args.iter().any(|token| token.starts_with('@')) {
        return Ok((text.to_string(), Vec::new()));
    }
    let processed = crate::process_file_arguments(cwd, &args)?;
    Ok((processed.text, processed.blocks))
}

/// Refreshes credentials and runs optional proactive compaction off the UI thread so the TUI and Ctrl+C remain responsive.
fn start_turn_preparation(
    session: &AgentSession,
    text: &str,
    blocks: Vec<UserContentBlock>,
    auth_file: Option<&Path>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let cancel = CancellationToken::new();
    *mehh_ai::lock_recover(&session.active_cancel_slot()) = Some(cancel.clone());

    let prompt_text = text.to_string();
    let provider_name = session.model.provider.clone();
    let model_api = session.model.api.clone();
    let has_explicit_key = session.has_explicit_api_key();
    let auth_file = auth_file.map(Path::to_path_buf);
    let existing_key = session.api_key.clone();
    let model = session.model.clone();
    let provider = session.provider.clone();
    let proactive_prep = session.prepare_proactive_compaction();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();

    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(async {
            let key_res = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(AgentSignal::PrepAborted);
                    return;
                }
                res = AgentSession::refresh_resolved_api_key(
                    &provider_name,
                    &model_api,
                    has_explicit_key,
                    auth_file.as_deref(),
                ) => res,
            };
            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
                return;
            }
            let new_api_key = match key_res {
                Ok(k) => k,
                Err(e) => {
                    let _ = tx.send(AgentSignal::PrepFailed(format!(
                        "Failed to resolve provider credentials: {e}"
                    )));
                    return;
                }
            };

            let mut compaction = None;
            let mut warning = None;
            if let Some(prep) = proactive_prep {
                let effective_key = new_api_key.as_deref().or(existing_key.as_deref());
                match crate::compaction::compact_with_provider(
                    &prep,
                    &model,
                    provider.as_ref(),
                    effective_key,
                    None,
                    cancel.clone(),
                )
                .await
                {
                    Ok(summary) => {
                        compaction = Some((summary, prep.first_kept_entry_id, prep.tokens_before));
                    }
                    Err(e) => {
                        if cancel.is_cancelled() {
                            let _ = tx.send(AgentSignal::PrepAborted);
                            return;
                        }
                        warning = Some(e.to_string());
                    }
                }
            }

            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
            } else {
                let _ = tx.send(AgentSignal::ReadyForTurn {
                    prompt_text,
                    blocks,
                    new_api_key,
                    compaction,
                    warning,
                    cancel,
                });
            }
        })
        .catch_unwind()
        .await;

        if outcome.is_err() {
            let _ = tx_panic.send(AgentSignal::PrepFailed(
                "turn preparation panicked".to_string(),
            ));
        }
    });
}

fn start_overflow_compaction(
    session: &AgentSession,
    prep: crate::compaction::CompactionPreparation,
    original_messages: Vec<Message>,
    prefix_persisted: bool,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let cancel = CancellationToken::new();
    *mehh_ai::lock_recover(&session.active_cancel_slot()) = Some(cancel.clone());

    let model = session.model.clone();
    let provider = session.provider.clone();
    let api_key = session.api_key.clone();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();
    let prep_meta = (prep.first_kept_entry_id.clone(), prep.tokens_before);

    tokio::spawn(async move {
        let res = AssertUnwindSafe(crate::compaction::compact_with_provider(
            &prep,
            &model,
            provider.as_ref(),
            api_key.as_deref(),
            None,
            cancel.clone(),
        ))
        .catch_unwind()
        .await;

        match res {
            Ok(compact_res) => {
                let cancelled = cancel.is_cancelled();
                let _ = tx.send(AgentSignal::OverflowCompacted {
                    original_messages,
                    prep: Some(prep_meta),
                    result: compact_res.map_err(|e| e.to_string()),
                    cancelled,
                    prefix_persisted,
                });
            }
            Err(_) => {
                let _ = tx_panic.send(AgentSignal::Failed(
                    "overflow compaction panicked".to_string(),
                ));
            }
        }
    });
}

/// Starts an agent turn in a background task and streams its events to the UI loop.
pub(super) fn start_turn(
    session: &AgentSession,
    state: &mut InteractiveState,
    text: &str,
    blocks: Vec<UserContentBlock>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.turn_start_idx = Some(state.items.len());
    state.last_turn_error = None;
    let mut content_blocks = blocks;
    if !content_blocks
        .iter()
        .any(|b| matches!(b, UserContentBlock::Text(_)))
    {
        content_blocks.insert(
            0,
            UserContentBlock::Text(mehh_ai::TextContent {
                text: text.to_string(),
                text_signature: None,
            }),
        );
    }
    let (user_msg, history, config) =
        session.prepare_prompt_content(UserContent::Blocks(content_blocks));

    let tx_events = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
        let _ = tx_events.send(AgentSignal::Event(Box::new(ev)));
    });

    tokio::spawn(async move {
        // A panic inside the agent loop must not wedge the UI: it is reported as a
        // failed turn so the loop can return to the prompt.
        let outcome = AssertUnwindSafe(run_agent_loop(
            vec![user_msg],
            history,
            config,
            Some(on_event),
        ))
        .catch_unwind()
        .await;

        match outcome {
            Ok(messages) => {
                let _ = tx_done.send(AgentSignal::Finished(messages));
            }
            Err(_) => {
                let _ = tx_done.send(AgentSignal::Failed("the agent loop panicked".to_string()));
            }
        }
    });

    state.is_busy = true;
    state.status = Some("Working...".to_string());
}

/// Resumes an agent loop after mid-turn overflow compaction without inserting a new UserMessage.
fn start_continuation_turn(
    session: &AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.last_turn_error = None;
    let (_unused, history, config) =
        session.prepare_prompt_content(UserContent::Text(String::new()));

    let tx_events = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
        let _ = tx_events.send(AgentSignal::Event(Box::new(ev)));
    });

    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(run_agent_loop(
            Vec::new(),
            history,
            config,
            Some(on_event),
        ))
        .catch_unwind()
        .await;

        match outcome {
            Ok(messages) => {
                let _ = tx_done.send(AgentSignal::Finished(messages));
            }
            Err(_) => {
                let _ = tx_done.send(AgentSignal::Failed("the agent loop panicked".to_string()));
            }
        }
    });

    state.is_busy = true;
    state.status = Some("Continuing after compaction...".to_string());
}

/// Spawns an interactive OAuth login flow off the TUI event loop so Ctrl+C and rendering stay live.
fn spawn_oauth_login(
    provider: &str,
    auth_file: Option<PathBuf>,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let provider_owned = provider.to_string();
    state.items.push(TranscriptItem::System(format!(
        "Starting OAuth login for {provider_owned}..."
    )));
    let tx_prompt = agent_tx.clone();
    let tx_done = agent_tx.clone();
    tokio::spawn(async move {
        let res = perform_login_with_prompt(
            &provider_owned,
            None,
            auth_file.as_deref(),
            move |msg| {
                let _ = tx_prompt.send(AgentSignal::SystemMessage(msg));
            },
        )
        .await
        .map_err(|e| e.to_string());
        let _ = tx_done.send(AgentSignal::LoginCompleted {
            provider: provider_owned,
            result: res,
        });
    });
}

/// Spawns an MCP OAuth PKCE flow off the TUI event loop.
fn spawn_mcp_auth(
    name: &str,
    mcp_manager: Option<&McpManager>,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let Some(mgr) = mcp_manager else {
        state.items.push(TranscriptItem::System(
            "MCP is not enabled for this session.".to_string(),
        ));
        return;
    };
    let (url, auth_store) = match mgr.prepare_server_oauth(name) {
        Ok(v) => v,
        Err(e) => {
            state.items.push(TranscriptItem::System(format!(
                "MCP authentication failed for '{name}': {e}"
            )));
            return;
        }
    };
    let server = name.to_string();
    state.items.push(TranscriptItem::System(format!(
        "Starting MCP OAuth authentication for '{server}'..."
    )));
    let tx_prompt = agent_tx.clone();
    let tx_done = agent_tx.clone();
    tokio::spawn(async move {
        let res = McpManager::complete_server_oauth_flow(&server, &url, &auth_store, |auth_url| {
            let _ = tx_prompt.send(AgentSignal::SystemMessage(format!(
                "Open the following URL to authenticate:\n{auth_url}"
            )));
        })
        .await
        .map_err(|e| e.to_string());
        let _ = tx_done.send(AgentSignal::McpAuthCompleted { server, result: res });
    });
}

async fn finish_mcp_auth_signal(
    server: String,
    result: Result<(), String>,
    mcp_manager: Option<&mut McpManager>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    codemode_enabled: bool,
) {
    match result {
        Ok(()) => {
            if let Some(mgr) = mcp_manager {
                match mgr.reconnect_server(&server).await {
                    Ok(summary) => {
                        state.mcp_summaries = mgr.summaries().to_vec();
                        let tools = mgr.tools().to_vec();
                        sync_mcp_tools(session, tools, codemode_enabled);
                        if let Some(err) = summary.error {
                            state.items.push(TranscriptItem::System(format!(
                                "Authenticated MCP server '{server}', but reconnect reported error: {err}"
                            )));
                            return;
                        }
                    }
                    Err(e) => {
                        state.mcp_summaries = mgr.summaries().to_vec();
                        state.items.push(TranscriptItem::System(format!(
                            "Authenticated MCP server '{server}', but reconnect failed: {e}"
                        )));
                        return;
                    }
                }
            }
            state.items.push(TranscriptItem::System(format!(
                "Authenticated MCP server '{server}'"
            )));
        }
        Err(e) => state.items.push(TranscriptItem::System(format!(
            "MCP authentication failed for '{server}': {e}"
        ))),
    }
}


/// Applies one cycle step on a setting row, syncs runtime state, persists it,
/// and updates the inline status banner inside `SettingsList`.
async fn cycle_active_setting_and_refresh(
    id: &str,
    forward: bool,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    if id == "permission_mode" {
        session.settings.permission_mode = Some(session.inspection_manager.mode().await);
    } else if id == "default_thinking_level" {
        session.settings.default_thinking_level =
            Some(session.thinking_level().unwrap_or(ThinkingLevel::Off));
    }
    let status = match apply_setting_cycle_dir(id, forward, session, state) {
        Ok(msg) => {
            if id == "permission_mode" {
                if let Some(mode) = session.settings.permission_mode {
                    session.inspection_manager.set_mode(mode).await;
                }
            }
            match persist_settings(&session.settings, id, state.settings_file.as_deref()) {
                Ok(()) => (format!("Saved: {msg}"), false),
                Err(e) => (format!("{msg} (save failed: {e})"), true),
            }
        }
        Err(e) => (format!("Failed: {e}"), true),
    };
    let snapshot = effective_settings_snapshot(session).await;
    refresh_settings_selector_with_status(state, &snapshot, Some(id), Some(status));
}

/// Handles keyboard input while any non-approval selector modal is open.
pub(super) async fn handle_non_approval_selector_key(
    key_str: &str,
    keybindings: &Keybindings,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    if key_str != "escape" && keybindings.matches(key_str, TuiAction::Interrupt) {
        state.selector = None;
        if let Some(orig) = state.theme_preview_restore.take() {
            state.theme = orig;
        }
        return;
    }

    if let Some(sel) = state.selector.as_mut() {
        if update_selector_filter(sel, key_str) {
            sync_theme_preview(state);
            return;
        }
    }

    let is_settings = state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::Settings);
    match key_str {
        "left" | "right" if is_settings => {
            if let Some(id) = state.selector.as_ref().and_then(settings_selected_id) {
                cycle_active_setting_and_refresh(&id, key_str == "right", session, state).await;
            }
        }
        "enter" | " " | "space" if key_str == "enter" || is_settings => {
            if is_settings {
                if let Some(id) = state.selector.as_ref().and_then(settings_selected_id) {
                    cycle_active_setting_and_refresh(&id, true, session, state).await;
                }
                return;
            }
            let Some(item) = state
                .selector
                .as_ref()
                .and_then(|s| s.list.selected_item())
                .cloned()
            else {
                return;
            };
            let kind = state.selector.as_ref().map(|s| s.kind);
            state.selector = None;
            if kind != Some(SelectorKind::Theme) {
                if let Some(orig) = state.theme_preview_restore.take() {
                    state.theme = orig;
                }
            }
            match kind {
                Some(SelectorKind::Model) => {
                    apply_interactive_model_switch(&item.id, session, state).await;
                }
                Some(SelectorKind::Mode) => {
                    match item.id.parse::<crate::tool_inspection::PermissionMode>() {
                        Ok(mode) => {
                            session.inspection_manager.set_mode(mode).await;
                            session.settings.permission_mode = Some(mode);
                            state.items.push(TranscriptItem::System(format!(
                                "Permission mode set to: {:?} — {} (this session; use /settings to save default)",
                                mode,
                                permission_mode_description(mode)
                            )));
                        }
                        Err(e) => state.items.push(TranscriptItem::System(e)),
                    }
                }
                Some(SelectorKind::Thinking) => {
                    let level = parse_thinking_level_str(&item.id).unwrap_or(ThinkingLevel::Off);
                    set_session_thinking_level(session, level);
                    state.items.push(TranscriptItem::System(format!(
                        "Thinking effort set to {} — {} (this session; use /settings to save default)",
                        thinking_level_label(level),
                        thinking_level_description(level)
                    )));
                }
                Some(SelectorKind::Theme) => {
                    state.theme_preview_restore = None;
                    let msg = apply_theme_choice(&item.id, session, state);
                    state.items.push(TranscriptItem::System(msg));
                }
                Some(SelectorKind::Tree) => {
                    match session.navigate_tree(&item.id, false, None).await {
                        Ok(_) => {
                            state.items = rebuild_transcript_items(session);
                            state.scroll_offset = 0;
                            state.items.push(TranscriptItem::System(format!(
                                "Switched to branch at {}",
                                item.id
                            )));
                        }
                        Err(e) => state.items.push(TranscriptItem::System(format!(
                            "Failed to switch branch: {e}"
                        ))),
                    }
                }
                Some(SelectorKind::Resume) => {
                    match SessionManager::open_file(PathBuf::from(&item.id), session.cwd.clone()) {
                        Ok(mgr) => {
                            session.session = mgr;
                            state.items = rebuild_transcript_items(session);
                            state.scroll_offset = 0;
                            state.items.push(TranscriptItem::System(format!(
                                "Resumed session {}",
                                session.session.session_id()
                            )));
                        }
                        Err(e) => state.items.push(TranscriptItem::System(format!(
                            "Failed to resume session: {e}"
                        ))),
                    }
                }
                Some(SelectorKind::Login) => {
                    let auth_file = state.auth_file.clone();
                    spawn_oauth_login(&item.id, auth_file, state, agent_tx);
                }
                _ => {}
            }
        }
        "escape" => {
            let had_filter = state
                .selector
                .as_ref()
                .is_some_and(|s| !s.list.filter.is_empty());
            if had_filter {
                if let Some(sel) = state.selector.as_mut() {
                    sel.list.set_filter("");
                    if let Some(ref mut sl) = sel.settings_list {
                        sl.set_filter("");
                        sl.clear_status();
                    }
                }
                sync_theme_preview(state);
            } else {
                state.selector = None;
                if let Some(orig) = state.theme_preview_restore.take() {
                    state.theme = orig;
                }
            }
        }
        "up" | "down" | "pageup" | "pagedown" => {
            if let Some(sel) = state.selector.as_mut() {
                if sel.kind == SelectorKind::Settings {
                    if let Some(ref mut sl) = sel.settings_list {
                        match key_str {
                            "up" => sl.move_up(),
                            "down" => sl.move_down(),
                            "pageup" => sl.page_up(),
                            "pagedown" => sl.page_down(),
                            _ => {}
                        }
                        sl.clear_status();
                        sel.list.selected = sl.selected;
                    } else {
                        match key_str {
                            "up" => sel.list.move_up(),
                            "down" => sel.list.move_down(),
                            "pageup" => sel.list.page_up(),
                            "pagedown" => sel.list.page_down(),
                            _ => {}
                        }
                    }
                } else {
                    match key_str {
                        "up" => sel.list.move_up(),
                        "down" => sel.list.move_down(),
                        "pageup" => sel.list.page_up(),
                        "pagedown" => sel.list.page_down(),
                        _ => {}
                    }
                }
            }
            sync_theme_preview(state);
        }
        _ => {}
    }
}

/// Runs the full interactive TUI mode loop.
pub async fn run_interactive_mode(
    session: &mut AgentSession,
    mcp_summaries: &[McpConnectionSummary],
    mut mcp_manager: Option<&mut McpManager>,
    codemode_enabled: bool,
) -> anyhow::Result<()> {
    let mut terminal = CrosstermTerminal::new()?;
    // CrosstermTerminal hides the hardware cursor and enters the alternate screen.
    let mut renderer = TuiRenderer::new();
    let mut state = InteractiveState::new(mcp_summaries.to_vec(), &session.settings);
    let mut editor = Editor::new()
        .with_prefix("> ")
        .with_placeholder("Type a message or /help...")
        .with_theme(state.theme.clone());
    let keybindings = state.keybindings.clone();

    // Background input reader thread with cooperative stop flag so exiting
    // interactive mode never leaks a permanently blocked read thread.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<crossterm::event::Event>(128);
    let reader_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop_bg = reader_stop.clone();
    let reader_handle = std::thread::spawn(move || {
        while !reader_stop_bg.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(evt) => {
                        if event_tx.blocking_send(evt).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    // Agent event channel. The extra sender keeps the channel open between turns.
    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel::<AgentSignal>();
    let _agent_keepalive = agent_tx.clone();

    let approval_broker = Arc::new(InteractiveApprovalBroker::new(agent_tx.clone()));
    session.set_approval_handler(Some(approval_broker));

    let mut tick_interval = tokio::time::interval(Duration::from_millis(80));
    let mut turn_running = false;
    let mut wheel_accelerator = WheelScrollAccelerator::default();

    // Render initial screen
    render_now(&state, &editor, session, &mut terminal, &mut renderer);

    'outer: loop {
        tokio::select! {
            evt = event_rx.recv() => {
                let Some(evt) = evt else { break 'outer; };
                match evt {
                    Event::Resize(_new_cols, _new_rows) => {
                        renderer.reset();
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            let lines = wheel_accelerator.next(-1, now_ms);
                            let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                            scroll_up_anchored(&mut state, lines, max_scroll);
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                        MouseEventKind::ScrollDown => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs_f64() * 1000.0)
                                .unwrap_or(0.0);
                            let lines = wheel_accelerator.next(1, now_ms);
                            let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                            scroll_down_anchored(&mut state, lines, max_scroll);
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                        _ => {}
                    },
                    Event::Paste(pasted) => {
                        editor.insert_text(&pasted);
                        let text = editor.text();
                        update_autocomplete_with_context(
                            &mut editor,
                            &text,
                            &session.cwd,
                            Some(session),
                            session.settings.theme.as_deref().or(Some(state.theme.name.as_str())),
                        );
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    Event::Key(key_event) => {
                        // Apply agent signals that are already queued so a key press
                        // cannot start a new turn before the previous one was persisted.
                        if let Some(running) = drain_agent_signals_async(
                            &mut agent_rx,
                            session,
                            &mut state,
                            &agent_tx,
                            mcp_manager.as_deref_mut(),
                            codemode_enabled,
                        )
                        .await
                        {
                            turn_running = running;
                        }
                        if let Some(key_str) = normalize_crossterm_key(key_event) {
                            if key_str == "ctrl+t" {
                                state.thinking_expanded = !state.thinking_expanded;
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            // 1. Selector overlay handling
                            if state.selector.is_some() {
                                if state.selector.as_ref().map(|s| s.kind) == Some(SelectorKind::ToolApproval) {
                                    let selected_id = state
                                        .selector
                                        .as_ref()
                                        .and_then(|s| s.list.selected_item())
                                        .map(|i| i.id.clone());
                                    let decision = tool_approval_decision(
                                        &key_str,
                                        &keybindings,
                                        selected_id.as_deref(),
                                    );
                                    match key_str.as_str() {
                                        "up" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.move_up();
                                            }
                                        }
                                        "down" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.move_down();
                                            }
                                        }
                                        "pageup" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.page_up();
                                            }
                                        }
                                        "pagedown" => {
                                            if let Some(sel) = state.selector.as_mut() {
                                                sel.list.page_down();
                                            }
                                        }
                                        _ => {}
                                    }

                                    if let Some(decision) = decision {
                                        state.selector = None;
                                        if let Some(pending) = state.pending_approval.take() {
                                            if decision.persist_rule || decision.session_rule {
                                                let arg_prefix = pending
                                                    .args
                                                    .get("command")
                                                    .and_then(|v| v.as_str())
                                                    .and_then(mehh_execpolicy::safe_rule_prefix);
                                                if mehh_execpolicy::is_shell_tool(&pending.tool_name)
                                                    && arg_prefix.is_none()
                                                {
                                                    // An empty prefix would allow every command for
                                                    // this shell. Refuse instead of widening policy.
                                                    state.items.push(TranscriptItem::System(
                                                        "No rule saved: command is compound or too broad to allow safely".to_string(),
                                                    ));
                                                } else {
                                                    let rule = crate::permissions::PermissionRule {
                                                        tool_name: pending.tool_name.clone(),
                                                        arg_prefix,
                                                        path_glob: None,
                                                        action: crate::permissions::PermissionAction::AlwaysAllow,
                                                    };
                                                    let save_result = if decision.persist_rule {
                                                        Some(session.inspection_manager.add_permission_rule(rule).await)
                                                    } else {
                                                        session.inspection_manager.add_session_permission_rule(rule).await;
                                                        None
                                                    };
                                                    state.items.push(TranscriptItem::System(
                                                        permission_rule_status_message(
                                                            decision.persist_rule,
                                                            &pending.tool_name,
                                                            save_result.as_ref(),
                                                        ),
                                                    ));
                                                }
                                            }
                                            let _ = pending.reply_tx.send(decision.allow);
                                        }
                                    }
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }

                                handle_non_approval_selector_key(
                                    &key_str,
                                    &keybindings,
                                    session,
                                    &mut state,
                                    &agent_tx,
                                )
                                .await;
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }

                            // 2. Interrupt handling depends on whether a turn is running
                            if turn_running {
                                if keybindings.matches(&key_str, TuiAction::Interrupt) {
                                    let aborted = session.abort();
                                    state.items.push(TranscriptItem::System(
                                        if aborted {
                                            "Cancelling current turn...".to_string()
                                        } else {
                                            "No running turn to cancel.".to_string()
                                        },
                                    ));
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }

                                if key_str == "alt+enter" {
                                    let text = editor.text();
                                    let trimmed = text.trim().to_string();
                                    if !trimmed.is_empty() {
                                        session.follow_up(trimmed.clone());
                                        editor.clear();
                                        state.items.push(TranscriptItem::System(format!(
                                            "Queued follow-up: {trimmed}"
                                        )));
                                    }
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                    continue 'outer;
                                }
                            } else if keybindings.matches(&key_str, TuiAction::Interrupt) && editor.is_empty() {
                                break 'outer;
                            }

                            // Scroll keybindings operate regardless of turn state when selector is closed
                            if keybindings.matches(&key_str, TuiAction::ScrollUp) {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                scroll_up_anchored(&mut state, 5, max_scroll);
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            if keybindings.matches(&key_str, TuiAction::ScrollDown) {
                                let max_scroll = state.last_max_scroll.load(Ordering::Relaxed);
                                scroll_down_anchored(&mut state, 5, max_scroll);
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }
                            if keybindings.matches(&key_str, TuiAction::ScrollBottom) {
                                state.scroll_offset = 0;
                                state.scroll_anchor.store(0, Ordering::Relaxed);
                                render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                continue 'outer;
                            }

                            // 3. Normal editor input handling
                            let output = editor.handle_editor_key(&key_str, &keybindings);
                            match output {
                                EditorOutput::Submit(submitted) => {
                                    let trimmed = submitted.trim();
                                    if trimmed.is_empty() {
                                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        continue 'outer;
                                    }

                                    if turn_running {
                                        match route_busy_input(trimmed) {
                                            BusyInput::Steer(text) => {
                                                session.steer(text.clone());
                                                state.items.push(TranscriptItem::System(format!(
                                                    "Queued steering message: {text}"
                                                )));
                                            }
                                            BusyInput::FollowUp(text) => {
                                                session.follow_up(text.clone());
                                                state.items.push(TranscriptItem::System(format!(
                                                    "Queued follow-up: {text}"
                                                )));
                                            }
                                        }
                                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        continue 'outer;
                                    }

                                    match handle_slash_command(
                                        trimmed,
                                        session,
                                        &mut state,
                                        mcp_manager.as_deref_mut(),
                                        codemode_enabled,
                                    ).await {
                                        SlashCommandOutcome::Quit => {
                                            break 'outer;
                                        }
                                        SlashCommandOutcome::Handled => {
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::StartOAuthLogin(provider) => {
                                            let auth_file = state.auth_file.clone();
                                            spawn_oauth_login(
                                                &provider,
                                                auth_file,
                                                &mut state,
                                                &agent_tx,
                                            );
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::StartMcpAuth(name) => {
                                            spawn_mcp_auth(
                                                &name,
                                                mcp_manager.as_deref(),
                                                &mut state,
                                                &agent_tx,
                                            );
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::Compact(custom_instructions) => {
                                            state.is_busy = true;
                                            state.status = Some("Compacting session history...".to_string());
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);

                                            let compact_res = session.compact(custom_instructions.as_deref()).await;
                                            state.invalidate_context_tokens();
                                            state.is_busy = false;
                                            state.status = None;

                                            match compact_res {
                                                Ok(Some(summary)) => {
                                                    state.items.push(TranscriptItem::System(format!(
                                                        "Compaction complete: {summary}"
                                                    )));
                                                }
                                                Ok(None) => {
                                                    state.items.push(TranscriptItem::System(
                                                        "History below compaction threshold; no compaction performed.".to_string(),
                                                    ));
                                                }
                                                Err(e) => {
                                                    state.items.push(TranscriptItem::System(format!(
                                                        "Compaction error: {e}"
                                                    )));
                                                }
                                            }

                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                        }
                                        SlashCommandOutcome::NotACommand => {
                                            if let Some(ref mut mgr) = mcp_manager {
                                                if mgr.take_tools_dirty() {
                                                    if let Ok(true) = mgr.refresh_server_tools().await {
                                                        sync_mcp_tools(session, mgr.tools().to_vec(), codemode_enabled);
                                                        state.items.push(TranscriptItem::System(
                                                            "MCP tools updated via tools/list_changed.".to_string(),
                                                        ));
                                                    }
                                                }
                                            }
                                            let (expanded_text, prompt_blocks) =
                                                match expand_prompt_with_files(&session.cwd, trimmed) {
                                                    Ok(v) => v,
                                                    Err(e) => {
                                                        state.items.push(TranscriptItem::System(
                                                            format!("Failed to attach @file: {e}"),
                                                        ));
                                                        render_now(
                                                            &state,
                                                            &editor,
                                                            session,
                                                            &mut terminal,
                                                            &mut renderer,
                                                        );
                                                        continue 'outer;
                                                    }
                                                };
                                            state.scroll_offset = 0;
                                            state.items.push(TranscriptItem::User(trimmed.to_string()));
                                            state.last_prompt_text = Some(trimmed.to_string());
                                            // Keep the fully expanded prompt so the overflow
                                            // retry does not drop attached @file content.
                                            state.last_prompt_content = Some((
                                                expanded_text.clone(),
                                                prompt_blocks.clone(),
                                            ));
                                            state.overflow_retried = false;
                                            state.is_busy = true;
                                            state.status = Some("Preparing turn...".to_string());
                                            *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
                                            *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                            start_turn_preparation(
                                                session,
                                                &expanded_text,
                                                prompt_blocks,
                                                state.auth_file.as_deref(),
                                                &agent_tx,
                                            );
                                            turn_running = true;
                                        }
                                    }
                                }
                                EditorOutput::Changed(current_text) => {
                                    update_autocomplete_with_context(
                                        &mut editor,
                                        &current_text,
                                        &session.cwd,
                                        Some(session),
                                        session.settings.theme.as_deref().or(Some(state.theme.name.as_str())),
                                    );
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                }
                                EditorOutput::None => {
                                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some(sig) = agent_rx.recv() => {
                if let AgentSignal::McpAuthCompleted { server, result } = sig {
                    finish_mcp_auth_signal(
                        server,
                        result,
                        mcp_manager.as_deref_mut(),
                        session,
                        &mut state,
                        codemode_enabled,
                    )
                    .await;
                    render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    continue 'outer;
                }
                let render_immediately = matches!(
                    sig,
                    AgentSignal::SystemMessage(_) | AgentSignal::LoginCompleted { .. }
                );
                match handle_agent_signal_async(sig, session, &mut state, &agent_tx).await {
                    SignalOutcome::TurnEnded => {
                        turn_running = false;
                        let finished = std::time::Instant::now();
                        *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(finished);
                        if let (Some(started), Some(idx)) = (
                            *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()),
                            state.items.iter().rposition(|it| matches!(it, TranscriptItem::Assistant { .. })),
                        ) {
                            state.thinking_elapsed.lock().unwrap_or_else(|e| e.into_inner()).insert(
                                idx,
                                finished.saturating_duration_since(started).as_millis() as u64,
                            );
                        }
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    SignalOutcome::RetryStarted => {
                        turn_running = true;
                        *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
                        *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        render_now(&state, &editor, session, &mut terminal, &mut renderer);
                    }
                    SignalOutcome::Continuing => {
                        if state.selector.is_some() || render_immediately {
                            render_now(&state, &editor, session, &mut terminal, &mut renderer);
                        }
                    }
                }
                // Streaming updates are painted by the 80 ms tick, so a burst of
                // token deltas does not rebuild the frame for every delta.
            }
            _ = tick_interval.tick(), if turn_running => {
                state.loader.tick();
                render_now(&state, &editor, session, &mut terminal, &mut renderer);
            }
        }
    }

    reader_stop.store(true, Ordering::Relaxed);
    drop(event_rx);
    let _ = reader_handle.join();
    let _ = terminal.write("\n");
    let _ = terminal.flush();
    Ok(())
}

