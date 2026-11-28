use anyhow::Result;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use mehh_agent::AgentEvent;
use mehh_ai::{now_ms, Message, UserContent, UserMessage};

use crate::subagent_control::SubagentControl;

/// Maximum number of non-concurrent RPC commands buffered while another command runs.
pub(super) const MAX_PENDING_LINES: usize = 256;

/// Maximum number of outbound `AgentEvent` messages buffered before backpressure drops overflow.
pub(super) const MAX_QUEUED_OUTBOUND_EVENTS: usize = 1024;

/// Serializes and sends JSONL responses and events to the writer.
pub(super) struct RpcOutput<W> {
    inner: Arc<Mutex<(W, Vec<u8>)>>,
}

impl<W> Clone for RpcOutput<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcOutput<W> {
    pub(super) fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new((writer, Vec::with_capacity(512)))),
        }
    }

    pub(super) async fn send_raw(&self, val: &serde_json::Value) -> Result<()> {
        let mut lock = self.inner.lock().await;
        let (writer, buf) = &mut *lock;
        buf.clear();
        serde_json::to_writer(&mut *buf, val)?;
        buf.push(b'\n');
        writer.write_all(buf).await?;
        writer.flush().await?;
        Ok(())
    }

    pub(super) async fn send_event(&self, event: &AgentEvent) -> Result<()> {
        let mut lock = self.inner.lock().await;
        let (writer, buf) = &mut *lock;
        buf.clear();
        write_rpc_event_json(buf, event)?;
        buf.push(b'\n');
        writer.write_all(buf).await?;
        writer.flush().await?;
        Ok(())
    }

    pub(super) async fn send_success(
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

    pub(super) async fn send_error(&self, id: Option<&str>, command: &str, error: &str) -> Result<()> {
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

#[derive(serde::Serialize)]
struct RpcMessageUpdateBorrowed<E> {
    #[serde(rename = "assistantMessageEvent")]
    assistant_message_event: E,
    #[serde(rename = "type")]
    r#type: &'static str,
}

#[derive(serde::Serialize)]
struct RpcStartBorrowed {
    #[serde(rename = "type")]
    r#type: &'static str,
}

#[derive(serde::Serialize)]
struct RpcIndexTypeBorrowed {
    #[serde(rename = "contentIndex")]
    content_index: usize,
    #[serde(rename = "type")]
    r#type: &'static str,
}

#[derive(serde::Serialize)]
struct RpcDeltaBorrowed<'a> {
    #[serde(rename = "contentIndex")]
    content_index: usize,
    delta: &'a str,
    #[serde(rename = "type")]
    r#type: &'static str,
}

#[derive(serde::Serialize)]
struct RpcEndBorrowed<'a> {
    content: &'a str,
    #[serde(rename = "contentIndex")]
    content_index: usize,
    #[serde(rename = "type")]
    r#type: &'static str,
}

/// Serializes an `AgentEvent` directly into `buf` for RPC JSONL output, bypassing
/// intermediate `serde_json::Value` allocations for hot streaming delta events while
/// matching `format_rpc_event(event)` byte-for-byte.
pub(super) fn write_rpc_event_json(buf: &mut Vec<u8>, event: &AgentEvent) -> serde_json::Result<()> {
    if let AgentEvent::MessageUpdate {
        assistant_message_event,
        ..
    } = event
    {
        use mehh_ai::AssistantMessageEvent;
        match assistant_message_event {
            AssistantMessageEvent::Start { .. } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcStartBorrowed { r#type: "start" },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::TextStart { content_index, .. } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcIndexTypeBorrowed {
                            content_index: *content_index,
                            r#type: "text_start",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcDeltaBorrowed {
                            content_index: *content_index,
                            delta,
                            r#type: "text_delta",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcEndBorrowed {
                            content,
                            content_index: *content_index,
                            r#type: "text_end",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcIndexTypeBorrowed {
                            content_index: *content_index,
                            r#type: "thinking_start",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcDeltaBorrowed {
                            content_index: *content_index,
                            delta,
                            r#type: "thinking_delta",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcEndBorrowed {
                            content,
                            content_index: *content_index,
                            r#type: "thinking_end",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcIndexTypeBorrowed {
                            content_index: *content_index,
                            r#type: "toolcall_start",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                return serde_json::to_writer(
                    buf,
                    &RpcMessageUpdateBorrowed {
                        assistant_message_event: RpcDeltaBorrowed {
                            content_index: *content_index,
                            delta,
                            r#type: "toolcall_delta",
                        },
                        r#type: "message_update",
                    },
                );
            }
            AssistantMessageEvent::ToolCallEnd { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {}
        }
    }
    serde_json::to_writer(buf, &format_rpc_event(event))
}

/// Converts an `AgentEvent` into a wire JSON object, stripping the cumulative
/// snapshots from `message_update` events to keep lines small. The deltas in
/// `assistantMessageEvent` and the final `MessageEnd` carry the same data.
pub fn format_rpc_event(event: &AgentEvent) -> serde_json::Value {
    if let AgentEvent::MessageUpdate {
        assistant_message_event,
        ..
    } = event
    {
        use mehh_ai::AssistantMessageEvent;
        let ame = match assistant_message_event {
            AssistantMessageEvent::Start { .. } => serde_json::json!({ "type": "start" }),
            AssistantMessageEvent::TextStart { content_index, .. } => serde_json::json!({
                "type": "text_start",
                "contentIndex": content_index,
            }),
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => serde_json::json!({
                "type": "text_delta",
                "contentIndex": content_index,
                "delta": delta,
            }),
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => serde_json::json!({
                "type": "text_end",
                "contentIndex": content_index,
                "content": content,
            }),
            AssistantMessageEvent::ThinkingStart { content_index, .. } => serde_json::json!({
                "type": "thinking_start",
                "contentIndex": content_index,
            }),
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => serde_json::json!({
                "type": "thinking_delta",
                "contentIndex": content_index,
                "delta": delta,
            }),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                ..
            } => serde_json::json!({
                "type": "thinking_end",
                "contentIndex": content_index,
                "content": content,
            }),
            AssistantMessageEvent::ToolCallStart { content_index, .. } => serde_json::json!({
                "type": "toolcall_start",
                "contentIndex": content_index,
            }),
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => serde_json::json!({
                "type": "toolcall_delta",
                "contentIndex": content_index,
                "delta": delta,
            }),
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => serde_json::json!({
                "type": "toolcall_end",
                "contentIndex": content_index,
                "toolCall": mehh_ai::tool_call_to_json(tool_call),
            }),
            AssistantMessageEvent::Done { reason, message } => serde_json::json!({
                "type": "done",
                "reason": reason,
                "message": mehh_ai::assistant_message_to_json(message),
            }),
            AssistantMessageEvent::Error { reason, error } => serde_json::json!({
                "type": "error",
                "reason": reason,
                "error": mehh_ai::assistant_message_to_json(error),
            }),
        };
        return serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": ame,
        });
    }
    serde_json::to_value(event).unwrap_or_else(|_| serde_json::json!({ "type": "error" }))
}

/// Commands that can be answered without borrowing `AgentSession` mutably.
/// Returns true when the command was handled; false means it must be queued
/// until the current prompt finishes.
pub(super) async fn handle_concurrent_command<W>(
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
            if let Some(tok) = mehh_ai::lock_recover(cancel_slot).as_ref() {
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

/// Polls an in-flight operation (`prompt` or `bash`) concurrently with incoming RPC commands
/// from the bounded stdin channel, handling concurrent commands immediately and buffering
/// sequential commands up to `MAX_PENDING_LINES`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_with_concurrent_commands<F, T, RF, W>(
    fut: F,
    reader_fut: &mut std::pin::Pin<&mut RF>,
    reader_done: &mut bool,
    line_rx: &mut tokio::sync::mpsc::Receiver<Result<String>>,
    rx_closed: &mut bool,
    pending_lines: &mut VecDeque<String>,
    output: &RpcOutput<W>,
    sub_control: &Arc<SubagentControl>,
    steering_queue: &Arc<StdMutex<VecDeque<Message>>>,
    follow_queue: &Arc<StdMutex<VecDeque<Message>>>,
    cancel_slot: &Arc<StdMutex<Option<CancellationToken>>>,
    bash_cancel_slot: &Arc<StdMutex<Option<CancellationToken>>>,
) -> Result<(T, bool)>
where
    F: std::future::Future<Output = T>,
    RF: std::future::Future<Output = ()>,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::pin!(fut);
    let mut aborted = false;
    loop {
        tokio::select! {
            res = &mut fut => return Ok((res, aborted)),
            _ = reader_fut.as_mut(), if !*reader_done => {
                *reader_done = true;
            }
            msg = line_rx.recv(), if !*rx_closed => {
                match msg {
                    None => {
                        *rx_closed = true;
                    }
                    Some(Err(e)) => return Err(e),
                    Some(Ok(raw)) => {
                        let conc_trimmed = raw.trim().to_string();
                        if conc_trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(cval) = serde_json::from_str::<serde_json::Value>(&conc_trimmed) {
                            if handle_concurrent_command(
                                &cval,
                                output,
                                sub_control,
                                steering_queue,
                                follow_queue,
                                cancel_slot,
                                bash_cancel_slot,
                            )
                            .await?
                            {
                                if cval.get("type").and_then(|v| v.as_str()) == Some("abort") {
                                    aborted = true;
                                }
                                continue;
                            }
                            if pending_lines.len() >= MAX_PENDING_LINES {
                                let cid = cval.get("id").and_then(|v| v.as_str());
                                let ctype = cval
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                output
                                    .send_error(cid, ctype, "Too many pending RPC commands")
                                    .await?;
                                continue;
                            }
                        } else if pending_lines.len() >= MAX_PENDING_LINES {
                            output
                                .send_error(None, "parse", "Too many pending RPC commands")
                                .await?;
                            continue;
                        }
                        pending_lines.push_back(conc_trimmed);
                    }
                }
            }
        }
    }
}
