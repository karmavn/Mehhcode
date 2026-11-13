//! Anthropic Messages API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::types::*;

/// Provider for the Anthropic Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Creates a new Anthropic provider with the given HTTP client.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
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
            let body = build_anthropic_request(&model, &context, &options);
            let url = if model.base_url.ends_with("/messages") {
                model.base_url.clone()
            } else {
                format!("{}/messages", model.base_url.trim_end_matches('/'))
            };

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");
            req = req.header("anthropic-version", "2023-06-01");

            if let Some(api_key) = &options.api_key {
                req = req.header("x-api-key", api_key);
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
            let mut state = AnthropicStreamState::new(&model);

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
                                state.process_events(&events, &sender);
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
            state.process_events(&remaining, &sender);
            state.finish(&sender);
        });

        stream
    }
}

/// Builds the JSON request payload for the Anthropic Messages API.
pub fn build_anthropic_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    let mut system_text = String::new();
    if let Some(sp) = &context.system_prompt {
        if !sp.is_empty() {
            system_text.push_str(sp);
        }
    }

    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut has_seen_non_system = false;

    for msg in &context.messages {
        match msg {
            Message::System(sys) => {
                if !has_seen_non_system {
                    if !system_text.is_empty() {
                        system_text.push_str("\n\n");
                    }
                    system_text.push_str(&sys.content);
                } else {
                    push_user_block(&mut messages, serde_json::json!({
                        "type": "text",
                        "text": sys.content,
                    }));
                }
            }
            Message::User(u) => {
                has_seen_non_system = true;
                match &u.content {
                    UserContent::Text(text) => {
                        push_user_block(&mut messages, serde_json::json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                    UserContent::Blocks(blocks) => {
                        for b in blocks {
                            match b {
                                UserContentBlock::Text(tc) => {
                                    push_user_block(&mut messages, serde_json::json!({
                                        "type": "text",
                                        "text": tc.text,
                                    }));
                                }
                                UserContentBlock::Image(img) => {
                                    push_user_block(&mut messages, serde_json::json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": img.mime_type,
                                            "data": img.data,
                                        }
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                has_seen_non_system = true;
                let mut tool_contents = Vec::new();
                for b in &tr.content {
                    match b {
                        ToolResultContentBlock::Text(tc) => {
                            tool_contents.push(serde_json::json!({
                                "type": "text",
                                "text": tc.text,
                            }));
                        }
                        ToolResultContentBlock::Image(img) => {
                            tool_contents.push(serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.mime_type,
                                    "data": img.data,
                                }
                            }));
                        }
                    }
                }
                let tool_block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tr.tool_call_id,
                    "content": tool_contents,
                    "is_error": tr.is_error,
                });
                push_user_block(&mut messages, tool_block);
            }
            Message::Assistant(a) => {
                has_seen_non_system = true;
                let mut assistant_blocks = Vec::new();
                for b in &a.content {
                    match b {
                        AssistantContentBlock::Text(tc) => {
                            assistant_blocks.push(serde_json::json!({
                                "type": "text",
                                "text": tc.text,
                            }));
                        }
                        AssistantContentBlock::Thinking(tc) => {
                            if tc.redacted.unwrap_or(false) {
                                assistant_blocks.push(serde_json::json!({
                                    "type": "redacted_thinking",
                                    "data": tc.thinking_signature.as_deref().unwrap_or(""),
                                }));
                            } else {
                                let mut block = serde_json::json!({
                                    "type": "thinking",
                                    "thinking": tc.thinking,
                                });
                                if let Some(sig) = &tc.thinking_signature {
                                    block.as_object_mut().unwrap().insert(
                                        "signature".to_string(),
                                        serde_json::json!(sig),
                                    );
                                }
                                assistant_blocks.push(block);
                            }
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            assistant_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": serde_json::Value::Object(tc.arguments.clone()),
                            }));
                        }
                    }
                }
                // Merge into previous assistant message if consecutive
                if let Some(last) = messages.last_mut() {
                    if last.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                        if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                            content.extend(assistant_blocks);
                            continue;
                        }
                    }
                }
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": assistant_blocks,
                }));
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("model".to_string(), serde_json::json!(model.id));
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));
    req_map.insert("stream".to_string(), serde_json::json!(true));

    if !system_text.is_empty() {
        req_map.insert("system".to_string(), serde_json::json!(system_text));
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let tool_vals: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            req_map.insert("tools".to_string(), serde_json::Value::Array(tool_vals));
        }
    }

    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let is_reasoning_enabled = model.reasoning
        && options.reasoning.as_ref().is_some_and(|r| *r != ThinkingLevel::Off);

    if is_reasoning_enabled {
        let reasoning_level = options.reasoning.as_ref().unwrap();
        let budget = match reasoning_level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16384,
        };
        let effective_budget = budget.max(1024);
        let effective_max_tokens = max_tokens.max(effective_budget + 1024);

        req_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(effective_max_tokens),
        );
        req_map.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": "enabled",
                "budget_tokens": effective_budget,
            }),
        );
    } else {
        req_map.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
        if let Some(temp) = options.temperature {
            req_map.insert("temperature".to_string(), serde_json::json!(temp));
        }
    }

    serde_json::Value::Object(req_map)
}

fn push_user_block(messages: &mut Vec<serde_json::Value>, block: serde_json::Value) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                content.push(block);
                return;
            }
        }
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": [block],
    }));
}

/// Internal block representation currently open during Anthropic streaming.
enum AnthropicOpenBlock {
    Text { content_index: usize },
    Thinking { content_index: usize },
    ToolCall {
        content_index: usize,
        json_buffer: String,
    },
}

/// Streaming state machine processing Anthropic SSE events.
pub struct AnthropicStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    open_blocks: HashMap<usize, AnthropicOpenBlock>,
}

impl AnthropicStreamState {
    /// Creates a new streaming state initialized for the model.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            open_blocks: HashMap::new(),
        }
    }

    /// Processes a slice of SSE events and emits corresponding AssistantMessageEvents.
    pub fn process_events(&mut self, events: &[SseEvent], sender: &AssistantEventSender) {
        for event in events {
            self.process_event(event, sender);
        }
    }

    /// Processes a single SSE event.
    pub fn process_event(&mut self, sse_event: &SseEvent, sender: &AssistantEventSender) {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&sse_event.data) else {
            return;
        };

        let event_type = sse_event
            .event
            .as_deref()
            .or_else(|| json.get("type").and_then(|v| v.as_str()))
            .unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(message) = json.get("message") {
                    if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                        self.output.response_id = Some(id.to_string());
                    }
                    if let Some(usage) = message.get("usage") {
                        self.update_usage(usage);
                    }
                }
                if !self.started {
                    self.started = true;
                    sender.push(AssistantMessageEvent::Start {
                        partial: self.output.clone(),
                    });
                }
            }
            "content_block_start" => {
                let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(block) = json.get("content_block") {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            let initial_text = block
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            self.output.content.push(AssistantContentBlock::Text(TextContent {
                                text: initial_text,
                                text_signature: None,
                            }));
                            let content_index = self.output.content.len() - 1;
                            self.open_blocks
                                .insert(index, AnthropicOpenBlock::Text { content_index });
                            sender.push(AssistantMessageEvent::TextStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        "thinking" => {
                            let initial_thinking = block
                                .get("thinking")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let signature = block
                                .get("signature")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            self.output
                                .content
                                .push(AssistantContentBlock::Thinking(ThinkingContent {
                                    thinking: initial_thinking,
                                    thinking_signature: signature,
                                    redacted: None,
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.open_blocks
                                .insert(index, AnthropicOpenBlock::Thinking { content_index });
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        "redacted_thinking" => {
                            let signature = block
                                .get("data")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            self.output
                                .content
                                .push(AssistantContentBlock::Thinking(ThinkingContent {
                                    thinking: "[Reasoning redacted]".to_string(),
                                    thinking_signature: signature,
                                    redacted: Some(true),
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.open_blocks
                                .insert(index, AnthropicOpenBlock::Thinking { content_index });
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        "tool_use" => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            self.output
                                .content
                                .push(AssistantContentBlock::ToolCall(ToolCall {
                                    id,
                                    name,
                                    arguments: serde_json::Map::new(),
                                    arguments_error: None,
                                    thought_signature: None,
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.open_blocks.insert(
                                index,
                                AnthropicOpenBlock::ToolCall {
                                    content_index,
                                    json_buffer: String::new(),
                                },
                            );
                            sender.push(AssistantMessageEvent::ToolCallStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(delta) = json.get("delta") {
                    let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            if let Some(AnthropicOpenBlock::Text { content_index }) =
                                self.open_blocks.get(&index)
                            {
                                let content_index = *content_index;
                                let text_delta = delta
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if let Some(AssistantContentBlock::Text(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    tc.text.push_str(&text_delta);
                                }
                                sender.push(AssistantMessageEvent::TextDelta {
                                    content_index,
                                    delta: text_delta,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                        "thinking_delta" => {
                            if let Some(AnthropicOpenBlock::Thinking { content_index }) =
                                self.open_blocks.get(&index)
                            {
                                let content_index = *content_index;
                                let thinking_delta = delta
                                    .get("thinking")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if let Some(AssistantContentBlock::Thinking(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    tc.thinking.push_str(&thinking_delta);
                                }
                                sender.push(AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta: thinking_delta,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                        "signature_delta" => {
                            if let Some(AnthropicOpenBlock::Thinking { content_index }) =
                                self.open_blocks.get(&index)
                            {
                                let content_index = *content_index;
                                let sig_delta = delta
                                    .get("signature")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if let Some(AssistantContentBlock::Thinking(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    let sig = tc
                                        .thinking_signature
                                        .get_or_insert_with(String::new);
                                    sig.push_str(sig_delta);
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(AnthropicOpenBlock::ToolCall {
                                content_index,
                                json_buffer,
                            }) = self.open_blocks.get_mut(&index)
                            {
                                let content_index = *content_index;
                                let partial_json = delta
                                    .get("partial_json")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                json_buffer.push_str(&partial_json);
                                sender.push(AssistantMessageEvent::ToolCallDelta {
                                    content_index,
                                    delta: partial_json,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(open) = self.open_blocks.remove(&index) {
                    match open {
                        AnthropicOpenBlock::Text { content_index } => {
                            let content = if let Some(AssistantContentBlock::Text(tc)) =
                                self.output.content.get(content_index)
                            {
                                tc.text.clone()
                            } else {
                                String::new()
                            };
                            sender.push(AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                partial: self.output.clone(),
                            });
                        }
                        AnthropicOpenBlock::Thinking { content_index } => {
                            let content = if let Some(AssistantContentBlock::Thinking(tc)) =
                                self.output.content.get(content_index)
                            {
                                tc.thinking.clone()
                            } else {
                                String::new()
                            };
                            sender.push(AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content,
                                partial: self.output.clone(),
                            });
                        }
                        AnthropicOpenBlock::ToolCall {
                            content_index,
                            json_buffer,
                        } => {
                            let mut tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get(content_index)
                            {
                                tc.clone()
                            } else {
                                ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: serde_json::Map::new(),
                                    arguments_error: None,
                                    thought_signature: None,
                                }
                            };

                            if json_buffer.is_empty() {
                                tool_call.arguments = serde_json::Map::new();
                            } else {
                                match serde_json::from_str::<serde_json::Value>(&json_buffer) {
                                    Ok(serde_json::Value::Object(map)) => {
                                        tool_call.arguments = map;
                                    }
                                    Ok(val) => {
                                        let mut map = serde_json::Map::new();
                                        map.insert("input".to_string(), val);
                                        tool_call.arguments = map;
                                    }
                                    Err(e) => {
                                        tool_call.arguments_error = Some(e.to_string());
                                    }
                                }
                            }

                            if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get_mut(content_index)
                            {
                                *tc = tool_call.clone();
                            }

                            sender.push(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = json.get("delta") {
                    if let Some(stop_reason_str) =
                        delta.get("stop_reason").and_then(|v| v.as_str())
                    {
                        self.output.stop_reason = match stop_reason_str {
                            "end_turn" => StopReason::Stop,
                            "max_tokens" => StopReason::Length,
                            "tool_use" => StopReason::ToolUse,
                            "stop_sequence" => StopReason::Stop,
                            "refusal" => {
                                self.output.error_message =
                                    Some("The model refused to complete the request".to_string());
                                StopReason::Error
                            }
                            _ => StopReason::Stop,
                        };
                    }
                }
                if let Some(usage) = json.get("usage") {
                    self.update_usage(usage);
                }
            }
            "message_stop" => {
                if !self.done {
                    self.done = true;
                    sender.push(AssistantMessageEvent::Done {
                        reason: self.output.stop_reason,
                        message: self.output.clone(),
                    });
                }
            }
            "error" => {
                self.output.stop_reason = StopReason::Error;
                let msg = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Anthropic API error");
                self.output.error_message = Some(msg.to_string());
                self.done = true;
                sender.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: self.output.clone(),
                });
            }
            _ => {}
        }
    }

    /// Closes any unclosed content blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        // Close any trailing open blocks
        let open_indices: Vec<usize> = self.open_blocks.keys().copied().collect();
        for index in open_indices {
            if let Some(open) = self.open_blocks.remove(&index) {
                match open {
                    AnthropicOpenBlock::Text { content_index } => {
                        let content = if let Some(AssistantContentBlock::Text(tc)) =
                            self.output.content.get(content_index)
                        {
                            tc.text.clone()
                        } else {
                            String::new()
                        };
                        sender.push(AssistantMessageEvent::TextEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    AnthropicOpenBlock::Thinking { content_index } => {
                        let content = if let Some(AssistantContentBlock::Thinking(tc)) =
                            self.output.content.get(content_index)
                        {
                            tc.thinking.clone()
                        } else {
                            String::new()
                        };
                        sender.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    AnthropicOpenBlock::ToolCall {
                        content_index,
                        json_buffer,
                    } => {
                        let mut tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                            self.output.content.get(content_index)
                        {
                            tc.clone()
                        } else {
                            ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: serde_json::Map::new(),
                                arguments_error: None,
                                thought_signature: None,
                            }
                        };

                        if !json_buffer.is_empty() {
                            match serde_json::from_str::<serde_json::Value>(&json_buffer) {
                                Ok(serde_json::Value::Object(map)) => {
                                    tool_call.arguments = map;
                                }
                                Ok(val) => {
                                    let mut map = serde_json::Map::new();
                                    map.insert("input".to_string(), val);
                                    tool_call.arguments = map;
                                }
                                Err(e) => {
                                    tool_call.arguments_error = Some(e.to_string());
                                }
                            }
                        }

                        if let Some(AssistantContentBlock::ToolCall(tc)) =
                            self.output.content.get_mut(content_index)
                        {
                            *tc = tool_call.clone();
                        }

                        sender.push(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial: self.output.clone(),
                        });
                    }
                }
            }
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

    fn update_usage(&mut self, usage: &serde_json::Value) {
        if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.output.usage.input = input;
        }
        if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            self.output.usage.output = output;
        }
        if let Some(cache_read) = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
            self.output.usage.cache_read = cache_read;
        }
        if let Some(cache_write) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
            self.output.usage.cache_write = cache_write;
        }
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
        self.output.usage.calculate_cost(&self.model.cost);
    }
}

/// Pure state-machine processor converting Anthropic SSE events into an AssistantMessage.
pub fn process_anthropic_sse_events(
    model: &Model,
    events: &[SseEvent],
    sender: &AssistantEventSender,
) -> AssistantMessage {
    let mut state = AnthropicStreamState::new(model);
    state.process_events(events, sender);
    state.finish(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model() -> Model {
        Model {
            id: "claude-3-7-sonnet-20250219".to_string(),
            name: "Claude 3.7 Sonnet".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
        }
    }

    #[test]
    fn test_build_anthropic_request() {
        let model = test_model();
        let mut params = serde_json::Map::new();
        params.insert("type".to_string(), serde_json::json!("object"));

        let context = Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Hello Claude".to_string()),
                    timestamp: 1000,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Hi there!".to_string(),
                        text_signature: None,
                    })],
                    api: "anthropic-messages".to_string(),
                    provider: "anthropic".to_string(),
                    model: "claude-3-7-sonnet-20250219".to_string(),
                    response_id: None,
                    usage: Usage {
                        input: 10,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                        total_tokens: 15,
                        cost: UsageCost {
                            input: 0.0,
                            output: 0.0,
                            cache_read: 0.0,
                            cache_write: 0.0,
                            total: 0.0,
                        },
                    },
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 1001,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("What's the weather?".to_string()),
                    timestamp: 1002,
                }),
            ],
            tools: Some(vec![ToolDefinition {
                name: "get_weather".to_string(),
                description: "Get weather for location".to_string(),
                parameters: serde_json::Value::Object(params),
            }]),
        };

        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            temperature: None,
            max_tokens: Some(4096),
            reasoning: Some(ThinkingLevel::Medium),
            session_id: None,
            headers: None,
        };

        let req = build_anthropic_request(&model, &context, &options);

        assert_eq!(req["model"], "claude-3-7-sonnet-20250219");
        assert_eq!(req["stream"], true);
        assert_eq!(req["system"], "You are a helpful assistant.");
        assert_eq!(req["thinking"]["type"], "enabled");
        assert_eq!(req["thinking"]["budget_tokens"], 8192);
        // Effective max_tokens should be expanded to budget + 1024 if needed
        assert!(req["max_tokens"].as_u64().unwrap() >= 8192 + 1024);

        let tools = req["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
        assert!(tools[0].get("input_schema").is_some());

        let msgs = req["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn test_process_anthropic_sse_events_thinking_text_tool_use() {
        let model = test_model();
        let (sender, _stream) = create_event_stream();

        let events = vec![
            SseEvent {
                event: Some("message_start".to_string()),
                data: serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_01X9Z",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-3-7-sonnet-20250219",
                        "content": [],
                        "stop_reason": null,
                        "usage": {
                            "input_tokens": 120,
                            "output_tokens": 1,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0
                        }
                    }
                })
                .to_string(),
            },
            // 1. Thinking block
            SseEvent {
                event: Some("content_block_start".to_string()),
                data: serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "thinking",
                        "thinking": ""
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": "User wants weather."
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "signature_delta",
                        "signature": "sig_abc123"
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_stop".to_string()),
                data: serde_json::json!({
                    "type": "content_block_stop",
                    "index": 0
                })
                .to_string(),
            },
            // 2. Text block
            SseEvent {
                event: Some("content_block_start".to_string()),
                data: serde_json::json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {
                        "type": "text_delta",
                        "text": "Checking the weather for you."
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_stop".to_string()),
                data: serde_json::json!({
                    "type": "content_block_stop",
                    "index": 1
                })
                .to_string(),
            },
            // 3. Tool use block with chunked input_json_delta
            SseEvent {
                event: Some("content_block_start".to_string()),
                data: serde_json::json!({
                    "type": "content_block_start",
                    "index": 2,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_01ABC",
                        "name": "get_weather",
                        "input": {}
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"location\":"
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": " \"San Francisco\"}"
                    }
                })
                .to_string(),
            },
            SseEvent {
                event: Some("content_block_stop".to_string()),
                data: serde_json::json!({
                    "type": "content_block_stop",
                    "index": 2
                })
                .to_string(),
            },
            // message_delta
            SseEvent {
                event: Some("message_delta".to_string()),
                data: serde_json::json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "tool_use",
                        "stop_sequence": null
                    },
                    "usage": {
                        "output_tokens": 42
                    }
                })
                .to_string(),
            },
            // message_stop
            SseEvent {
                event: Some("message_stop".to_string()),
                data: serde_json::json!({
                    "type": "message_stop"
                })
                .to_string(),
            },
        ];

        let result = process_anthropic_sse_events(&model, &events, &sender);

        assert_eq!(result.response_id.as_deref(), Some("msg_01X9Z"));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.usage.input, 120);
        assert_eq!(result.usage.output, 42);
        assert_eq!(result.content.len(), 3);

        // Block 0: Thinking
        match &result.content[0] {
            AssistantContentBlock::Thinking(tc) => {
                assert_eq!(tc.thinking, "User wants weather.");
                assert_eq!(tc.thinking_signature.as_deref(), Some("sig_abc123"));
            }
            _ => panic!("Expected Thinking content block at index 0"),
        }

        // Block 1: Text
        match &result.content[1] {
            AssistantContentBlock::Text(tc) => {
                assert_eq!(tc.text, "Checking the weather for you.");
            }
            _ => panic!("Expected Text content block at index 1"),
        }

        // Block 2: Tool call
        match &result.content[2] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "toolu_01ABC");
                assert_eq!(tc.name, "get_weather");
                assert_eq!(tc.arguments["location"], "San Francisco");
            }
            _ => panic!("Expected ToolCall content block at index 2"),
        }
    }
}
