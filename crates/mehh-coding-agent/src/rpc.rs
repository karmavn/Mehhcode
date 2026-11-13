//! Headless RPC protocol for embedding the coding agent over asynchronous stdin/stdout or duplex streams.

use std::sync::Arc;
use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use mehh_agent::AgentEvent;
use mehh_ai::{Model, ThinkingLevel};

use crate::agent_session::AgentSession;

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

/// Converts an `AgentEvent` into a wire JSON object, stripping the cumulative `partial`
/// snapshot from `message_update` events to keep lines small.
pub fn format_rpc_event(event: &AgentEvent) -> serde_json::Value {
    match serde_json::to_value(event) {
        Ok(mut val) => {
            if let Some(obj) = val.as_object_mut() {
                if obj.get("type").and_then(|v| v.as_str()) == Some("message_update") {
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

/// Runs the agent session in RPC mode, reading JSONL commands from `reader` and writing
/// streamed events and command responses to `writer`.
pub async fn run_rpc_mode<R, W>(
    session: &mut AgentSession,
    mut reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let output = RpcOutput::new(writer);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
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

                let prompt_res = session.prompt(message, Some(on_event)).await;
                // Allow event drain task to finish processing buffered events
                let _ = drain_handle.await;

                match prompt_res {
                    Ok(_) => {
                        output.send_success(id, "prompt", None).await?;
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
                output.send_success(id, "get_state", Some(state_data)).await?;
            }
            "set_model" => {
                let provider = val.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let model_id = val
                    .get("modelId")
                    .or_else(|| val.get("model_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let all_models = mehh_ai::builtin_models();
                let found = all_models.into_iter().find(|m| {
                    m.provider.eq_ignore_ascii_case(provider)
                        && m.id.eq_ignore_ascii_case(model_id)
                });

                let model = match found {
                    Some(m) => m,
                    None => Model {
                        id: model_id.to_string(),
                        name: model_id.to_string(),
                        api: match provider {
                            "anthropic" => "anthropic-messages".to_string(),
                            "google" | "gemini" => "google-generative-ai".to_string(),
                            "openai-responses" => "openai-responses".to_string(),
                            _ => "openai-completions".to_string(),
                        },
                        provider: provider.to_string(),
                        base_url: String::new(),
                        reasoning: false,
                        input: vec!["text".to_string()],
                        cost: Default::default(),
                        context_window: 128_000,
                        max_tokens: 4096,
                        headers: None,
                    },
                };

                let prov = AgentSession::default_provider_for_model(&model);
                session.set_model(model.clone(), prov);
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
            "get_available_models" => {
                let models = mehh_ai::builtin_models();
                output
                    .send_success(
                        id,
                        "get_available_models",
                        Some(serde_json::json!({ "models": models })),
                    )
                    .await?;
            }
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
                output.send_success(id, "abort", None).await?;
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
        now_ms, AssistantContentBlock, AssistantMessage, FauxProvider, ModelCost, StopReason,
        TextContent, Usage,
    };

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
        }
    }

    #[tokio::test]
    async fn test_run_rpc_mode_lifecycle() {
        let temp_dir =
            std::env::temp_dir().join(format!("mehh_rpc_test_{}", Uuid::now_v7()));
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
}
