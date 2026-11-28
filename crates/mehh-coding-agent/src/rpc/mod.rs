//! Headless RPC protocol for embedding the coding agent over asynchronous stdin/stdout or duplex streams.

mod protocol;
#[cfg(test)]
mod tests;

pub use protocol::format_rpc_event;
#[cfg(test)]
use protocol::handle_concurrent_command;
use protocol::{
    run_with_concurrent_commands, RpcOutput, MAX_PENDING_LINES, MAX_QUEUED_OUTBOUND_EVENTS,
};

use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio_util::sync::CancellationToken;

use mehh_agent::AgentEvent;
use mehh_ai::{Message, StopReason, ThinkingLevel};

use crate::agent_session::AgentSession;
use crate::session::SessionManager;

/// Runs the agent session in RPC mode, reading JSONL commands from `reader` and writing
/// streamed events and command responses to `writer`.
pub async fn run_rpc_mode<R, W>(session: &mut AgentSession, mut reader: R, writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let output = RpcOutput::new(writer);
    let mut pending_lines: VecDeque<String> = VecDeque::new();

    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<Result<String>>(MAX_PENDING_LINES);
    let reader_fut = async move {
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if line_tx.send(Ok(std::mem::take(&mut buf))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = line_tx.send(Err(e.into())).await;
                    break;
                }
            }
        }
    };
    tokio::pin!(reader_fut);
    let mut reader_done = false;
    let mut rx_closed = false;

    loop {
        let raw_line = if let Some(queued) = pending_lines.pop_front() {
            queued
        } else {
            let mut next = None;
            while next.is_none() && !rx_closed {
                tokio::select! {
                    _ = reader_fut.as_mut(), if !reader_done => {
                        reader_done = true;
                    }
                    msg = line_rx.recv() => {
                        match msg {
                            Some(Ok(l)) => next = Some(l),
                            Some(Err(e)) => return Err(e),
                            None => {
                                rx_closed = true;
                            }
                        }
                    }
                }
            }
            match next {
                Some(l) => l,
                None => break,
            }
        };

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                output
                    .send_error(None, "parse", &format!("Malformed JSON: {e}"))
                    .await?;
                continue;
            }
        };

        let id = val.get("id").and_then(|v| v.as_str());
        let cmd_type = match val.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                output
                    .send_error(id, "unknown", "Missing 'type' field in command")
                    .await?;
                continue;
            }
        };

        match cmd_type {
            "prompt" => {
                let message = val.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if message.trim().is_empty() {
                    output
                        .send_error(id, "prompt", "Message must not be empty")
                        .await?;
                    continue;
                }
                let (event_tx, mut event_rx) =
                    tokio::sync::mpsc::channel::<AgentEvent>(MAX_QUEUED_OUTBOUND_EVENTS);
                let out_clone = output.clone();

                let drain_handle = tokio::spawn(async move {
                    while let Some(ev) = event_rx.recv().await {
                        if out_clone.send_event(&ev).await.is_err() {
                            break;
                        }
                    }
                });

                let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
                    let _ = event_tx.try_send(ev);
                });

                let cancel_slot = session.active_cancel_slot();
                let sub_control = session.subagent_control();
                let steering_queue = session.steering_queue();
                let follow_queue = session.follow_up_queue();
                let bash_cancel_slot = session.active_bash_cancel.clone();
                let (prompt_res, prompt_aborted) = run_with_concurrent_commands(
                    session.prompt(message, Some(on_event)),
                    &mut reader_fut,
                    &mut reader_done,
                    &mut line_rx,
                    &mut rx_closed,
                    &mut pending_lines,
                    &output,
                    &sub_control,
                    &steering_queue,
                    &follow_queue,
                    &cancel_slot,
                    &bash_cancel_slot,
                )
                .await?;

                // Allow event drain task to finish processing buffered events
                let _ = drain_handle.await;

                match prompt_res {
                    Ok(new_messages) => {
                        let stop_err = new_messages.iter().rev().find_map(|m| match m {
                            Message::Assistant(a)
                                if matches!(
                                    a.stop_reason,
                                    StopReason::Error | StopReason::Aborted
                                ) =>
                            {
                                Some(a.error_message.clone().unwrap_or_else(|| {
                                    format!("Agent stopped with {:?}", a.stop_reason)
                                }))
                            }
                            _ => None,
                        });
                        if let Some(err_msg) = stop_err {
                            output.send_error(id, "prompt", &err_msg).await?;
                        } else if prompt_aborted {
                            output.send_error(id, "prompt", "Operation aborted").await?;
                        } else {
                            output.send_success(id, "prompt", None).await?;
                        }
                    }
                    Err(e) => {
                        output.send_error(id, "prompt", &e.to_string()).await?;
                    }
                }
            }
            "get_state" => {
                let state_data = serde_json::json!({
                    "model": session.model(),
                    "thinkingLevel": session.effective_thinking_level(),
                    "sessionId": session.session().session_id(),
                    "sessionFile": session.session().file_path().map(|p| p.to_string_lossy().to_string()),
                    "messageCount": session.session().entries().len(),
                });
                output
                    .send_success(id, "get_state", Some(state_data))
                    .await?;
            }
            "set_model" => {
                let provider = val.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let model_id = val
                    .get("modelId")
                    .or_else(|| val.get("model_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let reg = match crate::models_config::ModelRegistry::load_default() {
                    Ok(r) => r,
                    Err(e) => {
                        output
                            .send_error(
                                id,
                                "set_model",
                                &format!("Failed to load models registry: {e}"),
                            )
                            .await?;
                        continue;
                    }
                };

                let model = match reg.resolve_exact(provider, model_id) {
                    Some(m) => m,
                    None => {
                        output
                            .send_error(
                                id,
                                "set_model",
                                &format!("Unknown model '{provider}/{model_id}'"),
                            )
                            .await?;
                        continue;
                    }
                };

                let resolved_key = if model.provider != "faux" && model.api != "faux" {
                    match crate::auth::resolve_api_key_async(&model.provider, None, None).await {
                        Ok(key) => key
                            .or_else(|| reg.provider_api_key(&model.provider).map(str::to_string)),
                        Err(e) => {
                            output
                                .send_error(
                                    id,
                                    "set_model",
                                    &format!("Failed to resolve provider credentials: {e}"),
                                )
                                .await?;
                            continue;
                        }
                    }
                } else {
                    None
                };

                let provider_changed =
                    !session.model.provider.eq_ignore_ascii_case(&model.provider);
                let prov = AgentSession::default_provider_for_model(&model);
                session.set_model(model.clone(), prov);
                if let Some(key) = resolved_key {
                    if provider_changed || !session.has_explicit_api_key() {
                        session.set_api_key(Some(key));
                    }
                }
                session.sync_thinking_for_model(true);
                output
                    .send_success(id, "set_model", Some(serde_json::to_value(&model)?))
                    .await?;
            }
            "set_thinking_level" => {
                let level_raw = val.get("level").filter(|v| !v.is_null());
                if let Some(lvl_json) = level_raw {
                    match serde_json::from_value::<ThinkingLevel>(lvl_json.clone()) {
                        Ok(lvl) => {
                            session.set_thinking_level(Some(lvl));
                            output.send_success(id, "set_thinking_level", None).await?;
                        }
                        Err(_) => {
                            output
                                .send_error(id, "set_thinking_level", "Invalid thinking level")
                                .await?;
                        }
                    }
                } else {
                    session.set_thinking_level(None);
                    output.send_success(id, "set_thinking_level", None).await?;
                }
            }
            "get_available_models" => match crate::models_config::ModelRegistry::load_default() {
                Ok(reg) => {
                    output
                        .send_success(
                            id,
                            "get_available_models",
                            Some(serde_json::json!({ "models": reg.models() })),
                        )
                        .await?;
                }
                Err(e) => {
                    output
                        .send_error(
                            id,
                            "get_available_models",
                            &format!("Failed to load models registry: {e}"),
                        )
                        .await?;
                }
            },
            "get_messages" => {
                let messages = session.session().build_context_messages();
                output
                    .send_success(
                        id,
                        "get_messages",
                        Some(serde_json::json!({ "messages": messages })),
                    )
                    .await?;
            }
            "get_entries" => {
                let entries = session.session().entries();
                let leaf_id = session.session().leaf_id();
                output
                    .send_success(
                        id,
                        "get_entries",
                        Some(serde_json::json!({ "entries": entries, "leafId": leaf_id })),
                    )
                    .await?;
            }
            "get_commands" => {
                let mut commands = Vec::new();
                for t in session.prompt_templates() {
                    commands.push(serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "source": "prompt",
                    }));
                }
                for s in session.skills() {
                    commands.push(serde_json::json!({
                        "name": format!("skill:{}", s.name),
                        "description": s.description,
                        "source": "skill",
                    }));
                }
                output
                    .send_success(
                        id,
                        "get_commands",
                        Some(serde_json::json!({ "commands": commands })),
                    )
                    .await?;
            }
            "compact" => {
                let custom_inst = val
                    .get("customInstructions")
                    .or_else(|| val.get("custom_instructions"))
                    .and_then(|v| v.as_str());

                let cancel_slot = session.active_cancel_slot();
                let sub_control = session.subagent_control();
                let steering_queue = session.steering_queue();
                let follow_queue = session.follow_up_queue();
                let bash_cancel_slot = session.active_bash_cancel.clone();
                let (compact_res, compact_aborted) = run_with_concurrent_commands(
                    session.compact(custom_inst),
                    &mut reader_fut,
                    &mut reader_done,
                    &mut line_rx,
                    &mut rx_closed,
                    &mut pending_lines,
                    &output,
                    &sub_control,
                    &steering_queue,
                    &follow_queue,
                    &cancel_slot,
                    &bash_cancel_slot,
                )
                .await?;

                match compact_res {
                    Ok(summary) => {
                        if compact_aborted {
                            output.send_error(id, "compact", "Operation aborted").await?;
                        } else {
                            output
                                .send_success(
                                    id,
                                    "compact",
                                    Some(serde_json::json!({ "summary": summary })),
                                )
                                .await?;
                        }
                    }
                    Err(e) => {
                        output.send_error(id, "compact", &e.to_string()).await?;
                    }
                }
            }
            "new_session" => match session.session_mut().new_session() {
                Ok(_) => {
                    session.clear_queues();
                    session.todo_state.write().await.clear();
                    output.send_success(id, "new_session", None).await?;
                }
                Err(e) => {
                    output.send_error(id, "new_session", &e.to_string()).await?;
                }
            },
            "abort" => {
                session.abort();
                output.send_success(id, "abort", None).await?;
            }
            "steer" => {
                let msg = val
                    .get("message")
                    .or_else(|| val.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if msg.trim().is_empty() {
                    output
                        .send_error(id, "steer", "Message must not be empty")
                        .await?;
                    continue;
                }
                session.steer(msg);
                output.send_success(id, "steer", None).await?;
            }
            "subagent_list" => {
                let agents: Vec<serde_json::Value> = session
                    .subagent_control()
                    .list()
                    .into_iter()
                    .map(|s| {
                        serde_json::json!({
                            "agent_id": s.agent_id,
                            "mode": s.mode,
                            "elapsed_ms": s.elapsed_ms,
                        })
                    })
                    .collect();
                output
                    .send_success(
                        id,
                        "subagent_list",
                        Some(serde_json::json!({ "agents": agents })),
                    )
                    .await?;
            }
            "subagent_steer" => {
                let agent_id = val.get("agent_id").and_then(|v| v.as_str());
                let message = val
                    .get("message")
                    .or_else(|| val.get("text"))
                    .and_then(|v| v.as_str());
                let ok = match (agent_id, message) {
                    (Some(id), Some(text)) if !text.trim().is_empty() => {
                        session.subagent_control().steer(id, text)
                    }
                    _ => false,
                };
                output
                    .send_success(id, "subagent_steer", Some(serde_json::json!({ "ok": ok })))
                    .await?;
            }
            "subagent_kill" => {
                let agent_id = val.get("agent_id").and_then(|v| v.as_str());
                let ok = match agent_id {
                    Some(id) => session.subagent_control().kill(id),
                    None => false,
                };
                output
                    .send_success(id, "subagent_kill", Some(serde_json::json!({ "ok": ok })))
                    .await?;
            }
            "follow_up" => {
                let msg = val
                    .get("message")
                    .or_else(|| val.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if msg.trim().is_empty() {
                    output
                        .send_error(id, "follow_up", "Message must not be empty")
                        .await?;
                    continue;
                }
                session.follow_up(msg);
                output.send_success(id, "follow_up", None).await?;
            }
            "clear_queue" => {
                session.clear_queues();
                output.send_success(id, "clear_queue", None).await?;
            }
            "cycle_model" => {
                let prev_model = session.model.clone();
                let prev_provider = session.provider.clone();
                let prev_api_key = session.api_key.clone();
                let prev_has_explicit_api_key = session.has_explicit_api_key();
                let prev_thinking_level = session.thinking_level;
                let prev_thinking_explicitly_off = session.thinking_explicitly_off;
                let prev_worker_api_key =
                    mehh_ai::lock_recover(&session.lead_worker).worker_api_key.clone();

                match session.cycle_model() {
                    Ok(Some(m)) => {
                        if let Err(e) = session.ensure_api_key(None).await {
                            session.set_model(prev_model, prev_provider);
                            mehh_ai::lock_recover(&session.lead_worker)
                                .set_worker_api_key(prev_worker_api_key);
                            if prev_has_explicit_api_key {
                                session.set_explicit_api_key(prev_api_key);
                            } else {
                                session.set_api_key(prev_api_key);
                            }
                            session.thinking_level = prev_thinking_level;
                            session.thinking_explicitly_off = prev_thinking_explicitly_off;
                            output
                                .send_error(
                                    id,
                                    "cycle_model",
                                    &format!("Failed to resolve provider credentials: {e}"),
                                )
                                .await?;
                        } else {
                            output
                                .send_success(id, "cycle_model", Some(serde_json::to_value(&m)?))
                                .await?;
                        }
                    }
                    Ok(None) => {
                        output.send_success(id, "cycle_model", None).await?;
                    }
                    Err(e) => {
                        output.send_error(id, "cycle_model", &e.to_string()).await?;
                    }
                }
            }
            "bash" => {
                let Some(bash_tool) = session.tools().iter().find(|t| t.name() == "bash").cloned()
                else {
                    output
                        .send_error(id, "bash", "bash tool is disabled for this session")
                        .await?;
                    continue;
                };
                let Some(command) = val
                    .get("command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                else {
                    output
                        .send_error(id, "bash", "Missing or empty 'command' argument")
                        .await?;
                    continue;
                };
                let cancel = CancellationToken::new();
                *mehh_ai::lock_recover(&session.active_bash_cancel) = Some(cancel.clone());

                let mut args = serde_json::Map::new();
                args.insert("command".to_string(), serde_json::json!(command));
                if let Some(timeout) = val.get("timeout").filter(|v| !v.is_null()) {
                    args.insert("timeout".to_string(), timeout.clone());
                }
                if let Some(timeout_ms) = val.get("timeoutMs").filter(|v| !v.is_null()) {
                    args.insert("timeoutMs".to_string(), timeout_ms.clone());
                }

                // The RPC route must go through the same governance chain as
                // agent tool calls: security denies, persisted rules and
                // approval requirements all apply here too.
                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id: "rpc_bash".to_string(),
                    tool_name: "bash".to_string(),
                    args: args.clone(),
                    is_read_only: Some(false),
                    cwd: session.cwd.clone(),
                    origin: None,
                };
                let decision = session
                    .inspection_manager
                    .inspect_and_authorize(&inspect_ctx, session.approval_handler.as_deref())
                    .await;
                if decision.block {
                    *mehh_ai::lock_recover(&session.active_bash_cancel) = None;
                    let reason = decision
                        .reason
                        .unwrap_or_else(|| "Tool blocked by security policy".to_string());
                    output.send_error(id, "bash", &reason).await?;
                } else {
                    // Keep reading commands while bash runs, otherwise
                    // `abort_bash` waits in the buffer until the command ends.
                    let cancel_slot = session.active_cancel_slot();
                    let sub_control = session.subagent_control();
                    let steering_queue = session.steering_queue();
                    let follow_queue = session.follow_up_queue();
                    let bash_cancel_slot = session.active_bash_cancel.clone();
                    let (res, _) = run_with_concurrent_commands(
                        bash_tool.execute("rpc_bash", args, cancel, None),
                        &mut reader_fut,
                        &mut reader_done,
                        &mut line_rx,
                        &mut rx_closed,
                        &mut pending_lines,
                        &output,
                        &sub_control,
                        &steering_queue,
                        &follow_queue,
                        &cancel_slot,
                        &bash_cancel_slot,
                    )
                    .await?;
                    *mehh_ai::lock_recover(&session.active_bash_cancel) = None;

                    match res {
                        Ok(tool_res) => {
                            let text = tool_res
                                .content
                                .iter()
                                .filter_map(|b| match b {
                                    mehh_ai::ToolResultContentBlock::Text(t) => {
                                        Some(t.text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            output
                                .send_success(
                                    id,
                                    "bash",
                                    Some(serde_json::json!({
                                        "output": text,
                                        "isError": tool_res.is_error.unwrap_or(false),
                                    })),
                                )
                                .await?;
                        }
                        Err(e) => {
                            if e.contains("Command aborted")
                                || e.contains("Missing required parameter")
                            {
                                output.send_error(id, "bash", &e).await?;
                            } else {
                                output
                                    .send_success(
                                        id,
                                        "bash",
                                        Some(serde_json::json!({
                                            "output": e,
                                            "isError": true,
                                        })),
                                    )
                                    .await?;
                            }
                        }
                    }
                }
            }
            "abort_bash" => {
                session.abort_bash();
                output.send_success(id, "abort_bash", None).await?;
            }
            "fork" => {
                let entry_id = val
                    .get("entryId")
                    .or_else(|| val.get("entry_id"))
                    .and_then(|v| v.as_str());
                match session.session().fork_at(entry_id) {
                    Ok(forked) => {
                        let new_id = forked.session_id().to_string();
                        session.session = forked;
                        output
                            .send_success(
                                id,
                                "fork",
                                Some(serde_json::json!({ "sessionId": new_id })),
                            )
                            .await?;
                    }
                    Err(e) => {
                        output.send_error(id, "fork", &e.to_string()).await?;
                    }
                }
            }
            "clone" => match session.session().clone_at_leaf() {
                Ok(cloned) => {
                    let new_id = cloned.session_id().to_string();
                    session.session = cloned;
                    output
                        .send_success(
                            id,
                            "clone",
                            Some(serde_json::json!({ "sessionId": new_id })),
                        )
                        .await?;
                }
                Err(e) => {
                    output.send_error(id, "clone", &e.to_string()).await?;
                }
            },
            "switch_session" => {
                let session_path = val
                    .get("sessionPath")
                    .or_else(|| val.get("session_path"))
                    .or_else(|| val.get("path"))
                    .and_then(|v| v.as_str());
                match session_path {
                    Some(p) => {
                        let path = PathBuf::from(p);
                        match SessionManager::open_existing_file(&path, session.cwd.clone()) {
                            Ok(opened) => {
                                let sid = opened.session_id().to_string();
                                session.session = opened;
                                session.clear_queues();
                                session.todo_state.write().await.clear();
                                output
                                    .send_success(
                                        id,
                                        "switch_session",
                                        Some(serde_json::json!({ "sessionId": sid })),
                                    )
                                    .await?;
                            }
                            Err(e) => {
                                output
                                    .send_error(id, "switch_session", &e.to_string())
                                    .await?;
                            }
                        }
                    }
                    None => {
                        output
                            .send_error(id, "switch_session", "Missing 'sessionPath' argument")
                            .await?;
                    }
                }
            }
            "get_tree" => {
                let tree = session.session().build_rpc_tree();
                let leaf_id = session.session().leaf_id();
                output
                    .send_success(
                        id,
                        "get_tree",
                        Some(serde_json::json!({
                            "tree": tree,
                            "leafId": leaf_id,
                        })),
                    )
                    .await?;
            }
            "set_session_name" => {
                let name = val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                match name {
                    Some(n) => match session.session_mut().set_session_name(Some(n.to_string())) {
                        Ok(()) => {
                            output.send_success(id, "set_session_name", None).await?;
                        }
                        Err(e) => {
                            output
                                .send_error(id, "set_session_name", &e.to_string())
                                .await?;
                        }
                    },
                    None => {
                        output
                            .send_error(id, "set_session_name", "Session name cannot be empty")
                            .await?;
                    }
                }
            }
            _ => {
                output
                    .send_error(id, cmd_type, &format!("Unsupported command: {cmd_type}"))
                    .await?;
            }
        }
    }

    Ok(())
}
