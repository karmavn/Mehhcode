use anyhow::{Context, Result};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use mehh_agent::{run_agent_loop, AgentEvent, AgentLoopConfig, DEFAULT_MAX_AGENT_TURNS};
use mehh_ai::{now_ms, Message, ThinkingLevel, UserContent, UserMessage};

use super::{
    can_retry_overflow_from_scratch, normalize_thinking_for_model, ActiveCancelGuard, AgentSession,
};

/// Marker used by `TodoState::format_moim_block` and by the idempotent injector below.
const MOIM_START: &str = "<working_checklist>";
const MOIM_END: &str = "</working_checklist>";

/// Replaces a previously injected MOIM block in place, or appends it once,
/// removing any duplicate MOIM blocks from the same string.
fn replace_moim_text(text: &mut String, block: &str) {
    if let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, block);
            let tail_start = start + block.len();
            if tail_start < text.len() {
                let mut tail = text[tail_start..].to_string();
                if strip_moim_text(&mut tail) {
                    text.replace_range(tail_start.., &tail);
                }
            }
            return;
        }
    }
    text.push_str("\n\n");
    text.push_str(block);
}

/// Removes all previously injected MOIM blocks from `text`. Returns whether text changed.
fn strip_moim_text(text: &mut String) -> bool {
    let mut changed = false;
    while let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, "");
            changed = true;
        } else {
            break;
        }
    }
    changed
}

/// Strips all MOIM blocks from a `UserMessage`. Returns whether the message changed.
fn strip_moim_from_user_message(u: &mut UserMessage) -> bool {
    match u.content {
        UserContent::Text(ref mut t) => strip_moim_text(t),
        UserContent::Blocks(ref mut blocks) => {
            let mut changed = false;
            let mut emptied_moim = vec![false; blocks.len()];
            for (idx, b) in blocks.iter_mut().enumerate() {
                if let mehh_ai::UserContentBlock::Text(t) = b {
                    if strip_moim_text(&mut t.text) {
                        changed = true;
                        if t.text.trim().is_empty() {
                            emptied_moim[idx] = true;
                        }
                    }
                }
            }
            if emptied_moim.iter().any(|&e| e) {
                let mut iter = emptied_moim.into_iter();
                blocks.retain(|_| !iter.next().unwrap_or(false));
            }
            changed
        }
    }
}

impl AgentSession {
    /// Prepares input, history, and agent loop configuration for a prompt turn with arbitrary user content.
    pub fn prepare_prompt_content(
        &self,
        content: UserContent,
    ) -> (Message, Vec<Message>, AgentLoopConfig) {
        // Routing decisions for the first turn must see a fresh prompt counter.
        mehh_ai::lock_recover(&self.lead_worker).reset_for_new_prompt();
        let expanded_content = match content {
            UserContent::Text(text) => UserContent::Text(self.expand_input(&text)),
            UserContent::Blocks(blocks) => {
                let expanded_blocks = blocks
                    .into_iter()
                    .map(|b| match b {
                        mehh_ai::UserContentBlock::Text(t) => {
                            mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                                text: self.expand_input(&t.text),
                                text_signature: t.text_signature,
                            })
                        }
                        image => image,
                    })
                    .collect();
                UserContent::Blocks(expanded_blocks)
            }
        };

        let user_msg = Message::User(UserMessage {
            content: expanded_content,
            timestamp: now_ms(),
        });

        let history = self.session.build_context_messages();

        let cancel = CancellationToken::new();
        {
            let mut slot = mehh_ai::lock_recover(&self.active_cancel);
            // Idempotent handoff: a cancelled preparation token means Ctrl+C
            // fired before this loop token existed. Carry the cancellation
            // over instead of blindly replacing it and running the turn.
            if slot.as_ref().is_some_and(|prev| prev.is_cancelled()) {
                cancel.cancel();
            }
            *slot = Some(cancel.clone());
        }

        let mut config = AgentLoopConfig::new(self.model.clone(), self.provider.clone());
        config.steering_queue = self.steering_queue.clone();
        config.follow_up_queue = self.follow_up_queue.clone();
        config.steering_mode = self.settings.steering_mode.unwrap_or_default();
        config.follow_up_mode = self.settings.follow_up_mode.unwrap_or_default();
        config.cancel = cancel;
        config.system_prompt = Some(self.system_prompt.clone());
        config.tools = self.tools.clone();
        config.dynamic_tools = Some(Arc::clone(&self.dynamic_tools));
        config.max_turns = Some(DEFAULT_MAX_AGENT_TURNS);
        config.stream_options.api_key = self.api_key.clone();
        {
            let mgr = mehh_ai::lock_recover(&self.lead_worker);
            if mgr.should_use_worker() {
                config.model = mgr.active_model();
                config.provider = mgr.active_provider();
                config.stream_options.api_key = mgr.active_api_key().or_else(|| {
                    if config.model.provider.eq_ignore_ascii_case(&self.model.provider) {
                        self.api_key.clone()
                    } else {
                        None
                    }
                });
            }
        }
        let (effective_reasoning, send_explicit_off) = normalize_thinking_for_model(
            &config.model,
            self.thinking_level,
            self.thinking_explicitly_off,
        );
        config.stream_options.reasoning = match effective_reasoning {
            Some(lvl) => Some(lvl),
            None if send_explicit_off => Some(ThinkingLevel::Off),
            None => None,
        };
        if let Some(t) = self.settings.temperature {
            config.stream_options.temperature = Some(t);
        }
        if let Some(m) = self.settings.max_tokens {
            config.stream_options.max_tokens = Some(m);
        }

        let inspection_mgr = self.inspection_manager.clone();
        let approval_h = self.approval_handler.clone();
        let session_cwd = self.cwd.clone();

        config.before_tool_call = Some(Arc::new(move |before_ctx, cancel| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_h = approval_h.clone();
            let session_cwd = session_cwd.clone();

            Box::pin(async move {
                let tool_name = before_ctx.tool_call.name.clone();
                let tool_call_id = before_ctx.tool_call.id.clone();
                let is_read_only = before_ctx
                    .context
                    .tools
                    .as_ref()
                    .and_then(|tools| tools.iter().find(|t| t.name() == tool_name))
                    .and_then(|t| t.is_safe_for_auto_approve(&before_ctx.args));

                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id,
                    tool_name,
                    args: before_ctx.args,
                    is_read_only,
                    cwd: session_cwd,
                    origin: None,
                };

                inspection_mgr
                    .inspect_and_authorize_cancellable(&inspect_ctx, approval_h.as_deref(), &cancel)
                    .await
            })
        }));

        let lead_worker_outcomes = self.lead_worker.clone();
        config.after_tool_call = Some(Arc::new(move |after_ctx, _cancel| {
            mehh_ai::lock_recover(&lead_worker_outcomes).record_tool_outcome(after_ctx.is_error);
            Box::pin(async { mehh_agent::types::AfterToolCallResult::default() })
        }));

        let todo_state = self.todo_state.clone();
        let session_cwd_moim = self.cwd.clone();
        let session_trusted = self.trusted;

        config.prepare_request = Some(Arc::new(move |req_ctx, _cancel| {
            let todo_state = todo_state.clone();
            let session_cwd = session_cwd_moim.clone();

            Box::pin(async move {
                let block_opt = {
                    let read = todo_state.read().await;
                    read.format_moim_block(&session_cwd, session_trusted)
                };

                let mut ctx = req_ctx.context;
                let mut changed = false;
                let message_has_moim = |msg: &Message| -> bool {
                    match msg {
                        Message::User(u) => match &u.content {
                            UserContent::Text(t) => t.contains(MOIM_START),
                            UserContent::Blocks(blocks) => blocks.iter().any(|b| match b {
                                mehh_ai::UserContentBlock::Text(t) => t.text.contains(MOIM_START),
                                _ => false,
                            }),
                        },
                        _ => false,
                    }
                };

                if let Some(ref moim_block) = block_opt {
                    let last_user_idx = ctx
                        .messages
                        .iter()
                        .rposition(|m| matches!(m, Message::User(_)));
                    let has_earlier_moim = last_user_idx.is_some_and(|last_idx| {
                        ctx.messages[..last_idx].iter().any(message_has_moim)
                    });

                    let messages = Arc::make_mut(&mut ctx.messages);
                    if has_earlier_moim {
                        if let Some(last_idx) = last_user_idx {
                            for msg in &mut messages[..last_idx] {
                                if let Message::User(ref mut u) = msg {
                                    changed |= strip_moim_from_user_message(u);
                                }
                            }
                        }
                    }

                    if let Some(last_idx) = last_user_idx {
                        if let Some(Message::User(ref mut u)) = messages.get_mut(last_idx) {
                            match u.content {
                                UserContent::Text(ref mut t) => {
                                    replace_moim_text(t, moim_block);
                                    changed = true;
                                }
                                UserContent::Blocks(ref mut blocks) => {
                                    let first_moim_idx = blocks.iter().position(|b| match b {
                                        mehh_ai::UserContentBlock::Text(t) => {
                                            t.text.contains(MOIM_START)
                                        }
                                        _ => false,
                                    });
                                    if let Some(idx) = first_moim_idx {
                                        if let Some(mehh_ai::UserContentBlock::Text(t)) =
                                            blocks.get_mut(idx)
                                        {
                                            replace_moim_text(&mut t.text, moim_block);
                                        }
                                        let mut emptied = vec![false; blocks.len()];
                                        for (i, b) in blocks.iter_mut().enumerate() {
                                            if i == idx {
                                                continue;
                                            }
                                            if let mehh_ai::UserContentBlock::Text(t) = b {
                                                if strip_moim_text(&mut t.text)
                                                    && t.text.trim().is_empty()
                                                {
                                                    emptied[i] = true;
                                                }
                                            }
                                        }
                                        if emptied.iter().any(|&e| e) {
                                            let mut iter = emptied.into_iter();
                                            blocks.retain(|_| !iter.next().unwrap_or(false));
                                        }
                                    } else {
                                        blocks.push(mehh_ai::UserContentBlock::Text(
                                            mehh_ai::TextContent {
                                                text: format!("\n\n{}", moim_block),
                                                text_signature: None,
                                            },
                                        ));
                                    }
                                    changed = true;
                                }
                            }
                        }
                    } else {
                        // Fallback: If no User message exists in history, insert after any leading System messages.
                        let insert_idx = messages
                            .iter()
                            .position(|m| !matches!(m, Message::System(_)))
                            .unwrap_or(messages.len());
                        messages.insert(
                            insert_idx,
                            Message::User(UserMessage {
                                content: UserContent::Text(moim_block.clone()),
                                timestamp: now_ms(),
                            }),
                        );
                        changed = true;
                    }
                } else {
                    // Checklist cleared: only clone-on-write when a stale MOIM block is present.
                    let has_stale_moim = ctx.messages.iter().any(message_has_moim);
                    if has_stale_moim {
                        for msg in Arc::make_mut(&mut ctx.messages).iter_mut() {
                            if let Message::User(u) = msg {
                                changed |= strip_moim_from_user_message(u);
                            }
                        }
                    }
                }

                if changed {
                    Some(mehh_agent::AgentRequestUpdate {
                        context: Some(ctx),
                        ..Default::default()
                    })
                } else {
                    None
                }
            })
        }));

        let lead_worker_turn = self.lead_worker.clone();
        let requested_thinking = self.thinking_level;
        let thinking_explicitly_off = self.thinking_explicitly_off;
        config.prepare_next_turn = Some(Arc::new(move |turn_ctx, _cancel| {
            let lead_worker = lead_worker_turn.clone();
            Box::pin(async move {
                let mut mgr = mehh_ai::lock_recover(&lead_worker);
                let stop_reason = turn_ctx.assistant_message.stop_reason;
                let is_empty_assistant = turn_ctx.assistant_message.content.is_empty();
                let repetition_detected = turn_ctx.tool_results.iter().any(|tr| {
                    tr.is_error
                        && tr.content.iter().any(|b| match b {
                            mehh_ai::ToolResultContentBlock::Text(t) => {
                                t.text.contains("Repeated identical tool call")
                            }
                            _ => false,
                        })
                });
                let is_repetition_break = is_empty_assistant || repetition_detected;
                mgr.record_turn_outcome(stop_reason, is_repetition_break);

                if mgr.is_active() {
                    let next_model = mgr.active_model();
                    let next_provider = mgr.active_provider();

                    if next_model.id != turn_ctx.assistant_message.model
                        || next_model.provider != turn_ctx.assistant_message.provider
                    {
                        let next_api_key = mgr.active_api_key();
                        let (eff, send_off) = normalize_thinking_for_model(
                            &next_model,
                            requested_thinking,
                            thinking_explicitly_off,
                        );
                        let next_thinking = match eff {
                            Some(lvl) => Some(lvl),
                            None if send_off => Some(ThinkingLevel::Off),
                            None => None,
                        };
                        Some(mehh_agent::AgentLoopTurnUpdate {
                            model: Some(next_model),
                            provider: Some(next_provider),
                            api_key: Some(next_api_key),
                            thinking_level: Some(next_thinking),
                            ..Default::default()
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        }));

        (user_msg, history, config)
    }

    /// Prepares input, history, and agent loop configuration for a prompt turn.
    pub fn prepare_prompt(&self, text: &str) -> (Message, Vec<Message>, AgentLoopConfig) {
        self.prepare_prompt_content(UserContent::Text(text.to_string()))
    }

    /// Appends new messages from an agent turn to the session manager.
    pub fn finish_prompt(&mut self, new_messages: &[Message]) -> Result<()> {
        self.session
            .append_entries(new_messages.iter().cloned().map(|message| {
                crate::session::SessionEntryKind::Message { message }
            }))?;
        Ok(())
    }

    /// Sends user content through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt_content(
        &mut self,
        content: UserContent,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        let (pre_cancel, mut cancel_guard) =
            ActiveCancelGuard::install_fresh(self.active_cancel.clone());

        let auth_res = tokio::select! {
            _ = pre_cancel.cancelled() => anyhow::bail!("Operation aborted"),
            res = self.ensure_api_key(None) => res,
        };
        auth_res?;
        if pre_cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }

        self.run_proactive_compaction().await;
        if pre_cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }

        let (user_msg, history, config) = self.prepare_prompt_content(content.clone());
        cancel_guard.set_token(config.cancel.clone());
        let new_messages = run_agent_loop(vec![user_msg], history, config, on_event.clone()).await;
        cancel_guard.disarm_and_clear();

        if self.is_overflow_turn(&new_messages) {
            if can_retry_overflow_from_scratch(&new_messages) {
                // Case A: Clean turn-0 overflow (only initial User + overflow Assistant) -> safe to compact & retry prompt.
                match self.try_overflow_compaction().await {
                    Ok(true) => {
                        let (retry_user, retry_history, retry_config) =
                            self.prepare_prompt_content(content);
                        cancel_guard.set_token(retry_config.cancel.clone());
                        let retry_messages =
                            run_agent_loop(vec![retry_user], retry_history, retry_config, on_event)
                                .await;
                        cancel_guard.disarm_and_clear();
                        self.finish_prompt(&retry_messages)?;
                        return Ok(retry_messages);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // The overflow turn must not be persisted: it is only a
                        // trigger for compaction, and retry deliberately avoids
                        // saving it. Surface the failure instead of swallowing it.
                        return Err(e).context("Overflow auto-compaction failed");
                    }
                }
            } else if new_messages.len() >= 2
                && matches!(new_messages.last(), Some(Message::Assistant(_)))
            {
                // Case B: Mid-turn overflow after completed turns, tools, or drained queue messages.
                // Persist completed prefix (everything before the trailing overflow AssistantMessage),
                // compact the session, and resume with empty prompts so prior turns/tools are never re-executed.
                let prefix = &new_messages[..new_messages.len() - 1];
                self.finish_prompt(prefix)?;
                match self.try_overflow_compaction().await {
                    Ok(true) => {
                        let (_, retry_history, retry_config) =
                            self.prepare_prompt_content(UserContent::Text(String::new()));
                        cancel_guard.set_token(retry_config.cancel.clone());
                        let continuation =
                            run_agent_loop(Vec::new(), retry_history, retry_config, on_event)
                                .await;
                        cancel_guard.disarm_and_clear();
                        self.finish_prompt(&continuation)?;
                        let mut combined = prefix.to_vec();
                        combined.extend(continuation);
                        return Ok(combined);
                    }
                    Ok(false) => {
                        self.finish_prompt(&new_messages[new_messages.len() - 1..])?;
                        return Ok(new_messages);
                    }
                    Err(e) => return Err(e).context("Overflow auto-compaction failed"),
                }
            }
        }

        self.finish_prompt(&new_messages)?;
        Ok(new_messages)
    }

    /// Sends a user prompt through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt(
        &mut self,
        text: &str,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        self.prompt_content(UserContent::Text(text.to_string()), on_event)
            .await
    }
}
