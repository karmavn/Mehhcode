//! OpenAI Responses API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry, RetryPolicy,
    DEFAULT_STREAM_CHUNK_TIMEOUT,
};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::streaming_json::ToolCallArgumentsStream;
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
            client: default_http_client(),
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
            let url = if base_url.ends_with("/responses") {
                base_url.to_string()
            } else {
                format!("{base_url}/responses")
            };

            let resp = match send_with_retry(
                || {
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
                    req.json(&body)
                },
                &RetryPolicy::default(),
                &cancel,
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
            let mut state = OpenAiResponsesStreamState::new(&model);

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
                                sender.send_batch(batch).await;
                                if state.done {
                                    break;
                                }
                            }
                            Ok(Some(Err(e))) => {
                                state.mark_error(e.to_string());
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
                state.mark_error("Stream ended prematurely before OpenAI Responses completion");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let truncated = if sanitized.len() > 64 {
        &sanitized[..64]
    } else {
        &sanitized
    };
    truncated.trim_end_matches('_').to_string()
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", crate::transform_messages::short_hash(item_id));
    if normalized.len() > 64 {
        normalized[..64].to_string()
    } else {
        normalized
    }
}

pub fn normalize_responses_tool_call_id(
    id: &str,
    target_model: &Model,
    source: &AssistantMessage,
) -> String {
    const ALLOWED_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode", "azure"];
    if !ALLOWED_PROVIDERS.contains(&target_model.provider.as_str()) {
        return normalize_id_part(id);
    }
    if !id.contains('|') {
        return normalize_id_part(id);
    }
    let (call_id, item_id) = id.split_once('|').unwrap();
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign = source.provider != target_model.provider || source.api != target_model.api;
    let mut normalized_item_id = if is_foreign {
        build_foreign_responses_item_id(item_id)
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

/// Builds the JSON request payload for OpenAI Responses API.
pub fn build_openai_responses_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::transform_messages;
    use crate::unicode::sanitize_surrogates;

    let mut instructions_parts = Vec::new();
    if let Some(sys) = &context.system_prompt {
        if !sys.is_empty() {
            instructions_parts.push(sanitize_surrogates(sys));
        }
    }

    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, m: &Model, a: &AssistantMessage| normalize_responses_tool_call_id(id, m, a)),
    );

    for msg in &transformed_messages {
        if let Message::System(sys) = msg {
            if !sys.content.is_empty() {
                instructions_parts.push(sanitize_surrogates(&sys.content));
            }
        }
    }

    let mut input_items: Vec<serde_json::Value> = Vec::new();

    for msg in &transformed_messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let content: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => {
                        vec![serde_json::json!({
                            "type": "input_text",
                            "text": sanitize_surrogates(text),
                        })]
                    }
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({
                                    "type": "input_text",
                                    "text": sanitize_surrogates(&tc.text),
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
                let is_same_provider_and_api = a.provider == model.provider && a.api == model.api;
                let is_different_model = is_same_provider_and_api && a.model != model.id;

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
                                "summary": [{ "type": "summary_text", "text": sanitize_surrogates(&th.thinking) }],
                            }));
                        }
                        AssistantContentBlock::Text(tc) => {
                            input_items.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": sanitize_surrogates(&tc.text),
                                }],
                                "status": "completed",
                            }));
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            let (call_id, item_id_raw) = if let Some((c, i)) = tc.id.split_once('|') {
                                (c.to_string(), Some(i.to_string()))
                            } else {
                                (tc.id.clone(), None)
                            };

                            let mut item_id = item_id_raw;
                            if is_different_model || !item_id.as_deref().map(|id| id.starts_with("fc_")).unwrap_or(false) {
                                item_id = None;
                            }

                            let mut obj = serde_json::json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            });
                            if let Some(id) = item_id {
                                obj.as_object_mut().unwrap().insert("id".to_string(), serde_json::json!(id));
                            }
                            input_items.push(obj);
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                let call_id = if let Some((c, _)) = tr.tool_call_id.split_once('|') {
                    c
                } else {
                    &tr.tool_call_id
                };
                let has_image = tr
                    .content
                    .iter()
                    .any(|b| matches!(b, ToolResultContentBlock::Image(_)));
                let output = if has_image {
                    let mut parts: Vec<serde_json::Value> = Vec::new();
                    for b in &tr.content {
                        match b {
                            ToolResultContentBlock::Text(tc) => parts.push(serde_json::json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(&tc.text),
                            })),
                            ToolResultContentBlock::Image(img) => parts.push(serde_json::json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                            })),
                        }
                    }
                    serde_json::Value::Array(parts)
                } else {
                    let text = tr
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ToolResultContentBlock::Text(tc) => Some(sanitize_surrogates(&tc.text)),
                            ToolResultContentBlock::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    serde_json::Value::String(text)
                };
                input_items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
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
        stream: Box<ToolCallArgumentsStream>,
    },
}

/// Streaming state machine processing OpenAI Responses SSE chunks.
pub struct OpenAiResponsesStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    pub done: bool,
    pub received_terminal: bool,
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
            received_terminal: false,
            slots: HashMap::new(),
        }
    }

    /// Marks the stream as aborted so `finish_into` emits an aborted terminal event.
    pub fn mark_aborted(&mut self, msg: &str) {
        self.output.stop_reason = StopReason::Aborted;
        self.output.error_message = Some(msg.to_string());
    }

    /// Marks the stream as failed so `finish_into` emits an error terminal event.
    pub fn mark_error(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Error;
        self.output.error_message = Some(msg.into());
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
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "[DONE]" {
            self.received_terminal = true;
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        if let Some(err_val) = json.get("error") {
            self.received_terminal = true;
            self.output.stop_reason = StopReason::Error;
            let err_msg = err_val
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err_val.as_str())
                .or_else(|| json.get("message").and_then(|m| m.as_str()))
                .unwrap_or("OpenAI Responses error");
            self.output.error_message = Some(err_msg.to_string());
            self.finish_into(events);
            return;
        }

        let ev_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
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
                            events.push(AssistantMessageEvent::ThinkingStart {
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
                            events.push(AssistantMessageEvent::TextStart {
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
                                    stream: Box::new(ToolCallArgumentsStream::new()),
                                },
                            );
                            events.push(AssistantMessageEvent::ToolCallStart {
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
                        events.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: ci,
                            delta: delta.to_string(),
                            partial: self.output.metadata_clone(),
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
                        events.push(AssistantMessageEvent::TextDelta {
                            content_index: ci,
                            delta: delta.to_string(),
                            partial: self.output.metadata_clone(),
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
                        stream,
                        ..
                    }) = self.slots.get_mut(&output_index)
                    {
                        let ci = *content_index;
                        let args = stream.append(delta);
                        if let Some(AssistantContentBlock::ToolCall(tc)) =
                            self.output.content.get_mut(ci)
                        {
                            tc.arguments = args;
                        }
                        events.push(AssistantMessageEvent::ToolCallDelta {
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
                    if let Some(OutputSlot::ToolCall { content_index, stream }) =
                        self.slots.get_mut(&output_index)
                    {
                        let delta = if args_str.starts_with(stream.text()) {
                            let d = &args_str[stream.text().len()..];
                            if !d.is_empty() {
                                Some(d.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let args = stream.replace(args_str);
                        if let Some(AssistantContentBlock::ToolCall(tc)) =
                            self.output.content.get_mut(*content_index)
                        {
                            tc.arguments = args;
                        }
                        if let Some(d) = delta {
                            events.push(AssistantMessageEvent::ToolCallDelta {
                                content_index: *content_index,
                                delta: d,
                                partial: self.output.clone(),
                            });
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
                            events.push(AssistantMessageEvent::ThinkingEnd {
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
                            events.push(AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                partial: self.output.clone(),
                            });
                        }
                        OutputSlot::ToolCall {
                            content_index,
                            mut stream,
                        } => {
                            if let Some(args_str) =
                                item.and_then(|i| i.get("arguments")).and_then(|v| v.as_str())
                            {
                                if !args_str.is_empty() {
                                    stream.replace(args_str);
                                }
                            }
                            let (args, err) = stream.finish();
                            let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get_mut(content_index)
                            {
                                tc.arguments = args;
                                tc.arguments_error = err;
                                tc.clone()
                            } else {
                                ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: args,
                                    arguments_error: err,
                                    thought_signature: None,
                                }
                            };

                            events.push(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "response.completed" => {
                self.received_terminal = true;
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
                            .unwrap_or_else(|| input.saturating_add(output));
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

                        self.output.usage.input =
                            input.saturating_sub(cached.saturating_add(cache_write));
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
                self.finish_into(events);
            }
            "response.incomplete" => {
                self.received_terminal = true;
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
                            .unwrap_or_else(|| input.saturating_add(output));
                        self.output.usage.input = input;
                        self.output.usage.output = output;
                        self.output.usage.total_tokens = total;
                        self.output.usage.calculate_cost(&self.model.cost);
                    }
                }
                self.output.stop_reason = StopReason::Length;
                self.finish_into(events);
            }
            "response.failed" | "error" => {
                self.received_terminal = true;
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
                self.finish_into(events);
            }
            _ => {}
        }
    }

    /// Closes any remaining active blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut batch = Vec::new();
        let out = self.finish_into(&mut batch);
        for ev in batch {
            sender.push(ev);
        }
        out
    }

    /// Closes any remaining active blocks and appends the terminal completion event to `events`.
    pub fn finish_into(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
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
                        events.push(AssistantMessageEvent::ThinkingEnd {
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
                        events.push(AssistantMessageEvent::TextEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    OutputSlot::ToolCall {
                        content_index,
                        stream,
                    } => {
                        let (args, err) = stream.finish();
                        let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                            self.output.content.get_mut(content_index)
                        {
                            tc.arguments = args;
                            tc.arguments_error = err;
                            tc.clone()
                        } else {
                            ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: args,
                                arguments_error: err,
                                thought_signature: None,
                            }
                        };

                        events.push(AssistantMessageEvent::ToolCallEnd {
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
            ..Default::default()
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
                    duration_ms: None,
                    usage: None,
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
            ..Default::default()
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

    #[test]
    fn build_request_normalizes_foreign_fc_item_ids() {
        let model = Model {
            id: "gpt-6.1-sol".to_string(),
            name: "GPT-6.1 Sol".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };

        let copilot_raw_id = "call_4VnzVawQXPB9MgYib7CiQFEY|I9b95oN1wD/cHXKTw3PpRkL6KkCtzTJhUxMouMWYwHeTo2j3htzfSk7YPx2vifiIM4g3A8XXyOj8q4Bt6SLUG7gqY1E3ELkrkVQNHglRfUmWj84lqxJY+Puieb3VKyX0FB+83TUzn91cDMF/4gzt990IzqVrc+nIb9RRscRD070Du16q1glydVjWR0SBJsE6TbY/esOjFpqplogQqrajm1eI++f3eLi73R6q7hVusY0QbeFySVxABCjhN0lXB04caBe1rzHjYzul6MAXj7uq+0r17VLq+yrtyYhN12wkmFqHeqTyEei6EFPbMy24Nc+IbJlkP0OCg02W+gOnyBFcbi2ctvJFSOhSjt1CqBdqCnnhwUqXjbWiT0wh3DmLScRgTHmGkaI+oAcQQjfic65nxj+TnEkReA==";

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Edit file".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: copilot_raw_id.to_string(),
                            name: "edit".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                        // An orphaned tool call without matching result:
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_orphan123|foreign_item_999".to_string(),
                            name: "orphan_tool".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "openai-responses".to_string(),
                    provider: "github-copilot".to_string(),
                    model: "gpt-5.5".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 200,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: copilot_raw_id.to_string(),
                    tool_name: "edit".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "ok".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 300,
                    duration_ms: None,
                    usage: None,
                }),
            ],
            tools: None,
        };

        let req = build_openai_responses_request(&model, &context, &StreamOptions::default());
        let input_items = req["input"].as_array().expect("input array");

        // Find function_call for edit
        let edit_fc = input_items
            .iter()
            .find(|item| item["type"] == "function_call" && item["name"] == "edit")
            .expect("edit function call");
        assert_eq!(edit_fc["call_id"], "call_4VnzVawQXPB9MgYib7CiQFEY");
        let item_id = edit_fc["id"].as_str().expect("id field present on function_call");
        assert!(item_id.starts_with("fc_"), "item id must start with fc_, got {}", item_id);
        assert!(item_id.len() <= 64, "item id must be <= 64 chars");

        // Find function_call_output for edit
        let edit_output = input_items
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_4VnzVawQXPB9MgYib7CiQFEY")
            .expect("edit function call output");
        assert_eq!(edit_output["output"], "ok");

        // Orphan tool call: must have a synthetic function_call_output with "No result provided"
        let orphan_output = input_items
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call_orphan123")
            .expect("orphan synthetic function call output");
        assert_eq!(orphan_output["output"], "No result provided");
    }

    #[test]
    fn openai_responses_sse_updates_partial_tool_arguments_incrementally() {
        let model = Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            ..Default::default()
        };
        let mut state = OpenAiResponsesStreamState::new(&model);
        let mut events = Vec::new();

        let chunk1 = serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_001",
                "call_id": "call_123",
                "name": "search"
            }
        })
        .to_string();
        state.process_chunk_into(&chunk1, &mut events);

        events.clear();

        let chunk2 = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": r#"{"query":"hel"#
        })
        .to_string();
        state.process_chunk_into(&chunk2, &mut events);

        assert_eq!(events.len(), 1);
        match &events[0] {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(delta, r#"{"query":"hel"#);
                match &partial.content[0] {
                    AssistantContentBlock::ToolCall(tc) => {
                        assert_eq!(
                            tc.arguments.get("query").and_then(|v| v.as_str()),
                            Some("hel"),
                            "ToolCallDelta partial AssistantMessage must reflect parsed partial arguments incrementally"
                        );
                    }
                    _ => panic!("Expected ToolCall at index 0"),
                }
            }
            _ => panic!("Expected ToolCallDelta event"),
        }

        events.clear();

        let chunk3 = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": r#"lo"}"#
        })
        .to_string();
        state.process_chunk_into(&chunk3, &mut events);

        assert_eq!(events.len(), 1);
        match &events[0] {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(delta, r#"lo"}"#);
                match &partial.content[0] {
                    AssistantContentBlock::ToolCall(tc) => {
                        assert_eq!(
                            tc.arguments.get("query").and_then(|v| v.as_str()),
                            Some("hello"),
                            "ToolCallDelta partial AssistantMessage must reflect parsed arguments incrementally"
                        );
                    }
                    _ => panic!("Expected ToolCall at index 0"),
                }
            }
            _ => panic!("Expected ToolCallDelta event"),
        }
    }

    #[test]
    fn test_openai_responses_error_chunk() {
        let model = test_responses_model();
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "error": {
                    "message": "Model is overloaded"
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
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("Model is overloaded"));
    }

    #[tokio::test]
    async fn responses_url_with_trailing_slash_requests_responses_once() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send(path);
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let mut model = test_responses_model();
        model.base_url = format!("http://{addr}/v1/responses/");
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Hello".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };
        let provider = OpenAiResponsesProvider::new();
        let stream = provider.stream(
            &model,
            &context,
            &StreamOptions::default(),
            tokio_util::sync::CancellationToken::new(),
        );

        let path = tokio::time::timeout(Duration::from_secs(3), rx)
            .await
            .expect("request did not reach the server")
            .expect("path sender dropped");
        assert_eq!(path, "/v1/responses");
        drop(stream);
    }

    #[test]
    fn openai_responses_forwards_tool_result_images() {
        let model = test_responses_model();
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Read the screenshot".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_read_1|fc_read_1".to_string(),
                        name: "read".to_string(),
                        arguments: {
                            let mut m = serde_json::Map::new();
                            m.insert("path".to_string(), serde_json::json!("screenshot.png"));
                            m
                        },
                        arguments_error: None,
                        thought_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 2,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_read_1|fc_read_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ToolResultContentBlock::Image(ImageContent {
                        mime_type: "image/png".to_string(),
                        data: "iVBORw0KGgo=".to_string(),
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

        let req = build_openai_responses_request(&model, &context, &StreamOptions::default());
        let input = req["input"].as_array().expect("input array");
        let output_item = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("function_call_output item");

        assert_eq!(output_item["call_id"], "call_read_1");
        let parts = output_item["output"].as_array().expect(
            "image-only tool result must serialize output as content parts, not an empty string",
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "input_image");
        assert_eq!(parts[0]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
    }

    #[test]
    fn response_usage_saturates_overflowing_totals() {
        let model = test_responses_model();
        let (sender, _stream) = create_event_stream();
        // total_tokens is omitted to exercise input + output, and cached +
        // cache_write overflows independently. Both must saturate in debug.
        let data = serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_overflow",
                "status": "completed",
                "usage": {
                    "input_tokens": u64::MAX,
                    "output_tokens": 1,
                    "input_tokens_details": {
                        "cached_tokens": u64::MAX,
                        "cache_write_tokens": u64::MAX
                    }
                }
            }
        })
        .to_string();
        let events = vec![SseEvent { event: None, data }];
        let result = process_openai_responses_sse_events(&model, &events, &sender);
        assert_eq!(result.usage.input, 0);
        assert_eq!(result.usage.total_tokens, u64::MAX);
    }
}
