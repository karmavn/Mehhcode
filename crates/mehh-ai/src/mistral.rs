//! Mistral Conversations API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;
use sha2::{Digest, Sha256};

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
            client: reqwest::Client::new(),
        }
    }
}

/// Normalizes a tool call ID to Mistral's requirement of exactly 9 ASCII alphanumeric characters.
/// If `id` already has length 9 and is all ASCII alphanumeric, returns it as-is.
/// Otherwise, computes a deterministic 9-character alphanumeric hex digest from SHA-256.
pub fn normalize_mistral_tool_call_id(id: &str) -> String {
    let filtered: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if filtered.len() == 9 {
        return filtered;
    }

    let hash = Sha256::digest(id.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in hash {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex[..9].to_string()
}

/// Builds the JSON request payload for Mistral Chat Completions API.
pub fn build_mistral_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // System prompt
    let mut system_texts = Vec::new();
    if let Some(sys) = &context.system_prompt {
        if !sys.is_empty() {
            system_texts.push(sys.clone());
        }
    }
    for msg in &context.messages {
        if let Message::System(sys) = msg {
            if !sys.content.is_empty() {
                system_texts.push(sys.content.clone());
            }
        }
    }
    if !system_texts.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system_texts.join("\n\n"),
        }));
    }

    for msg in &context.messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let content_val = match &u.content {
                    UserContent::Text(text) => serde_json::json!(text),
                    UserContent::Blocks(blocks) => {
                        let chunks: Vec<serde_json::Value> = blocks
                            .iter()
                            .map(|b| match b {
                                UserContentBlock::Text(t) => {
                                    serde_json::json!({
                                        "type": "text",
                                        "text": t.text,
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
                    asst_map.insert("content".to_string(), serde_json::json!(text_content));
                }

                let tool_calls = a.tool_calls();
                if !tool_calls.is_empty() {
                    let tc_json: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            let normalized_id = normalize_mistral_tool_call_id(&tc.id);
                            let args_str = serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".to_string());
                            serde_json::json!({
                                "id": normalized_id,
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

                messages.push(serde_json::Value::Object(asst_map));
            }
            Message::ToolResult(tr) => {
                let normalized_id = normalize_mistral_tool_call_id(&tr.tool_call_id);
                let text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ToolResultContentBlock::Text(tc) => Some(tc.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": normalized_id,
                    "name": tr.tool_name,
                    "content": text,
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
        let context = context.clone();
        let options = options.clone();

        tokio::spawn(async move {
            let body = build_mistral_request(&model, &context, &options);
            let base_url = if model.base_url.is_empty() {
                "https://api.mistral.ai/v1"
            } else {
                model.base_url.trim_end_matches('/')
            };
            let url = if base_url.ends_with("/chat/completions") {
                base_url.to_string()
            } else {
                format!("{base_url}/chat/completions")
            };

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");
            req = req.header("accept", "text/event-stream");

            if let Some(api_key) = &options.api_key {
                req = req.header("authorization", format!("Bearer {api_key}"));
            }
            if let Some(headers) = &model.headers {
                for (k, v) in headers {
                    req = req.header(k, v);
                }
            }
            if let Some(headers) = &options.headers {
                for (k, v) in headers {
                    req = req.header(k, v);
                }
            }
            req = req.json(&body);

            let resp = tokio::select! {
                _ = cancel.cancelled() => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Aborted;
                    err_msg.error_message = Some("Request was cancelled".to_string());
                    sender.push(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: err_msg,
                    });
                    return;
                }
                res = req.send() => {
                    match res {
                        Ok(r) => r,
                        Err(e) => {
                            let mut err_msg = AssistantMessage::empty_for_model(&model);
                            err_msg.stop_reason = StopReason::Error;
                            err_msg.error_message = Some(e.to_string());
                            sender.push(AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error: err_msg,
                            });
                            return;
                        }
                    }
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let mut err_msg = AssistantMessage::empty_for_model(&model);
                err_msg.stop_reason = StopReason::Error;
                err_msg.error_message = Some(format!("HTTP {status}: {text}"));
                sender.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: err_msg,
                });
                return;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = SseParser::new();
            let mut state = MistralStreamState::new(&model);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let mut err_msg = state.finish(&sender);
                        err_msg.stop_reason = StopReason::Aborted;
                        err_msg.error_message = Some("Request was cancelled".to_string());
                        sender.push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: err_msg,
                        });
                        return;
                    }
                    chunk_opt = byte_stream.next() => {
                        match chunk_opt {
                            Some(Ok(bytes)) => {
                                let chunk_str = String::from_utf8_lossy(&bytes);
                                let events = sse_parser.feed(&chunk_str);
                                for ev in events {
                                    state.process_chunk(&ev.data, &sender);
                                }
                            }
                            Some(Err(e)) => {
                                let mut err_msg = state.finish(&sender);
                                err_msg.stop_reason = StopReason::Error;
                                err_msg.error_message = Some(e.to_string());
                                sender.push(AssistantMessageEvent::Error {
                                    reason: StopReason::Error,
                                    error: err_msg,
                                });
                                return;
                            }
                            None => break,
                        }
                    }
                }
            }

            let remaining = sse_parser.finish();
            for ev in remaining {
                state.process_chunk(&ev.data, &sender);
            }
            state.finish(&sender);
        });

        stream
    }
}

struct ToolCallInProgress {
    content_index: usize,
    raw_args: String,
}

/// Streaming state machine processing Mistral SSE chunks.
pub struct MistralStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    active_thinking_index: Option<usize>,
    active_text_index: Option<usize>,
    active_tool_calls: HashMap<usize, ToolCallInProgress>,
}

impl MistralStreamState {
    /// Creates a new Mistral stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            active_thinking_index: None,
            active_text_index: None,
            active_tool_calls: HashMap::new(),
        }
    }

    /// Processes a raw SSE data payload string.
    pub fn process_chunk(&mut self, data: &str, sender: &AssistantEventSender) {
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        if self.output.response_id.is_none() {
            if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                self.output.response_id = Some(id.to_string());
            }
        }

        if !self.started {
            self.started = true;
            sender.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        // Usage
        if let Some(usage) = json.get("usage") {
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
                self.output.usage.input + self.output.usage.output + cached
            };
            self.output.usage.calculate_cost(&self.model.cost);
        }

        let Some(choice) = json.get("choices").and_then(|c| c.get(0)) else {
            return;
        };

        if let Some(finish_reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            self.output.stop_reason = match finish_reason {
                "stop" => StopReason::Stop,
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::Length,
                "error" => StopReason::Error,
                _ => StopReason::Stop,
            };
        }

        let Some(delta) = choice.get("delta") else {
            return;
        };

        // 1. Content: string or structured array
        if let Some(content_val) = delta.get("content") {
            if let Some(text_str) = content_val.as_str() {
                self.handle_text_delta(text_str, sender);
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
                            self.handle_thinking_delta(&thinking_text, sender);
                        }
                    } else if chunk_type == "text" {
                        if let Some(t) = chunk.get("text").and_then(|v| v.as_str()) {
                            self.handle_text_delta(t, sender);
                        }
                    }
                }
            }
        }

        // 2. Tool calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            // Close active text or thinking before tool calls
            self.flush_text_block(sender);
            self.flush_thinking_block(sender);

            for tc_val in tool_calls {
                let idx = tc_val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let id_opt = tc_val.get("id").and_then(|v| v.as_str());
                let fn_obj = tc_val.get("function");
                let name_opt = fn_obj.and_then(|f| f.get("name")).and_then(|v| v.as_str());
                let args_opt = fn_obj.and_then(|f| f.get("arguments")).and_then(|v| v.as_str());

                if !self.active_tool_calls.contains_key(&idx) {
                    let id = id_opt
                        .map(normalize_mistral_tool_call_id)
                        .unwrap_or_else(|| {
                            normalize_mistral_tool_call_id(&format!("call_{}_{}", now_ms(), idx))
                        });
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
                        },
                    );

                    sender.push(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: self.output.clone(),
                    });
                }

                if let Some(in_progress) = self.active_tool_calls.get_mut(&idx) {
                    if let Some(args_delta) = args_opt {
                        if !args_delta.is_empty() {
                            in_progress.raw_args.push_str(args_delta);
                            sender.push(AssistantMessageEvent::ToolCallDelta {
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

    fn handle_text_delta(&mut self, text: &str, sender: &AssistantEventSender) {
        if text.is_empty() {
            return;
        }
        self.flush_thinking_block(sender);

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
                sender.push(AssistantMessageEvent::TextStart {
                    content_index: idx,
                    partial: self.output.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get_mut(content_index) {
            tc.text.push_str(text);
        }

        sender.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta: text.to_string(),
            partial: self.output.clone(),
        });
    }

    fn handle_thinking_delta(&mut self, text: &str, sender: &AssistantEventSender) {
        if text.is_empty() {
            return;
        }
        self.flush_text_block(sender);

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
                sender.push(AssistantMessageEvent::ThinkingStart {
                    content_index: idx,
                    partial: self.output.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContentBlock::Thinking(tc)) = self.output.content.get_mut(content_index) {
            tc.thinking.push_str(text);
        }

        sender.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta: text.to_string(),
            partial: self.output.clone(),
        });
    }

    fn flush_text_block(&mut self, sender: &AssistantEventSender) {
        if let Some(text_idx) = self.active_text_index.take() {
            let content = if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get(text_idx) {
                tc.text.clone()
            } else {
                String::new()
            };
            sender.push(AssistantMessageEvent::TextEnd {
                content_index: text_idx,
                content,
                partial: self.output.clone(),
            });
        }
    }

    fn flush_thinking_block(&mut self, sender: &AssistantEventSender) {
        if let Some(th_idx) = self.active_thinking_index.take() {
            let content = if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get(th_idx) {
                th.thinking.clone()
            } else {
                String::new()
            };
            sender.push(AssistantMessageEvent::ThinkingEnd {
                content_index: th_idx,
                content,
                partial: self.output.clone(),
            });
        }
    }

    /// Closes all active blocks, completes tool calls parsing, and emits Done/Error events.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        self.flush_thinking_block(sender);
        self.flush_text_block(sender);

        // Finalize tool calls
        let mut tool_call_indices: Vec<(usize, usize, String)> = self
            .active_tool_calls
            .drain()
            .map(|(key, in_prog)| (key, in_prog.content_index, in_prog.raw_args))
            .collect();
        tool_call_indices.sort_by_key(|t| t.0);

        for (_, content_index, raw_args) in tool_call_indices {
            let parsed_args = match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw_args) {
                Ok(map) => map,
                Err(e) => {
                    if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                        tc.arguments_error = Some(e.to_string());
                    }
                    serde_json::Map::new()
                }
            };

            let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                tc.arguments = parsed_args;
                tc.clone()
            } else {
                continue;
            };

            sender.push(AssistantMessageEvent::ToolCallEnd {
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
            if self.output.stop_reason == StopReason::Error {
                sender.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: self.output.clone(),
                });
            } else {
                sender.push(AssistantMessageEvent::Done {
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
                    model: "magistral-medium-latest".to_string(),
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
}
