//! Mistral Conversations API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::oauth::redact_error_body_with_limit;
use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry_with_secrets, RetryPolicy,
    DEFAULT_STREAM_CHUNK_TIMEOUT,
};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::types::*;

/// Provider for Mistral Conversations API.
pub struct MistralProvider {
    client: reqwest::Client,
}

impl MistralProvider {
    /// Creates a new Mistral provider with default HTTP client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new Mistral provider with the given HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for MistralProvider {
    fn default() -> Self {
        Self {
            client: default_http_client(),
        }
    }
}

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;

fn derive_mistral_tool_call_id(id: &str, attempt: usize) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if !normalized.is_empty() {
        normalized.as_str()
    } else {
        id
    };
    let seed = if attempt == 0 {
        seed_base.to_string()
    } else {
        format!("{seed_base}:{attempt}")
    };
    let hash = crate::transform_messages::short_hash(&seed);
    let filtered: String = hash.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if filtered.len() >= MISTRAL_TOOL_CALL_ID_LENGTH {
        filtered[..MISTRAL_TOOL_CALL_ID_LENGTH].to_string()
    } else {
        format!("{:0<9}", filtered)
    }
}

#[derive(Debug, Default, Clone)]
pub struct MistralToolCallIdNormalizer {
    id_map: HashMap<String, String>,
    reverse_map: HashMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }

        let mut attempt = 0;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            let owner = self.reverse_map.get(&candidate);
            match owner {
                None => {
                    self.id_map.insert(id.to_string(), candidate.clone());
                    self.reverse_map.insert(candidate.clone(), id.to_string());
                    return candidate;
                }
                Some(owner_id) if owner_id == id => {
                    self.id_map.insert(id.to_string(), candidate.clone());
                    return candidate;
                }
                _ => {
                    attempt += 1;
                }
            }
        }
    }
}

/// Normalizes a tool call ID to Mistral's requirement of exactly 9 ASCII alphanumeric characters.
pub fn normalize_mistral_tool_call_id(id: &str) -> String {
    let mut normalizer = MistralToolCallIdNormalizer::new();
    normalizer.normalize(id)
}

/// Builds the JSON request payload for Mistral Chat Completions API.
pub fn build_mistral_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::transform_messages;

    let mut normalizer = MistralToolCallIdNormalizer::new();
    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, _m: &Model, _a: &AssistantMessage| normalizer.normalize(id)),
    );

    let mut messages: Vec<serde_json::Value> = Vec::new();

    // System prompt
    let mut system_text = String::new();
    let mut push_sys = |s: &str| {
        if !s.is_empty() {
            if !system_text.is_empty() {
                system_text.push_str("\n\n");
            }
            system_text.push_str(&crate::unicode::sanitize_surrogates_cow(s));
        }
    };
    if let Some(sys) = &context.system_prompt {
        push_sys(sys);
    }
    for msg in &transformed_messages {
        if let Message::System(sys) = msg {
            push_sys(&sys.content);
        }
    }
    if !system_text.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system_text,
        }));
    }

    for msg in &transformed_messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let content_val = match &u.content {
                    UserContent::Text(text) => {
                        serde_json::json!(crate::unicode::sanitize_surrogates_cow(text))
                    }
                    UserContent::Blocks(blocks) => {
                        let chunks: Vec<serde_json::Value> = blocks
                            .iter()
                            .map(|b| match b {
                                UserContentBlock::Text(t) => {
                                    serde_json::json!({
                                        "type": "text",
                                        "text": crate::unicode::sanitize_surrogates_cow(&t.text),
                                    })
                                }
                                UserContentBlock::Image(img) => {
                                    serde_json::json!({
                                        "type": "image_url",
                                        "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                                    })
                                }
                            })
                            .collect();
                        serde_json::Value::Array(chunks)
                    }
                };
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content_val,
                }));
            }
            Message::Assistant(a) => {
                let mut asst_map = serde_json::Map::new();
                asst_map.insert("role".to_string(), serde_json::json!("assistant"));

                let text_content = a.text_content();
                if !text_content.is_empty() {
                    asst_map.insert(
                        "content".to_string(),
                        serde_json::json!(crate::unicode::sanitize_surrogates_cow(&text_content)),
                    );
                }

                let tool_calls = a.tool_calls();
                if !tool_calls.is_empty() {
                    let tc_json: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            let args_str = serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": args_str,
                                }
                            })
                        })
                        .collect();
                    asst_map.insert("tool_calls".to_string(), serde_json::Value::Array(tc_json));
                }

                // Mistral rejects an assistant message with neither content nor tool_calls.
                if !asst_map.contains_key("content") && !asst_map.contains_key("tool_calls") {
                    asst_map.insert("content".to_string(), serde_json::json!(""));
                }

                messages.push(serde_json::Value::Object(asst_map));
            }
            Message::ToolResult(tr) => {
                let mut text = String::new();
                let mut has_text = false;
                for b in &tr.content {
                    if let ToolResultContentBlock::Text(tc) = b {
                        if has_text {
                            text.push('\n');
                        }
                        text.push_str(&crate::unicode::sanitize_surrogates_cow(&tc.text));
                        has_text = true;
                    }
                }

                let images: Vec<serde_json::Value> = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ToolResultContentBlock::Image(img) => Some(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{};base64,{}", img.mime_type, img.data) }
                        })),
                        _ => None,
                    })
                    .collect();
                let content = if images.is_empty() {
                    serde_json::Value::String(text)
                } else {
                    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
                    parts.extend(images);
                    serde_json::Value::Array(parts)
                };

                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "name": tr.tool_name,
                    "content": content,
                }));
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("model".to_string(), serde_json::json!(model.id));
    req_map.insert("stream".to_string(), serde_json::json!(true));
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let decls: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            req_map.insert("tools".to_string(), serde_json::Value::Array(decls));
        }
    }

    if let Some(temp) = options.temperature {
        req_map.insert("temperature".to_string(), serde_json::json!(temp));
    }
    if let Some(max_tokens) = options.max_tokens {
        req_map.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
    }

    if model.reasoning && options.reasoning != Some(ThinkingLevel::Off) {
        req_map.insert("prompt_mode".to_string(), serde_json::json!("reasoning"));
    }

    serde_json::Value::Object(req_map)
}

fn insert_custom_headers(
    header_map: &mut reqwest::header::HeaderMap,
    headers: Option<&HashMap<String, String>>,
) -> Result<(), String> {
    if let Some(map) = headers {
        for (k, v) in map {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("Invalid header name '{k}': {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| format!("Invalid header value for '{k}': {e}"))?;
            header_map.insert(name, value);
        }
    }
    Ok(())
}

fn mistral_chat_completions_url(base_url: &str) -> String {
    const DEFAULT_BASE: &str = "https://api.mistral.ai/v1";
    let path = crate::http_safety::normalized_endpoint_path(base_url, DEFAULT_BASE);
    let extra: &[&str] = if path.ends_with("/chat/completions") {
        &[]
    } else {
        &["chat", "completions"]
    };
    crate::http_safety::append_endpoint_segments_preserving_query(base_url, DEFAULT_BASE, extra)
}

#[async_trait::async_trait]
impl LlmProvider for MistralProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AssistantEventStream {
        let (sender, stream) = create_event_stream();
        let client = self.client.clone();
        let model = model.clone();
        let options = options.clone();
        let body_res = serde_json::to_vec(&build_mistral_request(&model, context, &options))
            .map_err(|e| format!("Failed to serialize request body: {e}"));

        tokio::spawn(async move {
            let body_bytes = match body_res {
                Ok(b) => b,
                Err(msg) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(msg);
                    let _ = sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };
            let url = mistral_chat_completions_url(&model.base_url);

            let api_key = options
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let secrets_owned: Vec<String> = api_key.clone().into_iter().collect();
            let secret_refs: Vec<&str> = secrets_owned.iter().map(String::as_str).collect();

            let mut header_map = reqwest::header::HeaderMap::new();
            header_map.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            header_map.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("text/event-stream"),
            );
            if let Some(ref key) = api_key {
                match reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                    Ok(val) => {
                        header_map.insert(reqwest::header::AUTHORIZATION, val);
                    }
                    Err(e) => {
                        let mut err_msg = AssistantMessage::empty_for_model(&model);
                        err_msg.stop_reason = StopReason::Error;
                        err_msg.error_message = Some(redact_error_body_with_limit(
                            &format!("Invalid authorization header: {e}"),
                            &secret_refs,
                            4096,
                        ));
                        let _ = sender
                            .send(AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error: err_msg,
                            })
                            .await;
                        return;
                    }
                }
            }
            if let Err(e) = insert_custom_headers(&mut header_map, model.headers.as_ref())
                .and_then(|_| insert_custom_headers(&mut header_map, options.headers.as_ref()))
            {
                let mut err_msg = AssistantMessage::empty_for_model(&model);
                err_msg.stop_reason = StopReason::Error;
                err_msg.error_message =
                    Some(redact_error_body_with_limit(&e, &secret_refs, 4096));
                let _ = sender
                    .send(AssistantMessageEvent::Error {
                        reason: StopReason::Error,
                        error: err_msg,
                    })
                    .await;
                return;
            }

            let resp = match send_with_retry_with_secrets(
                || {
                    client
                        .post(&url)
                        .headers(header_map.clone())
                        .body(body_bytes.clone())
                },
                &RetryPolicy::default(),
                &cancel,
                &secret_refs,
            )
            .await
            {
                Ok(r) => r,
                Err(failure) => {
                    sender.send(retry_failure_event(failure, &model)).await;
                    return;
                }
            };

            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = SseParser::new();
            let mut state = MistralStreamState::new(&model);
            state.set_secrets(secrets_owned.clone());

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        state.mark_aborted("Request was cancelled");
                        let mut batch = Vec::new();
                        state.finish_into(&mut batch);
                        sender.send_batch(batch).await;
                        return;
                    }
                    chunk_opt = tokio::time::timeout(DEFAULT_STREAM_CHUNK_TIMEOUT, byte_stream.next()) => {
                        match chunk_opt {
                            Ok(Some(Ok(bytes))) => {
                                let events = sse_parser.feed_bytes(&bytes);
                                if sse_parser.has_overflowed() {
                                    state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
                                    let mut batch = Vec::new();
                                    state.finish_into(&mut batch);
                                    sender.send_batch(batch).await;
                                    return;
                                }
                                let mut batch = Vec::new();
                                for ev in events {
                                    state.process_chunk_into(&ev.data, &mut batch);
                                }
                                if !sender.send_batch(batch).await {
                                    return;
                                }
                                if state.done {
                                    return;
                                }
                            }
                            Ok(Some(Err(e))) => {
                                state.mark_error(redact_error_body_with_limit(
                                    &e.without_url().to_string(),
                                    &secret_refs,
                                    4096,
                                ));
                                let mut batch = Vec::new();
                                state.finish_into(&mut batch);
                                sender.send_batch(batch).await;
                                return;
                            }
                            Err(_) => {
                                state.mark_error(format!(
                                    "Stream read timed out after {} seconds",
                                    DEFAULT_STREAM_CHUNK_TIMEOUT.as_secs()
                                ));
                                let mut batch = Vec::new();
                                state.finish_into(&mut batch);
                                sender.send_batch(batch).await;
                                return;
                            }
                            Ok(None) => break,
                        }
                    }
                }
            }

            let remaining = sse_parser.finish();
            let mut batch = Vec::new();
            for ev in remaining {
                state.process_chunk_into(&ev.data, &mut batch);
            }
            if sse_parser.has_overflowed() {
                state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
            } else if sse_parser.has_pending_bytes() || !state.received_terminal {
                state.mark_error("Stream ended prematurely before Mistral completion");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

struct ToolCallInProgress {
    content_index: usize,
    raw_args: String,
    id_is_generated: bool,
}

/// Streaming state machine processing Mistral SSE chunks.
pub struct MistralStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    pub received_terminal: bool,
    active_thinking_index: Option<usize>,
    active_text_index: Option<usize>,
    active_tool_calls: HashMap<usize, ToolCallInProgress>,
    tool_call_id_normalizer: MistralToolCallIdNormalizer,
    secrets: Vec<String>,
}

impl MistralStreamState {
    /// Creates a new Mistral stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            received_terminal: false,
            active_thinking_index: None,
            active_text_index: None,
            active_tool_calls: HashMap::new(),
            tool_call_id_normalizer: MistralToolCallIdNormalizer::new(),
            secrets: Vec::new(),
        }
    }

    /// Configures active request secrets to redact from any error messages.
    pub fn set_secrets(&mut self, secrets: Vec<String>) {
        self.secrets = secrets;
    }

    fn redact_error(&self, msg: &str) -> String {
        let refs: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
        redact_error_body_with_limit(msg, &refs, 4096)
    }

    /// Marks the stream as aborted so `finish_into` emits an aborted terminal event.
    pub fn mark_aborted(&mut self, msg: &str) {
        self.output.stop_reason = StopReason::Aborted;
        self.output.error_message = Some(self.redact_error(msg));
    }

    /// Marks the stream as failed so `finish_into` emits an error terminal event.
    pub fn mark_error(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Error;
        let raw = msg.into();
        self.output.error_message = Some(self.redact_error(&raw));
    }

    /// Processes a raw SSE data payload string and sends produced events.
    pub fn process_chunk(&mut self, data: &str, sender: &AssistantEventSender) {
        let mut batch = Vec::new();
        self.process_chunk_into(data, &mut batch);
        for ev in batch {
            sender.push(ev);
        }
    }

    /// Processes a raw SSE data payload string, appending produced events to `events`.
    pub fn process_chunk_into(&mut self, data: &str, events: &mut Vec<AssistantMessageEvent>) {
        if self.done {
            return;
        }
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "[DONE]" {
            self.received_terminal = true;
            self.finish_into(events);
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        if let Some(err_val) = json.get("error").filter(|v| !v.is_null()) {
            self.received_terminal = true;
            let msg = err_val
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err_val.as_str())
                .unwrap_or("Mistral API error");
            self.mark_error(msg);
            self.finish_into(events);
            return;
        }

        if let Some(msg_val) = json.get("message").and_then(|m| m.as_str()) {
            if json.get("choices").is_none() {
                self.received_terminal = true;
                self.mark_error(msg_val);
                self.finish_into(events);
                return;
            }
        }

        if self.output.response_id.is_none() {
            if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                self.output.response_id = Some(id.to_string());
            }
        }

        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        // Usage
        if let Some(usage) = json.get("usage").filter(|u| u.is_object()) {
            let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let completion = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let total = usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            self.output.usage.input = prompt.saturating_sub(cached);
            self.output.usage.output = completion;
            self.output.usage.cache_read = cached;
            self.output.usage.cache_write = 0;
            self.output.usage.total_tokens = if total > 0 {
                total
            } else {
                self.output
                    .usage
                    .input
                    .saturating_add(self.output.usage.output)
                    .saturating_add(cached)
            };
            self.output.usage.calculate_cost(&self.model.cost);
        }

        let Some(choice) = json.get("choices").and_then(|c| c.get(0)) else {
            return;
        };

        if let Some(finish_reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if !finish_reason.is_empty() {
                self.received_terminal = true;
                if !matches!(
                    self.output.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    self.output.stop_reason = match finish_reason {
                        "stop" => StopReason::Stop,
                        "tool_calls" => StopReason::ToolUse,
                        "length" | "model_length" => StopReason::Length,
                        "error" => {
                            if self.output.error_message.is_none() {
                                self.output.error_message =
                                    Some("Mistral stream finished with reason: error".to_string());
                            }
                            StopReason::Error
                        }
                        _ => StopReason::Stop,
                    };
                }
            }
        }

        let Some(delta) = choice.get("delta") else {
            return;
        };

        // 1. Content: string or structured array
        if let Some(content_val) = delta.get("content") {
            if let Some(text_str) = content_val.as_str() {
                self.handle_text_delta(text_str, events);
            } else if let Some(chunks) = content_val.as_array() {
                for chunk in chunks {
                    let chunk_type = chunk.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if chunk_type == "thinking" {
                        let mut thinking_text = String::new();
                        if let Some(th_arr) = chunk.get("thinking").and_then(|v| v.as_array()) {
                            for part in th_arr {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    thinking_text.push_str(t);
                                }
                            }
                        } else if let Some(t) = chunk.get("thinking").and_then(|v| v.as_str()) {
                            thinking_text.push_str(t);
                        }
                        if !thinking_text.is_empty() {
                            self.handle_thinking_delta(&thinking_text, events);
                        }
                    } else if chunk_type == "text" {
                        if let Some(t) = chunk.get("text").and_then(|v| v.as_str()) {
                            self.handle_text_delta(t, events);
                        }
                    }
                }
            }
        }

        // 2. Tool calls
        if let Some(tool_calls) = delta
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
        {
            // Close active text or thinking before tool calls
            self.flush_text_block(events);
            self.flush_thinking_block(events);

            for tc_val in tool_calls {
                let idx = tc_val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let id_opt = tc_val
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let fn_obj = tc_val.get("function");
                let name_opt = fn_obj.and_then(|f| f.get("name")).and_then(|v| v.as_str());
                let args_opt = fn_obj.and_then(|f| f.get("arguments")).and_then(|v| v.as_str());

                if !self.active_tool_calls.contains_key(&idx) {
                    let (id, id_is_generated) = match id_opt {
                        Some(id) => (self.tool_call_id_normalizer.normalize(id), false),
                        None => (
                            self.tool_call_id_normalizer
                                .normalize(&format!("call_{}_{}", now_ms(), idx)),
                            true,
                        ),
                    };
                    let name = name_opt.unwrap_or("").to_string();

                    let tool_call = ToolCall {
                        id,
                        name,
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    };
                    self.output
                        .content
                        .push(AssistantContentBlock::ToolCall(tool_call));
                    let content_index = self.output.content.len() - 1;

                    self.active_tool_calls.insert(
                        idx,
                        ToolCallInProgress {
                            content_index,
                            raw_args: String::new(),
                            id_is_generated,
                        },
                    );

                    events.push(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: self.output.clone(),
                    });
                }

                if let Some(in_progress) = self.active_tool_calls.get_mut(&idx) {
                    // Some proxies emit id and name in separate SSE chunks. A
                    // generated placeholder must give way to the real provider
                    // id when it finally arrives; a provider id already in place
                    // is never overwritten.
                    if let Some(id) = id_opt {
                        if in_progress.id_is_generated {
                            let normalized = self.tool_call_id_normalizer.normalize(id);
                            if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get_mut(in_progress.content_index)
                            {
                                tc.id = normalized;
                                in_progress.id_is_generated = false;
                            }
                        }
                    }
                    if let Some(name) = name_opt {
                        if !name.is_empty() {
                            if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get_mut(in_progress.content_index)
                            {
                                if tc.name.is_empty() {
                                    tc.name = name.to_string();
                                }
                            }
                        }
                    }
                    if let Some(args_delta) = args_opt {
                        if !args_delta.is_empty() {
                            in_progress.raw_args.push_str(args_delta);
                            events.push(AssistantMessageEvent::ToolCallDelta {
                                content_index: in_progress.content_index,
                                delta: args_delta.to_string(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    fn handle_text_delta(&mut self, text: &str, events: &mut Vec<AssistantMessageEvent>) {
        if text.is_empty() {
            return;
        }
        self.flush_thinking_block(events);

        let content_index = match self.active_text_index {
            Some(idx) => idx,
            None => {
                self.output
                    .content
                    .push(AssistantContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                let idx = self.output.content.len() - 1;
                self.active_text_index = Some(idx);
                events.push(AssistantMessageEvent::TextStart {
                    content_index: idx,
                    partial: self.output.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get_mut(content_index) {
            tc.text.push_str(text);
        }

        events.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta: text.to_string(),
            partial: self.output.metadata_clone(),
        });
    }

    fn handle_thinking_delta(&mut self, text: &str, events: &mut Vec<AssistantMessageEvent>) {
        if text.is_empty() {
            return;
        }
        self.flush_text_block(events);

        let content_index = match self.active_thinking_index {
            Some(idx) => idx,
            None => {
                self.output
                    .content
                    .push(AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                let idx = self.output.content.len() - 1;
                self.active_thinking_index = Some(idx);
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: idx,
                    partial: self.output.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContentBlock::Thinking(tc)) = self.output.content.get_mut(content_index) {
            tc.thinking.push_str(text);
        }

        events.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta: text.to_string(),
            partial: self.output.metadata_clone(),
        });
    }

    fn flush_text_block(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if let Some(text_idx) = self.active_text_index.take() {
            let content = if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get(text_idx) {
                tc.text.clone()
            } else {
                String::new()
            };
            events.push(AssistantMessageEvent::TextEnd {
                content_index: text_idx,
                content,
                partial: self.output.clone(),
            });
        }
    }

    fn flush_thinking_block(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if let Some(th_idx) = self.active_thinking_index.take() {
            let content = if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get(th_idx) {
                th.thinking.clone()
            } else {
                String::new()
            };
            events.push(AssistantMessageEvent::ThinkingEnd {
                content_index: th_idx,
                content,
                partial: self.output.clone(),
            });
        }
    }

    /// Closes all active blocks, completes tool calls parsing, and emits Done/Error events.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut batch = Vec::new();
        let out = self.finish_into(&mut batch);
        for ev in batch {
            sender.push(ev);
        }
        out
    }

    /// Closes all active blocks, completes tool calls parsing, and appends terminal events to `events`.
    pub fn finish_into(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
        self.flush_thinking_block(events);
        self.flush_text_block(events);

        // Finalize tool calls
        let mut tool_call_indices: Vec<(usize, usize, String)> = self
            .active_tool_calls
            .drain()
            .map(|(key, in_prog)| (key, in_prog.content_index, in_prog.raw_args))
            .collect();
        tool_call_indices.sort_by_key(|t| t.0);

        for (_, content_index, raw_args) in tool_call_indices {
            let (parsed_args, err) =
                crate::streaming_json::parse_tool_call_arguments(Some(&raw_args));

            let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                self.output.content.get_mut(content_index)
            {
                tc.arguments = parsed_args;
                tc.arguments_error = err;
                tc.clone()
            } else {
                continue;
            };

            events.push(AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial: self.output.clone(),
            });
        }

        if self.output.stop_reason == StopReason::Stop
            && self.output.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)))
        {
            self.output.stop_reason = StopReason::ToolUse;
        }

        if !self.done {
            self.done = true;
            if matches!(
                self.output.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                events.push(AssistantMessageEvent::Error {
                    reason: self.output.stop_reason,
                    error: self.output.clone(),
                });
            } else {
                events.push(AssistantMessageEvent::Done {
                    reason: self.output.stop_reason,
                    message: self.output.clone(),
                });
            }
        }

        self.output.clone()
    }
}

/// Pure processor converting Mistral SSE events into an AssistantMessage.
pub fn process_mistral_sse_events(
    model: &Model,
    events: &[SseEvent],
    sender: &AssistantEventSender,
) -> AssistantMessage {
    let mut state = MistralStreamState::new(model);
    for event in events {
        state.process_chunk(&event.data, sender);
    }
    state.finish(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_mistral_tool_call_id() {
        // Valid 9-char alphanumeric string is preserved
        let valid = "abc123XYZ";
        assert_eq!(normalize_mistral_tool_call_id(valid), "abc123XYZ");

        // Non-alphanumeric or different length is hashed to 9-char hex
        let short = "call_1";
        let norm_short = normalize_mistral_tool_call_id(short);
        assert_eq!(norm_short.len(), 9);
        assert!(norm_short.chars().all(|c| c.is_ascii_alphanumeric()));

        // Deterministic
        assert_eq!(norm_short, normalize_mistral_tool_call_id(short));

        let long = "call_123456789_abcdef";
        let norm_long = normalize_mistral_tool_call_id(long);
        assert_eq!(norm_long.len(), 9);
        assert!(norm_long.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_build_mistral_request() {
        let model = Model {
            id: "magistral-medium-latest".to_string(),
            name: "Magistral Medium Latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        };

        let context = Context {
            system_prompt: Some("System instructions".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Hello".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "long_tool_call_id_here".to_string(),
                        name: "calculator".to_string(),
                        arguments: {
                            let mut m = serde_json::Map::new();
                            m.insert("expr".to_string(), serde_json::json!("2+2"));
                            m
                        },
                        arguments_error: None,
                        thought_signature: None,
                    })],
                    api: "mistral-conversations".to_string(),
                    provider: "mistral".to_string(),
                    model: "mistral-small".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "long_tool_call_id_here".to_string(),
                    tool_name: "calculator".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "4".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 3,
                    duration_ms: None,
                    usage: None,
                }),
            ],
            tools: None,
        };

        let options = StreamOptions {
            api_key: Some("key-123".to_string()),
            temperature: Some(0.7),
            max_tokens: Some(4096),
            reasoning: Some(ThinkingLevel::High),
            session_id: None,
            headers: None,
            ..Default::default()
        };

        let req = build_mistral_request(&model, &context, &options);
        assert_eq!(req["model"], "magistral-medium-latest");
        assert_eq!(req["stream"], true);
        assert_eq!(req["prompt_mode"], "reasoning");
        assert_eq!(req["temperature"], 0.7);
        assert_eq!(req["max_tokens"], 4096);

        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");

        // Assistant tool call ID normalized to 9 chars
        let asst_tc_id = messages[2]["tool_calls"][0]["id"].as_str().unwrap();
        assert_eq!(asst_tc_id.len(), 9);

        // ToolResult tool call ID normalized identically
        let tool_result_id = messages[3]["tool_call_id"].as_str().unwrap();
        assert_eq!(tool_result_id, asst_tc_id);
    }

    #[test]
    fn test_process_mistral_sse_events() {
        let model = Model {
            id: "magistral-medium-latest".to_string(),
            name: "Magistral Medium Latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();

        let chunks = vec![
            serde_json::json!({
                "id": "cmpl-mistral-123",
                "choices": [{
                    "delta": {
                        "content": [{
                            "type": "thinking",
                            "thinking": [{ "type": "text", "text": "Let me think." }]
                        }]
                    }
                }]
            }).to_string(),
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "content": "The answer is "
                    }
                }]
            }).to_string(),
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "content": "42."
                    }
                }]
            }).to_string(),
            serde_json::json!({
                "choices": [{
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 15,
                    "completion_tokens": 8,
                    "total_tokens": 23,
                    "prompt_tokens_details": {
                        "cached_tokens": 5
                    }
                }
            }).to_string(),
        ];

        let sse_events: Vec<SseEvent> = chunks
            .into_iter()
            .map(|data| SseEvent {
                event: None,
                data,
            })
            .collect();

        let result = process_mistral_sse_events(&model, &sse_events, &sender);
        assert_eq!(result.response_id.as_deref(), Some("cmpl-mistral-123"));
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.usage.input, 10); // 15 - 5
        assert_eq!(result.usage.output, 8);
        assert_eq!(result.usage.cache_read, 5);
        assert_eq!(result.usage.total_tokens, 23);

        assert_eq!(result.content.len(), 2);
        match &result.content[0] {
            AssistantContentBlock::Thinking(th) => assert_eq!(th.thinking, "Let me think."),
            _ => panic!("Expected Thinking block at index 0"),
        }
        match &result.content[1] {
            AssistantContentBlock::Text(tc) => assert_eq!(tc.text, "The answer is 42."),
            _ => panic!("Expected Text block at index 1"),
        }
    }

    #[test]
    fn mistral_normalizer_resolves_hash_collisions_deterministically() {
        let mut normalizer = MistralToolCallIdNormalizer::new();

        // Two distinct IDs that would collide on same 9-char alphanumeric string if simply stripped
        let id_a = "abcdef123";
        let id_b = "abcdef_123";

        let norm_a = normalizer.normalize(id_a);
        let norm_b = normalizer.normalize(id_b);

        assert_eq!(norm_a.len(), 9);
        assert_eq!(norm_b.len(), 9);
        assert!(norm_a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(norm_b.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(norm_a, norm_b, "Colliding IDs must resolve to distinct candidates");

        // Deterministic repeat
        assert_eq!(normalizer.normalize(id_a), norm_a);
        assert_eq!(normalizer.normalize(id_b), norm_b);

        let model = Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Call tools".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: id_a.to_string(),
                            name: "tool_a".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: id_b.to_string(),
                            name: "tool_b".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "other".to_string(),
                    provider: "other".to_string(),
                    model: "other".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 200,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: id_a.to_string(),
                    tool_name: "tool_a".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "res_a".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 300,
                    duration_ms: None,
                    usage: None,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: id_b.to_string(),
                    tool_name: "tool_b".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "res_b".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 301,
                    duration_ms: None,
                    usage: None,
                }),
            ],
            tools: None,
        };

        let req = build_mistral_request(&model, &context, &StreamOptions::default());
        let messages = req["messages"].as_array().expect("messages array");
        let assistant_tc = messages[1]["tool_calls"].as_array().unwrap();
        assert_eq!(assistant_tc[0]["id"], norm_a);
        assert_eq!(assistant_tc[1]["id"], norm_b);

        // Tool results adopt the same mapped IDs
        assert_eq!(messages[2]["tool_call_id"], norm_a);
        assert_eq!(messages[3]["tool_call_id"], norm_b);
    }

    #[test]
    fn test_mistral_empty_tool_arguments() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "id": "cmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_123456789",
                            "type": "function",
                            "function": {
                                "name": "get_time",
                                "arguments": ""
                            }
                        }]
                    }
                }]
            }).to_string(),
            "[DONE]".to_string(),
        ];
        let sse_events: Vec<SseEvent> = chunks.into_iter().map(|data| SseEvent { event: None, data }).collect();
        let result = process_mistral_sse_events(&model, &sse_events, &sender);
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.name, "get_time");
                assert_eq!(tc.arguments_error, None);
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_mistral_error_chunk() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "message": "Rate limit exceeded"
            }).to_string(),
        ];
        let sse_events: Vec<SseEvent> = chunks.into_iter().map(|data| SseEvent { event: None, data }).collect();
        let result = process_mistral_sse_events(&model, &sse_events, &sender);
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("Rate limit exceeded"));
    }

    #[test]
    fn streamed_tool_call_ids_with_separator_variants_stay_distinct() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "id": "cmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            { "index": 0, "id": "ab-cd-ef-ghi", "type": "function",
                              "function": { "name": "tool_a", "arguments": "" } },
                            { "index": 1, "id": "ab_cd_ef_ghi", "type": "function",
                              "function": { "name": "tool_b", "arguments": "" } }
                        ]
                    }
                }]
            })
            .to_string(),
            "[DONE]".to_string(),
        ];
        let sse_events: Vec<SseEvent> = chunks
            .into_iter()
            .map(|data| SseEvent { event: None, data })
            .collect();
        let result = process_mistral_sse_events(&model, &sse_events, &sender);
        let ids: Vec<&str> = result
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContentBlock::ToolCall(tc) => Some(tc.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "streamed IDs differing only by a separator collapsed into one"
        );
    }

    #[test]
    fn streamed_tool_call_adopts_provider_id_arriving_in_later_chunk() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };
        let mut state = MistralStreamState::new(&model);
        let mut events = Vec::new();

        // Chunk 1 carries only the name, so a generated id is used initially.
        state.process_chunk_into(
            &serde_json::json!({
                "id": "cmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": { "name": "get_weather", "arguments": "" }
                        }]
                    }
                }]
            })
            .to_string(),
            &mut events,
        );

        // Chunk 2 delivers the real provider id on the same tool call index.
        state.process_chunk_into(
            &serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_real_123",
                            "function": { "arguments": "{}" }
                        }]
                    }
                }]
            })
            .to_string(),
            &mut events,
        );

        state.finish_into(&mut events);

        let end = events
            .iter()
            .find_map(|ev| match ev {
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("ToolCallEnd emitted");
        assert_eq!(end.name, "get_weather");
        assert_eq!(
            end.id,
            normalize_mistral_tool_call_id("call_real_123"),
            "later provider id must replace the generated id, got {:?}",
            end.id
        );
    }

    #[test]
    fn mistral_null_error_model_length_error_message_empty_tool_calls_and_multi_block_delta() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            ..Default::default()
        };

        // 1. "error": null, "usage": null, empty tool_calls: [], empty id: "" followed by real id,
        //    and finish_reason "model_length" + saturating usage totals.
        let mut state = MistralStreamState::new(&model);
        let mut events = Vec::new();
        for chunk in [
            serde_json::json!({
                "id": "cmpl-m1",
                "error": null,
                "usage": null,
                "choices": [{
                    "delta": { "content": "Part 1 ", "tool_calls": [] }
                }]
            }),
            serde_json::json!({
                "choices": [{
                    "delta": { "content": "Part 2", "tool_calls": [] }
                }]
            }),
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "",
                            "function": { "name": "ping", "arguments": "{\"x\":" }
                        }]
                    }
                }]
            }),
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "realid999",
                            "function": { "arguments": "1}" }
                        }]
                    },
                    "finish_reason": "model_length"
                }],
                "usage": {
                    "prompt_tokens": u64::MAX,
                    "completion_tokens": 5
                }
            }),
        ] {
            state.process_chunk_into(&chunk.to_string(), &mut events);
        }
        let delta = events
            .iter()
            .rfind(|ev| matches!(ev, AssistantMessageEvent::ToolCallDelta { .. }))
            .expect("ToolCallDelta emitted");
        match delta {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                partial,
                ..
            } => {
                assert_eq!(*content_index, 1);
                assert_eq!(partial.content.len(), 2);
            }
            _ => unreachable!(),
        }
        let res = state.finish_into(&mut events);
        assert_eq!(res.stop_reason, StopReason::Length);
        assert_eq!(res.error_message, None);
        assert_eq!(res.usage.total_tokens, u64::MAX);
        assert_eq!(res.content.len(), 2);
        match &res.content[0] {
            AssistantContentBlock::Text(t) => assert_eq!(t.text, "Part 1 Part 2"),
            other => panic!("Expected merged Text block, got {other:?}"),
        }
        match &res.content[1] {
            AssistantContentBlock::ToolCall(tc) => assert_eq!(tc.id, "realid999"),
            other => panic!("Expected ToolCall block, got {other:?}"),
        }

        // 2. finish_reason "error" sets error_message
        let mut err_state = MistralStreamState::new(&model);
        let mut err_events = Vec::new();
        err_state.process_chunk_into(
            &serde_json::json!({
                "choices": [{ "delta": {}, "finish_reason": "error" }]
            })
            .to_string(),
            &mut err_events,
        );
        let err_res = err_state.finish_into(&mut err_events);
        assert_eq!(err_res.stop_reason, StopReason::Error);
        assert!(err_res.error_message.is_some());
    }

    #[test]
    fn mistral_url_preserves_query_params() {
        assert_eq!(
            mistral_chat_completions_url("https://proxy.example.com/v1?api-version=2024"),
            "https://proxy.example.com/v1/chat/completions?api-version=2024"
        );
        assert_eq!(
            mistral_chat_completions_url(
                "https://proxy.example.com/v1/chat/completions/?api-version=2024"
            ),
            "https://proxy.example.com/v1/chat/completions?api-version=2024"
        );
        assert_eq!(
            mistral_chat_completions_url(""),
            "https://api.mistral.ai/v1/chat/completions"
        );
    }

    #[test]
    fn mistral_ignores_chunks_after_terminal_error_and_preserves_stop_reason() {
        let model = Model {
            id: "mistral-large-latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            ..Default::default()
        };
        let mut state = MistralStreamState::new(&model);
        let mut events = Vec::new();

        state.process_chunk_into(
            &serde_json::json!({ "error": { "message": "Service unavailable" } }).to_string(),
            &mut events,
        );
        let len_after_error = events.len();
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                ..
            })
        ));

        state.process_chunk_into(
            &serde_json::json!({
                "choices": [{
                    "delta": { "content": "trailing" },
                    "finish_reason": "stop"
                }]
            })
            .to_string(),
            &mut events,
        );
        state.process_chunk_into("[DONE]", &mut events);

        assert_eq!(events.len(), len_after_error);
        assert_eq!(state.output.stop_reason, StopReason::Error);
        assert_eq!(
            state.output.error_message.as_deref(),
            Some("Service unavailable")
        );

        // Even before `[DONE]` triggers `done = true`, a subsequent `finish_reason` cannot overwrite `Error`.
        let mut err_state = MistralStreamState::new(&model);
        let mut err_events = Vec::new();
        err_state.process_chunk_into(
            &serde_json::json!({
                "choices": [{ "delta": {}, "finish_reason": "error" }]
            })
            .to_string(),
            &mut err_events,
        );
        err_state.process_chunk_into(
            &serde_json::json!({
                "choices": [{ "delta": {}, "finish_reason": "stop" }]
            })
            .to_string(),
            &mut err_events,
        );
        err_state.process_chunk_into("[DONE]", &mut err_events);
        assert_eq!(err_state.output.stop_reason, StopReason::Error);
    }
}
