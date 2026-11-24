//! Headless RPC protocol for embedding the coding agent over asynchronous stdin/stdout or duplex streams.

use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use mehh_agent::{AgentEvent, AgentTool};
use mehh_ai::{now_ms, Message, StopReason, ThinkingLevel, UserContent, UserMessage};

use crate::agent_session::AgentSession;
use crate::session::SessionManager;
use crate::subagent_control::SubagentControl;

/// Serializes and sends JSONL responses and events to the writer.
struct RpcOutput<W> {
    writer: Arc<Mutex<W>>,
}

impl<W> Clone for RpcOutput<W> {
    fn clone(&self) -> Self {
        Self {
            writer: Arc::clone(&self.writer),
        }
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcOutput<W> {
    fn new(writer: W) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    async fn send_raw(&self, val: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_string(val)?;
        line.push('\n');
        let mut lock = self.writer.lock().await;
        lock.write_all(line.as_bytes()).await?;
        lock.flush().await?;
        Ok(())
    }

    async fn send_success(
        &self,
        id: Option<&str>,
        command: &str,
        data: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut obj = serde_json::Map::new();
        if let Some(id_val) = id {
            obj.insert(
                "id".to_string(),
                serde_json::Value::String(id_val.to_string()),
            );
        }
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("response".to_string()),
        );
        obj.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
        obj.insert("success".to_string(), serde_json::Value::Bool(true));
        if let Some(d) = data {
            obj.insert("data".to_string(), d);
        }
        self.send_raw(&serde_json::Value::Object(obj)).await
    }

    async fn send_error(&self, id: Option<&str>, command: &str, error: &str) -> Result<()> {
        let mut obj = serde_json::Map::new();
        if let Some(id_val) = id {
            obj.insert(
                "id".to_string(),
                serde_json::Value::String(id_val.to_string()),
            );
        }
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("response".to_string()),
        );
        obj.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
        obj.insert("success".to_string(), serde_json::Value::Bool(false));
        obj.insert(
            "error".to_string(),
            serde_json::Value::String(error.to_string()),
        );
        self.send_raw(&serde_json::Value::Object(obj)).await
    }
}

/// Converts an `AgentEvent` into a wire JSON object, stripping the cumulative
/// snapshots from `message_update` events to keep lines small. The deltas in
/// `assistantMessageEvent` and the final `MessageEnd` carry the same data.
pub fn format_rpc_event(event: &AgentEvent) -> serde_json::Value {
    match serde_json::to_value(event) {
        Ok(mut val) => {
            if let Some(obj) = val.as_object_mut() {
                if obj.get("type").and_then(|v| v.as_str()) == Some("message_update") {
                    obj.remove("message");
                    let key = if obj.contains_key("assistantMessageEvent") {
                        Some("assistantMessageEvent")
                    } else if obj.contains_key("assistant_message_event") {
                        Some("assistant_message_event")
                    } else {
                        None
                    };
                    if let Some(key) = key {
                        if let Some(ame) = obj.get_mut(key) {
                            if let Some(ame_obj) = ame.as_object_mut() {
                                ame_obj.remove("partial");
                            }
                        }
                    }
                }
            }
            val
        }
        Err(_) => serde_json::json!({ "type": "error" }),
    }
}

/// Commands that can be answered without borrowing `AgentSession` mutably.
/// Returns true when the command was handled; false means it must be queued
/// until the current prompt finishes.
async fn handle_concurrent_command<W>(
    val: &serde_json::Value,
    output: &RpcOutput<W>,
    sub_control: &Arc<SubagentControl>,
    steering_queue: &Arc<StdMutex<VecDeque<Message>>>,
    follow_queue: &Arc<StdMutex<VecDeque<Message>>>,
    cancel_slot: &Arc<StdMutex<Option<CancellationToken>>>,
    bash_cancel_slot: &Arc<StdMutex<Option<CancellationToken>>>,
) -> Result<bool>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let id = val.get("id").and_then(|v| v.as_str());
    let cmd = match val.get("type").and_then(|v| v.as_str()) {
        Some(cmd) => cmd,
        None => return Ok(false),
    };
    let text = || {
        val.get("message")
            .or_else(|| val.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    };

    match cmd {
        "abort" => {
            if let Some(tok) = mehh_ai::lock_recover(cancel_slot).take() {
                tok.cancel();
            }
            output.send_success(id, "abort", None).await?;
        }
        "abort_bash" => {
            if let Some(tok) = mehh_ai::lock_recover(bash_cancel_slot).take() {
                tok.cancel();
            }
            output.send_success(id, "abort_bash", None).await?;
        }
        "steer" | "follow_up" => {
            let msg = text();
            if msg.trim().is_empty() {
                output.send_error(id, cmd, "Message must not be empty").await?;
                return Ok(true);
            }
            let queue = if cmd == "steer" {
                steering_queue
            } else {
                follow_queue
            };
            mehh_ai::lock_recover(queue).push_back(Message::User(UserMessage {
                content: UserContent::Text(msg.to_string()),
                timestamp: now_ms(),
            }));
            output.send_success(id, cmd, None).await?;
        }
        "subagent_list" => {
            let agents: Vec<serde_json::Value> = sub_control
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
                .send_success(id, "subagent_list", Some(serde_json::json!({ "agents": agents })))
                .await?;
        }
        "subagent_steer" => {
            let ok = match (val.get("agent_id").and_then(|v| v.as_str()), text()) {
                (Some(agent_id), msg) if !msg.trim().is_empty() => sub_control.steer(agent_id, msg),
                _ => false,
            };
            output
                .send_success(id, "subagent_steer", Some(serde_json::json!({ "ok": ok })))
                .await?;
        }
        "subagent_kill" => {
            let ok = val
                .get("agent_id")
                .and_then(|v| v.as_str())
                .is_some_and(|agent_id| sub_control.kill(agent_id));
            output
                .send_success(id, "subagent_kill", Some(serde_json::json!({ "ok": ok })))
                .await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Runs the agent session in RPC mode, reading JSONL commands from `reader` and writing
/// streamed events and command responses to `writer`.
pub async fn run_rpc_mode<R, W>(session: &mut AgentSession, mut reader: R, writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let output = RpcOutput::new(writer);
    let mut line = String::new();
    let mut pending_lines: VecDeque<String> = VecDeque::new();

    loop {
        let raw_line = if let Some(queued) = pending_lines.pop_front() {
            queued
        } else {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                break;
            }
            line.clone()
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
                let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
                let out_clone = output.clone();

                let drain_handle = tokio::spawn(async move {
                    while let Some(ev) = event_rx.recv().await {
                        let json_val = format_rpc_event(&ev);
                        let _ = out_clone.send_raw(&json_val).await;
                    }
                });

                let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
                    let _ = event_tx.send(ev);
                });

                let cancel_slot = session.active_cancel_slot();
                let sub_control = session.subagent_control();
                let steering_queue = session.steering_queue();
                let follow_queue = session.follow_up_queue();
                let bash_cancel_slot = session.active_bash_cancel.clone();
                let prompt_res = {
                    let prompt_fut = session.prompt(message, Some(on_event));
                    tokio::pin!(prompt_fut);
                    let mut concurrent_line = String::new();
                    let mut stdin_eof = false;

                    loop {
                        tokio::select! {
                            res = &mut prompt_fut => break res,
                            read_res = reader.read_line(&mut concurrent_line), if !stdin_eof => {
                                match read_res {
                                    Ok(0) => {
                                        stdin_eof = true;
                                    }
                                    Ok(_) => {
                                        let conc_trimmed = concurrent_line.trim().to_string();
                                        concurrent_line.clear();
                                        if conc_trimmed.is_empty() {
                                            continue;
                                        }
                                        if let Ok(cval) = serde_json::from_str::<serde_json::Value>(&conc_trimmed) {
                                            if handle_concurrent_command(
                                                &cval,
                                                &output,
                                                &sub_control,
                                                &steering_queue,
                                                &follow_queue,
                                                &cancel_slot,
                                                &bash_cancel_slot,
                                            )
                                            .await?
                                            {
                                                continue;
                                            }
                                        }
                                        pending_lines.push_back(conc_trimmed);
                                    }
                                    Err(e) => return Err(e.into()),
                                }
                            }
                        }
                    }
                };

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
                    "thinkingLevel": session.thinking_level(),
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
                output
                    .send_success(id, "set_model", Some(serde_json::to_value(&model)?))
                    .await?;
            }
            "set_thinking_level" => {
                let level_raw = val.get("level");
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

                match session.compact(custom_inst).await {
                    Ok(summary) => {
                        output
                            .send_success(
                                id,
                                "compact",
                                Some(serde_json::json!({ "summary": summary })),
                            )
                            .await?;
                    }
                    Err(e) => {
                        output.send_error(id, "compact", &e.to_string()).await?;
                    }
                }
            }
            "new_session" => match session.session_mut().new_session() {
                Ok(_) => {
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
            "cycle_model" => match session.cycle_model() {
                Ok(Some(m)) => {
                    let _ = session.ensure_api_key(None).await;
                    output
                        .send_success(id, "cycle_model", Some(serde_json::to_value(&m)?))
                        .await?;
                }
                Ok(None) => {
                    output.send_success(id, "cycle_model", None).await?;
                }
                Err(e) => {
                    output.send_error(id, "cycle_model", &e.to_string()).await?;
                }
            },
            "bash" => {
                let command = val.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let cancel = CancellationToken::new();
                *mehh_ai::lock_recover(&session.active_bash_cancel) = Some(cancel.clone());

                // The RPC route must go through the same governance chain as
                // agent tool calls: security denies, persisted rules and
                // approval requirements all apply here too.
                let mut inspect_args = serde_json::Map::new();
                inspect_args.insert("command".to_string(), serde_json::json!(command));
                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id: "rpc_bash".to_string(),
                    tool_name: "bash".to_string(),
                    args: inspect_args,
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
                    let bash_tool = mehh_tools::BashTool::new(session.cwd.clone());
                    let args = [("command".to_string(), serde_json::json!(command))]
                        .into_iter()
                        .collect();

                    // Keep reading commands while bash runs, otherwise
                    // `abort_bash` waits in the buffer until the command ends.
                    let cancel_slot = session.active_cancel_slot();
                    let sub_control = session.subagent_control();
                    let steering_queue = session.steering_queue();
                    let follow_queue = session.follow_up_queue();
                    let bash_cancel_slot = session.active_bash_cancel.clone();
                    let exec_fut = bash_tool.execute("rpc_bash", args, cancel, None);
                    tokio::pin!(exec_fut);
                    let mut concurrent_line = String::new();
                    let mut stdin_eof = false;
                    let res = loop {
                        tokio::select! {
                            res = &mut exec_fut => break res,
                            read_res = reader.read_line(&mut concurrent_line), if !stdin_eof => {
                                match read_res {
                                    Ok(0) => stdin_eof = true,
                                    Ok(_) => {
                                        let conc_trimmed = concurrent_line.trim().to_string();
                                        concurrent_line.clear();
                                        if conc_trimmed.is_empty() {
                                            continue;
                                        }
                                        if let Ok(cval) = serde_json::from_str::<serde_json::Value>(&conc_trimmed) {
                                            if handle_concurrent_command(
                                                &cval,
                                                &output,
                                                &sub_control,
                                                &steering_queue,
                                                &follow_queue,
                                                &cancel_slot,
                                                &bash_cancel_slot,
                                            )
                                            .await?
                                            {
                                                continue;
                                            }
                                        }
                                        pending_lines.push_back(conc_trimmed);
                                    }
                                    Err(e) => return Err(e.into()),
                                }
                            }
                        }
                    };
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
                        match SessionManager::open_file(&path, session.cwd.clone()) {
                            Ok(opened) => {
                                let sid = opened.session_id().to_string();
                                session.session = opened;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use uuid::Uuid;

    use mehh_ai::{
        create_event_stream, now_ms, AssistantContentBlock, AssistantEventStream,
        AssistantMessage, AssistantMessageEvent, Context, FauxProvider, LlmProvider, Model,
        ModelCost, StopReason, StreamOptions, TextContent, Usage,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Model provider that blocks until the test releases it, so commands can
    /// be sent while a prompt is demonstrably still in flight.
    struct GatedProvider {
        started: tokio::sync::watch::Sender<bool>,
        release: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for GatedProvider {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            _options: &StreamOptions,
            cancel: CancellationToken,
        ) -> AssistantEventStream {
            let (sender, stream) = create_event_stream();
            let started = self.started.clone();
            let release = self.release.clone();
            let model = model.clone();
            tokio::spawn(async move {
                let _ = started.send(true);
                while !release.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                    if cancel.is_cancelled() {
                        return;
                    }
                }
                let msg = AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "done".into(),
                        text_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                };
                sender.push(AssistantMessageEvent::Start {
                    partial: msg.clone(),
                });
                sender.push(AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: msg,
                });
            });
            stream
        }
    }

    use crate::session::SessionManager;
    use crate::settings::Settings;

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

    #[tokio::test]
    async fn test_run_rpc_mode_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_test_{}", Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let faux = FauxProvider::new();
        // Response for prompt
        faux.add_response(AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "Hello from RPC".to_string(),
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
        // Response for compaction
        faux.add_response(AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "## Goal\nTest compaction via RPC".to_string(),
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

        let session_mgr = SessionManager::in_memory(temp_dir.clone());
        let mut session = AgentSession::new(
            temp_dir.clone(),
            make_test_model(),
            Arc::new(faux),
            session_mgr,
            Settings::default(),
        );

        let (client_write, server_read) = tokio::io::duplex(8192);
        let (server_write, client_read) = tokio::io::duplex(8192);

        let mut client_writer = client_write;
        let mut client_reader = BufReader::new(client_read);

        // Spawn RPC runner
        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
        });

        // 1. Send get_state
        let cmd_get_state = serde_json::json!({
            "id": "req_1",
            "type": "get_state"
        });
        client_writer
            .write_all(format!("{cmd_get_state}\n").as_bytes())
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        let mut resp_line = String::new();
        client_reader.read_line(&mut resp_line).await.unwrap();
        let resp_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp_json["id"], "req_1");
        assert_eq!(resp_json["command"], "get_state");
        assert_eq!(resp_json["success"], true);

        // 2. Send prompt
        let cmd_prompt = serde_json::json!({
            "id": "req_2",
            "type": "prompt",
            "message": "Hello"
        });
        client_writer
            .write_all(format!("{cmd_prompt}\n").as_bytes())
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        // Read until prompt response is received
        let mut prompt_succeeded = false;
        loop {
            resp_line.clear();
            let n = client_reader.read_line(&mut resp_line).await.unwrap();
            if n == 0 {
                break;
            }
            let ev: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
            if ev.get("type").and_then(|v| v.as_str()) == Some("response")
                && ev.get("command").and_then(|v| v.as_str()) == Some("prompt")
            {
                assert_eq!(ev["id"], "req_2");
                assert_eq!(ev["success"], true);
                prompt_succeeded = true;
                break;
            }
        }
        assert!(prompt_succeeded);

        // 3. Send get_messages
        let cmd_get_messages = serde_json::json!({
            "id": "req_3",
            "type": "get_messages"
        });
        client_writer
            .write_all(format!("{cmd_get_messages}\n").as_bytes())
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        resp_line.clear();
        client_reader.read_line(&mut resp_line).await.unwrap();
        let msgs_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(msgs_json["id"], "req_3");
        assert_eq!(msgs_json["command"], "get_messages");
        assert_eq!(msgs_json["success"], true);
        let msgs = msgs_json["data"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2); // User + Assistant

        // 4. Send compact
        let cmd_compact = serde_json::json!({
            "id": "req_4",
            "type": "compact"
        });
        client_writer
            .write_all(format!("{cmd_compact}\n").as_bytes())
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        resp_line.clear();
        client_reader.read_line(&mut resp_line).await.unwrap();
        let comp_json: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(comp_json["id"], "req_4");
        assert_eq!(comp_json["command"], "compact");
        assert_eq!(comp_json["success"], true);

        // Close client writer to shut down RPC cleanly
        drop(client_writer);
        let res = rpc_handle.await.unwrap();
        assert!(res.is_ok());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn subagent_kill_without_agent_id_reports_false() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_kill_{}", Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
        let release = Arc::new(AtomicBool::new(false));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            make_test_model(),
            Arc::new(GatedProvider {
                started: started_tx,
                release: release.clone(),
            }),
            SessionManager::in_memory(temp_dir.clone()),
            Settings::default(),
        );

        let (client_write, server_read) = tokio::io::duplex(8192);
        let (server_write, client_read) = tokio::io::duplex(8192);
        let mut client_writer = client_write;
        let mut client_reader = BufReader::new(client_read);
        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
        });

        client_writer
            .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
            .await
            .unwrap();
        client_writer.flush().await.unwrap();
        while !*started_rx.borrow() {
            started_rx.changed().await.unwrap();
        }

        client_writer
            .write_all(b"{\"id\":\"k1\",\"type\":\"subagent_kill\"}\n")
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        let answered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut line = String::new();
                if client_reader.read_line(&mut line).await.unwrap() == 0 {
                    return None;
                }
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if value["id"] == "k1" {
                    return Some(value);
                }
            }
        })
        .await
        .expect("subagent_kill must be answered")
        .expect("RPC stream closed unexpectedly");
        assert_eq!(
            answered["data"]["ok"], false,
            "a kill request without agent_id must not report success"
        );

        release.store(true, Ordering::SeqCst);
        drop(client_writer);
        let _ = rpc_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn subagent_list_responds_while_prompt_is_still_running() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_busy_{}", Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
        let release = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(GatedProvider {
            started: started_tx,
            release: release.clone(),
        });
        let session_mgr = SessionManager::in_memory(temp_dir.clone());
        let mut session = AgentSession::new(
            temp_dir.clone(),
            make_test_model(),
            provider,
            session_mgr,
            Settings::default(),
        );

        let (client_write, server_read) = tokio::io::duplex(8192);
        let (server_write, client_read) = tokio::io::duplex(8192);
        let mut client_writer = client_write;
        let mut client_reader = BufReader::new(client_read);
        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
        });

        client_writer
            .write_all(b"{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"slow\"}\n")
            .await
            .unwrap();
        client_writer.flush().await.unwrap();
        while !*started_rx.borrow() {
            started_rx.changed().await.unwrap();
        }

        client_writer
            .write_all(b"{\"id\":\"q1\",\"type\":\"subagent_list\"}\n")
            .await
            .unwrap();
        client_writer.flush().await.unwrap();

        let answered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut line = String::new();
                if client_reader.read_line(&mut line).await.unwrap() == 0 {
                    return None;
                }
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if value["id"] == "q1" {
                    return Some(value);
                }
            }
        })
        .await
        .expect("subagent_list must be answered while the prompt is in flight")
        .expect("RPC stream closed unexpectedly");
        assert_eq!(answered["command"], "subagent_list");
        assert_eq!(answered["success"], true);

        release.store(true, Ordering::SeqCst);
        drop(client_writer);
        let _ = rpc_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_rpc_rejects_unknown_model_and_reports_provider_error() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_rpc_err_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let faux = Arc::new(FauxProvider::with_responses(vec![
            mehh_ai::AssistantMessage {
                content: vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                    text: String::new(),
                    text_signature: None,
                })],
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: mehh_ai::Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("upstream rpc error".to_string()),
                timestamp: mehh_ai::now_ms(),
            },
        ]));

        let mut session = AgentSession::new(
            temp_dir.clone(),
            make_test_model(),
            faux,
            crate::session::SessionManager::in_memory(&temp_dir),
            crate::settings::Settings::default(),
        );

        let (client_write, server_read) = tokio::io::duplex(8192);
        let (server_write, client_read) = tokio::io::duplex(8192);

        let mut client_write = client_write;
        let mut reader = tokio::io::BufReader::new(client_read);

        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(
                &mut session,
                tokio::io::BufReader::new(server_read),
                server_write,
            )
            .await
        });

        let mut line = String::new();

        // 1. Unknown model fails with success: false
        let bad_model = serde_json::json!({
            "id": "m1",
            "type": "set_model",
            "provider": "openai",
            "modelId": "nonexistent-model-xyz"
        });
        client_write
            .write_all(format!("{bad_model}\n").as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let m_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(m_resp["id"], "m1");
        assert_eq!(m_resp["success"], false);

        // 2. Prompt returning StopReason::Error fails with success: false
        let bad_prompt = serde_json::json!({
            "id": "p1",
            "type": "prompt",
            "message": "trigger failure"
        });
        client_write
            .write_all(format!("{bad_prompt}\n").as_bytes())
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        let mut prompt_err_seen = false;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap();
            if n == 0 {
                break;
            }
            let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
            if ev.get("type").and_then(|v| v.as_str()) == Some("response")
                && ev.get("command").and_then(|v| v.as_str()) == Some("prompt")
            {
                assert_eq!(ev["id"], "p1");
                assert_eq!(ev["success"], false);
                assert!(ev["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("upstream rpc error"));
                prompt_err_seen = true;
                break;
            }
        }
        assert!(prompt_err_seen);

        drop(client_write);
        assert!(rpc_handle.await.unwrap().is_ok());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn rpc_queue_session_tree_and_bash_commands() {
        let temp_dir =
            std::env::temp_dir().join(format!("mehh_rpc_commands_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let mut session = AgentSession::new(
            temp_dir.clone(),
            mehh_ai::faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );
        // This test asserts RPC plumbing, not the global permission store. Other
        // tests mutate USERPROFILE/HOME in parallel, so make the governance
        // chain hermetic here instead of inheriting a possible deny-all store.
        session.inspection_manager = Arc::new(
            crate::tool_inspection::ToolInspectionManager::new_with_store(
                crate::tool_inspection::PermissionMode::Auto,
                temp_dir.clone(),
                crate::permissions::PermissionStore::new(Vec::new(), None),
            ),
        );

        let (client_write, server_read) = tokio::io::duplex(64 * 1024);
        let (server_write, client_read) = tokio::io::duplex(64 * 1024);

        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
        });

        let mut writer = client_write;
        let mut reader = BufReader::new(client_read);

        async fn send_and_expect(
            writer: &mut tokio::io::DuplexStream,
            reader: &mut BufReader<tokio::io::DuplexStream>,
            cmd: serde_json::Value,
            expected_cmd: &str,
        ) -> serde_json::Value {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            writer
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .unwrap();
            writer.flush().await.unwrap();

            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.unwrap();
                assert!(n > 0, "EOF before receiving response for {expected_cmd}");
                let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
                if ev.get("type").and_then(|v| v.as_str()) == Some("response")
                    && ev.get("command").and_then(|v| v.as_str()) == Some(expected_cmd)
                {
                    assert_eq!(
                        ev["success"], true,
                        "Expected success for {expected_cmd}: {:?}",
                        ev
                    );
                    return ev;
                }
            }
        }

        // 1. steer
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "1", "type": "steer", "message": "steering msg" }),
            "steer",
        )
        .await;

        // 2. follow_up
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "2", "type": "follow_up", "message": "follow up msg" }),
            "follow_up",
        )
        .await;

        // 3. clear_queue
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "3", "type": "clear_queue" }),
            "clear_queue",
        )
        .await;

        // 4. cycle_model
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "4", "type": "cycle_model" }),
            "cycle_model",
        )
        .await;

        // 5. set_session_name
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "5", "type": "set_session_name", "name": "RPC Named Session" }),
            "set_session_name",
        ).await;

        // 6. get_tree
        let tree_resp = send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "6", "type": "get_tree" }),
            "get_tree",
        )
        .await;
        assert!(tree_resp["data"]["tree"].is_array());

        // 7. clone
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "7", "type": "clone" }),
            "clone",
        )
        .await;

        // 8. fork
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "8", "type": "fork" }),
            "fork",
        )
        .await;

        // 9. bash
        let echo_cmd = if cfg!(windows) {
            "Write-Output 'hello_rpc'"
        } else {
            "echo 'hello_rpc'"
        };
        let bash_resp = send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "9", "type": "bash", "command": echo_cmd }),
            "bash",
        )
        .await;
        assert!(bash_resp["data"]["output"]
            .as_str()
            .unwrap()
            .contains("hello_rpc"));

        // 9b. bash non-zero exit code resolves with isError: true instead of RPC error
        let fail_cmd = "exit 1";
        let fail_resp = send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "9b", "type": "bash", "command": fail_cmd }),
            "bash",
        )
        .await;
        assert_eq!(fail_resp["data"]["isError"], true);
        assert!(fail_resp["data"]["output"]
            .as_str()
            .unwrap()
            .contains("Command exited with code 1"));

        // 10. abort_bash
        send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "10", "type": "abort_bash" }),
            "abort_bash",
        )
        .await;

        // 11. switch_session
        let session_file = temp_dir.join("switched_session.jsonl");
        let sw_mgr = SessionManager::open_file(&session_file, &temp_dir).unwrap();
        let switch_resp = send_and_expect(
            &mut writer,
            &mut reader,
            serde_json::json!({ "id": "11", "type": "switch_session", "sessionPath": session_file.to_string_lossy() }),
            "switch_session",
        ).await;
        assert_eq!(switch_resp["data"]["sessionId"], sw_mgr.session_id());

        drop(writer);
        assert!(rpc_handle.await.unwrap().is_ok());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn rpc_bash_obeys_tool_inspection_deny() {
        let temp_dir =
            std::env::temp_dir().join(format!("mehh_rpc_bash_deny_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let mut session = AgentSession::new(
            temp_dir.clone(),
            mehh_ai::faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings {
                permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
                ..Settings::default()
            },
        );

        let (client_write, server_read) = tokio::io::duplex(64 * 1024);
        let (server_write, client_read) = tokio::io::duplex(64 * 1024);
        let rpc_handle = tokio::spawn(async move {
            run_rpc_mode(&mut session, BufReader::new(server_read), server_write).await
        });

        let mut writer = client_write;
        let mut reader = BufReader::new(client_read);
        writer
            .write_all(b"{\"id\":\"deny_1\",\"type\":\"bash\",\"command\":\"rm -rf /\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = None;
        for _ in 0..8 {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "EOF before bash response");
            let ev: serde_json::Value = serde_json::from_str(&line).unwrap();
            if ev.get("type").and_then(|v| v.as_str()) == Some("response")
                && ev.get("command").and_then(|v| v.as_str()) == Some("bash")
                && ev.get("id").and_then(|v| v.as_str()) == Some("deny_1")
            {
                response = Some(ev);
                break;
            }
        }
        let response = response.expect("bash response for deny_1");
        assert_eq!(
            response["success"], false,
            "dangerous RPC bash must be blocked: {response}"
        );
        let error = response["error"].as_str().unwrap_or("");
        assert!(
            error.contains("security policy"),
            "unexpected block reason: {error}"
        );

        drop(writer);
        assert!(rpc_handle.await.unwrap().is_ok());
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
