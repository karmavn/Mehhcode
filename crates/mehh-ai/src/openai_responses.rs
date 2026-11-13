//! OpenAI Responses API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::types::*;

/// Provider for the OpenAI Responses API.
pub struct OpenAiResponsesProvider {
    client: reqwest::Client,
}

impl OpenAiResponsesProvider {
    /// Creates a new OpenAI Responses provider with default HTTP client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new OpenAI Responses provider with the given HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for OpenAiResponsesProvider {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiResponsesProvider {
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
            let body = build_openai_responses_request(&model, &context, &options);
            let base_url = if model.base_url.is_empty() {
                "https://api.openai.com/v1"
            } else {
                model.base_url.trim_end_matches('/')
            };
            let url = if model.base_url.ends_with("/responses") {
                model.base_url.clone()
            } else {
                format!("{base_url}/responses")
            };

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");

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
            let mut state = OpenAiResponsesStreamState::new(&model);

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

/// Builds the JSON request payload for OpenAI Responses API.
pub fn build_openai_responses_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    let mut instructions_parts = Vec::new();
    if let Some(sys) = &context.system_prompt {
        if !sys.is_empty() {
            instructions_parts.push(sys.clone());
        }
    }
    for msg in &context.messages {
        if let Message::System(sys) = msg {
            if !sys.content.is_empty() {
                instructions_parts.push(sys.content.clone());
            }
        }
    }

    let mut input_items: Vec<serde_json::Value> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let content: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => {
                        vec![serde_json::json!({
                            "type": "input_text",
                            "text": text,
                        })]
                    }
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({
                                    "type": "input_text",
                                    "text": tc.text,
                                })
                            }
                            UserContentBlock::Image(img) => {
                                serde_json::json!({
                                    "type": "input_image",
                                    "detail": "auto",
                                    "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                                })
                            }
                        })
                        .collect(),
                };
                if !content.is_empty() {
                    input_items.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Thinking(th) => {
                            if let Some(sig) = &th.thinking_signature {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(sig) {
                                    input_items.push(val);
                                    continue;
                                }
                            }
                            input_items.push(serde_json::json!({
                                "type": "reasoning",
                                "summary": [{ "type": "summary_text", "text": th.thinking }],
                            }));
                        }
                        AssistantContentBlock::Text(tc) => {
                            input_items.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": tc.text,
                                }],
                                "status": "completed",
                            }));
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            let call_id = tc.id.split('|').next().unwrap_or(&tc.id);
                            input_items.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            }));
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                let call_id = tr.tool_call_id.split('|').next().unwrap_or(&tr.tool_call_id);
                let text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ToolResultContentBlock::Text(tc) => Some(tc.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                input_items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text,
                }));
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("model".to_string(), serde_json::json!(model.id));
    req_map.insert("stream".to_string(), serde_json::json!(true));
    req_map.insert("store".to_string(), serde_json::json!(false));

    if !instructions_parts.is_empty() {
        req_map.insert(
            "instructions".to_string(),
            serde_json::json!(instructions_parts.join("\n\n")),
        );
    }

    req_map.insert("input".to_string(), serde_json::Value::Array(input_items));

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let tool_vals: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            req_map.insert("tools".to_string(), serde_json::Value::Array(tool_vals));
        }
    }

    if let Some(max_tokens) = options.max_tokens {
        req_map.insert(
            "max_output_tokens".to_string(),
            serde_json::json!(max_tokens),
        );
    }

    if !model.reasoning {
        if let Some(temp) = options.temperature {
            req_map.insert("temperature".to_string(), serde_json::json!(temp));
        }
    } else if let Some(level) = options.reasoning {
        if level != ThinkingLevel::Off {
            let effort = match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
                ThinkingLevel::Off => "low",
            };
            req_map.insert(
                "reasoning".to_string(),
                serde_json::json!({ "effort": effort }),
            );
        }
    }

    serde_json::Value::Object(req_map)
}

enum OutputSlot {
    Thinking {
        content_index: usize,
        id: String,
    },
    Text {
        content_index: usize,
        id: String,
    },
    ToolCall {
        content_index: usize,
        arguments_buf: String,
    },
}

/// Streaming state machine processing OpenAI Responses SSE chunks.
pub struct OpenAiResponsesStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    slots: HashMap<usize, OutputSlot>,
}

impl OpenAiResponsesStreamState {
    /// Creates a new OpenAI Responses stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            slots: HashMap::new(),
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

        let ev_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if !self.started {
            self.started = true;
            sender.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        match ev_type {
            "response.created" => {
                if let Some(id) = json
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(|v| v.as_str())
                {
                    self.output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if let Some(item) = json.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let item_id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    match item_type {
                        "reasoning" => {
                            self.output
                                .content
                                .push(AssistantContentBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                    redacted: None,
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.slots.insert(
                                output_index,
                                OutputSlot::Thinking {
                                    content_index,
                                    id: item_id,
                                },
                            );
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        "message" => {
                            self.output
                                .content
                                .push(AssistantContentBlock::Text(TextContent {
                                    text: String::new(),
                                    text_signature: None,
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.slots.insert(
                                output_index,
                                OutputSlot::Text {
                                    content_index,
                                    id: item_id,
                                },
                            );
                            sender.push(AssistantMessageEvent::TextStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                        }
                        "function_call" => {
                            let call_id = item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let id = if !item_id.is_empty() && !call_id.is_empty() {
                                format!("{call_id}|{item_id}")
                            } else if !call_id.is_empty() {
                                call_id
                            } else {
                                item_id
                            };
                            let tc = ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: serde_json::Map::new(),
                                arguments_error: None,
                                thought_signature: None,
                            };
                            self.output.content.push(AssistantContentBlock::ToolCall(tc));
                            let content_index = self.output.content.len() - 1;
                            self.slots.insert(
                                output_index,
                                OutputSlot::ToolCall {
                                    content_index,
                                    arguments_buf: String::new(),
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
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = json.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    if let Some(OutputSlot::Thinking { content_index, .. }) =
                        self.slots.get(&output_index)
                    {
                        let ci = *content_index;
                        if let Some(AssistantContentBlock::Thinking(tc)) =
                            self.output.content.get_mut(ci)
                        {
                            tc.thinking.push_str(delta);
                        }
                        sender.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: ci,
                            delta: delta.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = json.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    if let Some(OutputSlot::Text { content_index, .. }) =
                        self.slots.get(&output_index)
                    {
                        let ci = *content_index;
                        if let Some(AssistantContentBlock::Text(tc)) =
                            self.output.content.get_mut(ci)
                        {
                            tc.text.push_str(delta);
                        }
                        sender.push(AssistantMessageEvent::TextDelta {
                            content_index: ci,
                            delta: delta.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let delta = json.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    if let Some(OutputSlot::ToolCall {
                        content_index,
                        arguments_buf,
                        ..
                    }) = self.slots.get_mut(&output_index)
                    {
                        let ci = *content_index;
                        arguments_buf.push_str(delta);
                        sender.push(AssistantMessageEvent::ToolCallDelta {
                            content_index: ci,
                            delta: delta.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if let Some(args_str) = json.get("arguments").and_then(|v| v.as_str()) {
                    if let Some(OutputSlot::ToolCall { arguments_buf, .. }) =
                        self.slots.get_mut(&output_index)
                    {
                        if !args_str.is_empty() {
                            *arguments_buf = args_str.to_string();
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let output_index = json
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let item = json.get("item");
                if let Some(slot) = self.slots.remove(&output_index) {
                    match slot {
                        OutputSlot::Thinking { content_index, id } => {
                            if let Some(item_obj) = item {
                                if let Some(AssistantContentBlock::Thinking(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    if tc.thinking.is_empty() {
                                        if let Some(summary_arr) =
                                            item_obj.get("summary").and_then(|s| s.as_array())
                                        {
                                            let s_text = summary_arr
                                                .iter()
                                                .filter_map(|p| {
                                                    p.get("text").and_then(|v| v.as_str())
                                                })
                                                .collect::<Vec<_>>()
                                                .join("\n\n");
                                            if !s_text.is_empty() {
                                                tc.thinking = s_text;
                                            }
                                        }
                                    }
                                    tc.thinking_signature =
                                        Some(serde_json::to_string(item_obj).unwrap_or(id));
                                }
                            }
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
                        OutputSlot::Text { content_index, id } => {
                            if let Some(item_obj) = item {
                                if let Some(AssistantContentBlock::Text(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    if tc.text.is_empty() {
                                        if let Some(content_arr) =
                                            item_obj.get("content").and_then(|c| c.as_array())
                                        {
                                            let t_text = content_arr
                                                .iter()
                                                .filter_map(|p| {
                                                    p.get("text").and_then(|v| v.as_str())
                                                })
                                                .collect::<Vec<_>>()
                                                .join("");
                                            if !t_text.is_empty() {
                                                tc.text = t_text;
                                            }
                                        }
                                    }
                                    tc.text_signature = Some(id);
                                }
                            }
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
                        OutputSlot::ToolCall {
                            content_index,
                            arguments_buf,
                            ..
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

                            let args_to_parse = if let Some(args_str) =
                                item.and_then(|i| i.get("arguments")).and_then(|v| v.as_str())
                            {
                                if !args_str.is_empty() {
                                    args_str
                                } else {
                                    &arguments_buf
                                }
                            } else {
                                &arguments_buf
                            };

                            if args_to_parse.is_empty() {
                                tool_call.arguments = serde_json::Map::new();
                            } else {
                                match serde_json::from_str::<serde_json::Value>(args_to_parse) {
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
            "response.completed" => {
                if let Some(resp) = json.get("response") {
                    if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                        self.output.response_id = Some(id.to_string());
                    }
                    if let Some(usage) = resp.get("usage") {
                        let input = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let total = usage
                            .get("total_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(input + output);
                        let cached = usage
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_write = usage
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cache_write_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);

                        self.output.usage.input = input.saturating_sub(cached + cache_write);
                        self.output.usage.output = output;
                        self.output.usage.cache_read = cached;
                        self.output.usage.cache_write = cache_write;
                        self.output.usage.total_tokens = total;
                        self.output.usage.calculate_cost(&self.model.cost);
                    }
                }
                if self
                    .output
                    .content
                    .iter()
                    .any(|b| matches!(b, AssistantContentBlock::ToolCall(_)))
                {
                    self.output.stop_reason = StopReason::ToolUse;
                } else {
                    self.output.stop_reason = StopReason::Stop;
                }
            }
            "response.incomplete" => {
                if let Some(resp) = json.get("response") {
                    if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                        self.output.response_id = Some(id.to_string());
                    }
                    if let Some(usage) = resp.get("usage") {
                        let input = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let total = usage
                            .get("total_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(input + output);
                        self.output.usage.input = input;
                        self.output.usage.output = output;
                        self.output.usage.total_tokens = total;
                        self.output.usage.calculate_cost(&self.model.cost);
                    }
                }
                self.output.stop_reason = StopReason::Length;
            }
            "response.failed" | "error" => {
                self.output.stop_reason = StopReason::Error;
                let err_msg = json
                    .get("error")
                    .and_then(|e| {
                        e.get("message")
                            .and_then(|m| m.as_str())
                            .or_else(|| e.as_str())
                    })
                    .or_else(|| json.get("message").and_then(|m| m.as_str()))
                    .unwrap_or("OpenAI Responses error");
                self.output.error_message = Some(err_msg.to_string());
            }
            _ => {}
        }
    }

    /// Closes any remaining active blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut indices: Vec<usize> = self.slots.keys().copied().collect();
        indices.sort_unstable();

        for idx in indices {
            if let Some(slot) = self.slots.remove(&idx) {
                match slot {
                    OutputSlot::Thinking { content_index, .. } => {
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
                    OutputSlot::Text { content_index, .. } => {
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
                    OutputSlot::ToolCall {
                        content_index,
                        arguments_buf,
                        ..
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

                        if !arguments_buf.is_empty() {
                            if let Ok(serde_json::Value::Object(map)) =
                                serde_json::from_str::<serde_json::Value>(&arguments_buf)
                            {
                                tool_call.arguments = map;
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

        if self.output.stop_reason == StopReason::Stop
            && self
                .output
                .content
                .iter()
                .any(|b| matches!(b, AssistantContentBlock::ToolCall(_)))
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

/// Pure processor converting OpenAI Responses SSE events into an AssistantMessage.
pub fn process_openai_responses_sse_events(
    model: &Model,
    events: &[SseEvent],
    sender: &AssistantEventSender,
) -> AssistantMessage {
    let mut state = OpenAiResponsesStreamState::new(model);
    for event in events {
        state.process_chunk(&event.data, sender);
    }
    state.finish(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_responses_model() -> Model {
        Model {
            id: "gpt-5-turbo".to_string(),
            name: "GPT-5 Turbo".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 1.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
        }
    }

    #[test]
    fn test_build_openai_responses_request() {
        let model = test_responses_model();
        let mut params = serde_json::Map::new();
        params.insert("type".to_string(), serde_json::json!("object"));

        let context = Context {
            system_prompt: Some("You are an expert mathematician.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Factor 120".to_string()),
                    timestamp: 1000,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::Thinking(ThinkingContent {
                            thinking: "Factoring 120 gives 2^3 * 3 * 5".to_string(),
                            thinking_signature: None,
                            redacted: None,
                        }),
                        AssistantContentBlock::Text(TextContent {
                            text: "I will factor it now.".to_string(),
                            text_signature: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_fact_1|fc_item_1".to_string(),
                            name: "factorize".to_string(),
                            arguments: {
                                let mut m = serde_json::Map::new();
                                m.insert("n".to_string(), serde_json::json!(120));
                                m
                            },
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "openai-responses".to_string(),
                    provider: "openai".to_string(),
                    model: "gpt-5-turbo".to_string(),
                    response_id: Some("resp_001".to_string()),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 1001,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_fact_1|fc_item_1".to_string(),
                    tool_name: "factorize".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "2^3 * 3 * 5".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 1002,
                }),
            ],
            tools: Some(vec![ToolDefinition {
                name: "factorize".to_string(),
                description: "Factors an integer".to_string(),
                parameters: serde_json::Value::Object(params),
            }]),
        };

        let options = StreamOptions {
            api_key: Some("sk-responses-key".to_string()),
            temperature: Some(0.7), // reasoning model should omit temperature
            max_tokens: Some(1024),
            reasoning: Some(ThinkingLevel::High),
            session_id: None,
            headers: None,
        };

        let req = build_openai_responses_request(&model, &context, &options);

        assert_eq!(req["model"], "gpt-5-turbo");
        assert_eq!(req["stream"], true);
        assert_eq!(req["store"], false);
        assert_eq!(req["instructions"], "You are an expert mathematician.");
        assert_eq!(req["max_output_tokens"], 1024);
        assert!(req.get("temperature").is_none());
        assert_eq!(req["reasoning"]["effort"], "high");

        let input = req["input"].as_array().expect("input array");
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "Factor 120");

        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "call_fact_1");
        assert_eq!(input[3]["name"], "factorize");

        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_fact_1");
        assert_eq!(input[4]["output"], "2^3 * 3 * 5");

        let tools = req["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "factorize");
    }

    #[test]
    fn test_process_openai_responses_sse_events() {
        let model = test_responses_model();
        let (sender, _stream) = create_event_stream();

        let chunks = vec![
            serde_json::json!({
                "type": "response.created",
                "response": { "id": "resp_test_abc" }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "reasoning", "id": "rs_001" }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.reasoning_text.delta",
                "output_index": 0,
                "delta": "Thinking through math."
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "id": "rs_001",
                    "summary": [{ "type": "summary_text", "text": "Thinking through math." }]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": { "type": "message", "id": "msg_001" }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 1,
                "delta": "The prime factors are:"
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {
                    "type": "message",
                    "id": "msg_001",
                    "content": [{ "type": "output_text", "text": "The prime factors are:" }]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 2,
                "item": {
                    "type": "function_call",
                    "id": "fc_001",
                    "call_id": "call_fact_1",
                    "name": "verify"
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 2,
                "delta": "{\"valid\": true}"
            })
            .to_string(),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "function_call",
                    "id": "fc_001",
                    "call_id": "call_fact_1",
                    "name": "verify",
                    "arguments": "{\"valid\": true}"
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_test_abc",
                    "status": "completed",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 40,
                        "total_tokens": 140,
                        "input_tokens_details": {
                            "cached_tokens": 20,
                            "cache_write_tokens": 0
                        }
                    }
                }
            })
            .to_string(),
        ];

        let sse_events: Vec<SseEvent> = chunks
            .into_iter()
            .map(|data| SseEvent {
                event: None,
                data,
            })
            .collect();

        let result = process_openai_responses_sse_events(&model, &sse_events, &sender);

        assert_eq!(result.response_id.as_deref(), Some("resp_test_abc"));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.usage.input, 80); // 100 - 20
        assert_eq!(result.usage.output, 40);
        assert_eq!(result.usage.cache_read, 20);
        assert_eq!(result.usage.total_tokens, 140);
        assert_eq!(result.content.len(), 3);

        match &result.content[0] {
            AssistantContentBlock::Thinking(th) => {
                assert_eq!(th.thinking, "Thinking through math.");
            }
            _ => panic!("Expected Thinking block at 0"),
        }

        match &result.content[1] {
            AssistantContentBlock::Text(tc) => {
                assert_eq!(tc.text, "The prime factors are:");
            }
            _ => panic!("Expected Text block at 1"),
        }

        match &result.content[2] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "call_fact_1|fc_001");
                assert_eq!(tc.name, "verify");
                assert_eq!(tc.arguments["valid"], true);
            }
            _ => panic!("Expected ToolCall block at 2"),
        }
    }
}
