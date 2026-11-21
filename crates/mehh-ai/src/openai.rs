//! OpenAI Chat Completions API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::retry::{
    default_http_client, send_with_retry, RetryFailure, RetryPolicy, DEFAULT_STREAM_CHUNK_TIMEOUT,
};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::*;

/// Provider for OpenAI Chat Completions API and compatible endpoints.
pub struct OpenAiCompletionsProvider {
    client: reqwest::Client,
}

impl Default for OpenAiCompletionsProvider {
    fn default() -> Self {
        Self::new(default_http_client())
    }
}

impl OpenAiCompletionsProvider {
    /// Creates a new OpenAI completions provider with the given HTTP client.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiCompletionsProvider {
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
            let body = build_openai_request(&model, &context, &options);
            let url = if model.base_url.ends_with("/chat/completions") {
                model.base_url.clone()
            } else {
                format!("{}/chat/completions", model.base_url.trim_end_matches('/'))
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
                Err(RetryFailure::Cancelled) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Aborted;
                    err_msg.error_message = Some("Request was cancelled".to_string());
                    sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
                Err(RetryFailure::HttpStatus { status, body }) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(format!("HTTP {status}: {body}"));
                    sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
                Err(RetryFailure::Transport(e)) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(e);
                    sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };

            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = SseParser::new();
            let mut state = OpenAiStreamState::new(&model);

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
                                for ev in &events {
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
            for ev in &remaining {
                state.process_chunk_into(&ev.data, &mut batch);
            }
            if sse_parser.has_overflowed() {
                state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
            } else if sse_parser.has_pending_bytes() || !state.received_terminal {
                state.mark_error("Stream ended prematurely without completion marker");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

/// Builds the JSON request payload for OpenAI Chat Completions API.
fn normalize_openai_tool_call_id(id: &str, model: &Model) -> String {
    if id.contains('|') {
        let separator_index = id.find('|').unwrap();
        let call_id: String = id[..separator_index]
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let item_id: String = id[separator_index + 1..]
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let combined_id = if !item_id.is_empty() {
            format!("{call_id}_{item_id}")
        } else {
            call_id.clone()
        };
        if combined_id.len() <= 40 {
            return combined_id;
        }
        let hash = crate::transform_messages::short_hash(id);
        let hash_prefix = if hash.len() >= 8 { &hash[..8] } else { &hash };
        let max_prefix_len = (40usize.saturating_sub(hash_prefix.len() + 1)).max(1);
        let prefix = if call_id.len() > max_prefix_len {
            &call_id[..max_prefix_len]
        } else {
            &call_id
        };
        return format!("{prefix}_{hash_prefix}");
    }

    // Every provider routing through the OpenAI Completions schema enforces
    // the same 40-character id limit, not just `provider == "openai"`.
    let needs_trim = matches!(
        model.provider.as_str(),
        "openai" | "openrouter" | "github-copilot" | "azure" | "azure-openai-responses"
    );
    if needs_trim && id.chars().count() > 40 {
        let hash = crate::transform_messages::short_hash(id);
        let hash_prefix: String = hash.chars().take(8).collect();
        let keep = 40usize.saturating_sub(hash_prefix.chars().count() + 1).max(1);
        let prefix: String = id.chars().take(keep).collect();
        return format!("{prefix}_{hash_prefix}");
    }
    id.to_string()
}

pub fn build_openai_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::transform_messages;
    use crate::unicode::sanitize_surrogates;

    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, m: &Model, _a: &AssistantMessage| normalize_openai_tool_call_id(id, m)),
    );

    let mut messages: Vec<serde_json::Value> = Vec::new();

    if let Some(sys_prompt) = &context.system_prompt {
        if !sys_prompt.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": sanitize_surrogates(sys_prompt),
            }));
        }
    }

    for msg in &transformed_messages {
        match msg {
            Message::System(sys) => {
                messages.push(serde_json::json!({
                    "role": "system",
                    "content": sanitize_surrogates(&sys.content),
                }));
            }
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": sanitize_surrogates(text),
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let json_blocks: Vec<serde_json::Value> = blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(&tc.text),
                                })
                            }
                            UserContentBlock::Image(img) => {
                                serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", img.mime_type, img.data),
                                    },
                                })
                            }
                        })
                        .collect();
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": json_blocks,
                    }));
                }
            },
            Message::Assistant(a) => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            text_parts.push(sanitize_surrogates(&tc.text));
                        }
                        AssistantContentBlock::Thinking(_) => {}
                        AssistantContentBlock::ToolCall(tc) => {
                            tool_calls.push(serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                },
                            }));
                        }
                    }
                }

                let mut msg_obj = serde_json::Map::new();
                msg_obj.insert("role".to_string(), serde_json::json!("assistant"));
                if !text_parts.is_empty() {
                    msg_obj.insert(
                        "content".to_string(),
                        serde_json::json!(text_parts.join("\n")),
                    );
                } else if tool_calls.is_empty() {
                    msg_obj.insert("content".to_string(), serde_json::json!(""));
                } else {
                    msg_obj.insert("content".to_string(), serde_json::Value::Null);
                }

                if !tool_calls.is_empty() {
                    msg_obj.insert(
                        "tool_calls".to_string(),
                        serde_json::Value::Array(tool_calls),
                    );
                }

                messages.push(serde_json::Value::Object(msg_obj));
            }
            Message::ToolResult(tr) => {
                let mut text_content = String::new();
                let mut image_parts: Vec<serde_json::Value> = Vec::new();
                for block in &tr.content {
                    match block {
                        ToolResultContentBlock::Text(tc) => {
                            if !text_content.is_empty() {
                                text_content.push('\n');
                            }
                            text_content.push_str(&sanitize_surrogates(&tc.text));
                        }
                        ToolResultContentBlock::Image(img) => {
                            image_parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.mime_type, img.data)
                                }
                            }));
                        }
                    }
                }
                let content = if image_parts.is_empty() {
                    serde_json::Value::String(text_content)
                } else {
                    let mut parts = vec![serde_json::json!({
                        "type": "text", "text": text_content
                    })];
                    parts.extend(image_parts);
                    serde_json::Value::Array(parts)
                };
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "content": content,
                }));
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("model".to_string(), serde_json::json!(model.id));
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));
    req_map.insert("stream".to_string(), serde_json::json!(true));
    if model
        .compat
        .as_ref()
        .and_then(|c| c.include_usage)
        .unwrap_or(true)
    {
        req_map.insert(
            "stream_options".to_string(),
            serde_json::json!({ "include_usage": true }),
        );
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let tool_vals: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        },
                    })
                })
                .collect();
            req_map.insert("tools".to_string(), serde_json::Value::Array(tool_vals));
        }
    }

    let is_reasoning = model.reasoning
        || model.id.starts_with("o1")
        || model.id.starts_with("o3")
        || model.id.starts_with("o4");

    if !is_reasoning {
        if let Some(temp) = options.temperature {
            req_map.insert("temperature".to_string(), serde_json::json!(temp));
        }
    }

    if let Some(max_tokens) = options.max_tokens {
        if is_reasoning {
            req_map.insert(
                "max_completion_tokens".to_string(),
                serde_json::json!(max_tokens),
            );
        } else {
            req_map.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
        }
    }

    if is_reasoning {
        if let Some(level) = &options.reasoning {
            if *level != ThinkingLevel::Off {
                let effort = match level {
                    ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
                    ThinkingLevel::Off => "low",
                };
                req_map.insert("reasoning_effort".to_string(), serde_json::json!(effort));
            }
        }
    }

    serde_json::Value::Object(req_map)
}

/// Internal struct tracking a streaming tool call being assembled.
struct ActiveToolCall {
    content_index: usize,
    id: String,
    name: String,
    stream: ToolCallArgumentsStream,
}

/// Streaming state machine processing OpenAI SSE chunks.
pub struct OpenAiStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    received_terminal: bool,
    active_thinking_index: Option<usize>,
    active_text_index: Option<usize>,
    tool_calls_by_index: HashMap<usize, ActiveToolCall>,
}

impl OpenAiStreamState {
    /// Creates a new streaming state initialized for the model.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            received_terminal: false,
            active_thinking_index: None,
            active_text_index: None,
            tool_calls_by_index: HashMap::new(),
        }
    }

    /// Marks the stream state as failed with an error message before finalizing.
    pub fn mark_error(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Error;
        self.output.error_message = Some(msg.into());
    }

    /// Marks the stream state as aborted before finalizing.
    pub fn mark_aborted(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Aborted;
        self.output.error_message = Some(msg.into());
    }

    /// Processes a raw SSE data payload string (e.g. JSON chunk or `[DONE]`).
    pub fn process_chunk(&mut self, data: &str, sender: &AssistantEventSender) {
        let mut out = Vec::new();
        self.process_chunk_into(data, &mut out);
        for ev in out {
            sender.push(ev);
        }
    }

    /// Processes a raw SSE data payload string and appends emitted events into `out`.
    pub fn process_chunk_into(&mut self, data: &str, out: &mut Vec<AssistantMessageEvent>) {
        let trimmed = data.trim();
        if trimmed == "[DONE]" {
            self.received_terminal = true;
            self.finish_into(out);
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        if let Some(err_obj) = json.get("error") {
            self.received_terminal = true;
            self.output.stop_reason = StopReason::Error;
            let msg = err_obj
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err_obj.as_str())
                .unwrap_or("OpenAI API error");
            self.output.error_message = Some(msg.to_string());
            self.finish_into(out);
            return;
        }

        if self.output.response_id.is_none() {
            if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                self.output.response_id = Some(id.to_string());
            }
        }

        if !self.started {
            self.started = true;
            out.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        if let Some(usage) = json.get("usage") {
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            self.output.usage.cache_read = cached;
            // OpenAI's prompt_tokens already includes cached_tokens. Bill the
            // non-cached portion at the input rate to avoid double counting.
            if let Some(prompt) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                self.output.usage.input = prompt.saturating_sub(cached);
            }
            if let Some(comp) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.output.usage.output = comp;
            }
            if let Some(total) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                self.output.usage.total_tokens = total;
            } else {
                self.output.usage.total_tokens =
                    self.output.usage.input + self.output.usage.output + cached;
            }
            self.output.usage.calculate_cost(&self.model.cost);
        }

        if let Some(choice) = json.get("choices").and_then(|c| c.get(0)) {
            if let Some(finish_reason_str) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                self.received_terminal = true;
                self.output.stop_reason = match finish_reason_str {
                    "stop" | "end" => StopReason::Stop,
                    "length" => StopReason::Length,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "content_filter" => {
                        self.output.error_message =
                            Some("Provider finish_reason: content_filter".to_string());
                        StopReason::Error
                    }
                    _ => StopReason::Stop,
                };
            }

            if let Some(delta) = choice.get("delta") {
                // 1. Reasoning content
                let reasoning_opt = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .or_else(|| delta.get("reasoning_text"))
                    .and_then(|v| v.as_str());

                if let Some(reasoning) = reasoning_opt {
                    if !reasoning.is_empty() {
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
                                out.push(AssistantMessageEvent::ThinkingStart {
                                    content_index: idx,
                                    partial: self.output.clone(),
                                });
                                idx
                            }
                        };
                        if let Some(AssistantContentBlock::Thinking(tc)) =
                            self.output.content.get_mut(content_index)
                        {
                            tc.thinking.push_str(reasoning);
                        }
                        out.push(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: reasoning.to_string(),
                            partial: self.output.metadata_clone(),
                        });
                    }
                }

                // 2. Text content
                if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                    if !content.is_empty() {
                        // Close any active thinking block before emitting text
                        if let Some(thinking_idx) = self.active_thinking_index.take() {
                            let thinking_content = if let Some(AssistantContentBlock::Thinking(tc)) =
                                self.output.content.get(thinking_idx)
                            {
                                tc.thinking.clone()
                            } else {
                                String::new()
                            };
                            out.push(AssistantMessageEvent::ThinkingEnd {
                                content_index: thinking_idx,
                                content: thinking_content,
                                partial: self.output.clone(),
                            });
                        }

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
                                out.push(AssistantMessageEvent::TextStart {
                                    content_index: idx,
                                    partial: self.output.clone(),
                                });
                                idx
                            }
                        };
                        if let Some(AssistantContentBlock::Text(tc)) =
                            self.output.content.get_mut(content_index)
                        {
                            tc.text.push_str(content);
                        }
                        out.push(AssistantMessageEvent::TextDelta {
                            content_index,
                            delta: content.to_string(),
                            partial: self.output.metadata_clone(),
                        });
                    }
                }

                // 3. Tool calls
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    // Close thinking or text before tool calls begin
                    if let Some(thinking_idx) = self.active_thinking_index.take() {
                        let thinking_content = if let Some(AssistantContentBlock::Thinking(tc)) =
                            self.output.content.get(thinking_idx)
                        {
                            tc.thinking.clone()
                        } else {
                            String::new()
                        };
                        out.push(AssistantMessageEvent::ThinkingEnd {
                            content_index: thinking_idx,
                            content: thinking_content,
                            partial: self.output.clone(),
                        });
                    }
                    if let Some(text_idx) = self.active_text_index.take() {
                        let text_content = if let Some(AssistantContentBlock::Text(tc)) =
                            self.output.content.get(text_idx)
                        {
                            tc.text.clone()
                        } else {
                            String::new()
                        };
                        out.push(AssistantMessageEvent::TextEnd {
                            content_index: text_idx,
                            content: text_content,
                            partial: self.output.clone(),
                        });
                    }

                    for tc_chunk in tool_calls {
                        let idx = tc_chunk
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;

                        let (content_index, args_delta) = if let Some(active) =
                            self.tool_calls_by_index.get_mut(&idx)
                        {
                            if let Some(id) = tc_chunk.get("id").and_then(|v| v.as_str()) {
                                active.id = id.to_string();
                                if let Some(AssistantContentBlock::ToolCall(tc)) =
                                    self.output.content.get_mut(active.content_index)
                                {
                                    tc.id = id.to_string();
                                }
                            }
                            if let Some(name) = tc_chunk
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                            {
                                active.name.push_str(name);
                                if let Some(AssistantContentBlock::ToolCall(tc)) =
                                    self.output.content.get_mut(active.content_index)
                                {
                                    tc.name = active.name.clone();
                                }
                            }
                            let delta = tc_chunk
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = active.stream.append(&delta);
                            if let Some(AssistantContentBlock::ToolCall(tc)) =
                                self.output.content.get_mut(active.content_index)
                            {
                                tc.arguments = args;
                            }
                            (active.content_index, delta)
                        } else {
                            let id = tc_chunk
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = tc_chunk
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let delta = tc_chunk
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            let mut stream = ToolCallArgumentsStream::new();
                            let args = if !delta.is_empty() {
                                stream.append(&delta)
                            } else {
                                serde_json::Map::new()
                            };

                            self.output
                                .content
                                .push(AssistantContentBlock::ToolCall(ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: args,
                                    arguments_error: None,
                                    thought_signature: None,
                                }));
                            let content_index = self.output.content.len() - 1;
                            self.tool_calls_by_index.insert(
                                idx,
                                ActiveToolCall {
                                    content_index,
                                    id,
                                    name,
                                    stream,
                                },
                            );
                            out.push(AssistantMessageEvent::ToolCallStart {
                                content_index,
                                partial: self.output.clone(),
                            });
                            (content_index, delta)
                        };

                        if !args_delta.is_empty() {
                            out.push(AssistantMessageEvent::ToolCallDelta {
                                content_index,
                                delta: args_delta,
                                partial: self.output.metadata_clone_with_block(content_index),
                            });
                        }
                    }
                }
            }
        }
    }

    /// Closes any remaining active blocks and appends the terminal completion event into `out`.
    pub fn finish_into(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
        if let Some(thinking_idx) = self.active_thinking_index.take() {
            let content = if let Some(AssistantContentBlock::Thinking(tc)) =
                self.output.content.get(thinking_idx)
            {
                tc.thinking.clone()
            } else {
                String::new()
            };
            out.push(AssistantMessageEvent::ThinkingEnd {
                content_index: thinking_idx,
                content,
                partial: self.output.clone(),
            });
        }

        if let Some(text_idx) = self.active_text_index.take() {
            let content = if let Some(AssistantContentBlock::Text(tc)) =
                self.output.content.get(text_idx)
            {
                tc.text.clone()
            } else {
                String::new()
            };
            out.push(AssistantMessageEvent::TextEnd {
                content_index: text_idx,
                content,
                partial: self.output.clone(),
            });
        }

        let mut indices: Vec<usize> = self.tool_calls_by_index.keys().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            if let Some(active) = self.tool_calls_by_index.remove(&idx) {
                let mut tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) =
                    self.output.content.get(active.content_index)
                {
                    tc.clone()
                } else {
                    ToolCall {
                        id: active.id.clone(),
                        name: active.name.clone(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    }
                };

                let (args, err) = active.stream.finish();
                tool_call.arguments = args;
                tool_call.arguments_error = err;

                if let Some(AssistantContentBlock::ToolCall(tc)) =
                    self.output.content.get_mut(active.content_index)
                {
                    *tc = tool_call.clone();
                }

                out.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: active.content_index,
                    tool_call,
                    partial: self.output.clone(),
                });
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
            if matches!(self.output.stop_reason, StopReason::Error | StopReason::Aborted) {
                out.push(AssistantMessageEvent::Error {
                    reason: self.output.stop_reason,
                    error: self.output.clone(),
                });
            } else {
                out.push(AssistantMessageEvent::Done {
                    reason: self.output.stop_reason,
                    message: self.output.clone(),
                });
            }
        }

        self.output.clone()
    }

    /// Closes any remaining active blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut out = Vec::new();
        let res = self.finish_into(&mut out);
        for ev in out {
            sender.push(ev);
        }
        res
    }
}

/// Pure processor converting OpenAI SSE events into an AssistantMessage.
pub fn process_openai_sse_events(
    model: &Model,
    events: &[SseEvent],
    sender: &AssistantEventSender,
) -> AssistantMessage {
    let mut state = OpenAiStreamState::new(model);
    for event in events {
        state.process_chunk(&event.data, sender);
    }
    state.finish(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model() -> Model {
        Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.5,
                output: 10.0,
                cache_read: 1.25,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_build_openai_request() {
        let model = test_model();
        let mut params = serde_json::Map::new();
        params.insert("type".to_string(), serde_json::json!("object"));

        let context = Context {
            system_prompt: Some("You are an expert assistant.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Calculate 42 * 2".to_string()),
                    timestamp: 1000,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_calc1".to_string(),
                        name: "calculator".to_string(),
                        arguments: {
                            let mut m = serde_json::Map::new();
                            m.insert("expr".to_string(), serde_json::json!("42 * 2"));
                            m
                        },
                        arguments_error: None,
                        thought_signature: None,
                    })],
                    api: "openai-completions".to_string(),
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                    response_id: Some("chatcmpl-1".to_string()),
                    usage: Usage {
                        input: 20,
                        output: 10,
                        cache_read: 0,
                        cache_write: 0,
                        total_tokens: 30,
                        cost: UsageCost {
                            input: 0.0,
                            output: 0.0,
                            cache_read: 0.0,
                            cache_write: 0.0,
                            total: 0.0,
                        },
                    },
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 1001,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_calc1".to_string(),
                    tool_name: "calculator".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "84".to_string(),
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
                name: "calculator".to_string(),
                description: "Evaluates expressions".to_string(),
                parameters: serde_json::Value::Object(params),
            }]),
        };

        let options = StreamOptions {
            api_key: Some("sk-test".to_string()),
            temperature: Some(0.7),
            max_tokens: Some(2048),
            reasoning: Some(ThinkingLevel::High),
            session_id: None,
            headers: None,
            ..Default::default()
        };

        let req = build_openai_request(&model, &context, &options);

        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert_eq!(req["stream_options"]["include_usage"], true);
        assert_eq!(req["temperature"], 0.7);
        assert_eq!(req["max_tokens"], 2048);
        assert!(req.get("max_completion_tokens").is_none());
        assert!(req.get("reasoning_effort").is_none());

        let tools = req["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "calculator");

        let msgs = req["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_calc1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_calc1");
        assert_eq!(msgs[3]["content"], "84");
    }

    #[test]
    fn test_build_openai_request_o3_reasoning() {
        let mut model = test_model();
        model.id = "o3-mini".to_string();
        model.reasoning = true;
        let context = Context::default();
        let options = StreamOptions {
            temperature: Some(0.7),
            max_tokens: Some(4096),
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let req = build_openai_request(&model, &context, &options);
        assert_eq!(req["model"], "o3-mini");
        assert!(req.get("temperature").is_none(), "reasoning model must omit temperature");
        assert_eq!(req["max_completion_tokens"], 4096);
        assert!(req.get("max_tokens").is_none());
        assert_eq!(req["reasoning_effort"], "high");
    }

    #[test]
    fn test_process_openai_sse_events_text_and_tool_call() {
        let model = test_model();
        let (sender, _stream) = create_event_stream();

        let chunks = vec![
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "I will check the "
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": "weather for you."
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
            // Tool call streamed in chunks
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_weather99",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": ""
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "{\"city\":"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": " \"Boston\"}"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
            serde_json::json!({
                "id": "chatcmpl-test01",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 55,
                    "completion_tokens": 28,
                    "total_tokens": 83,
                    "prompt_tokens_details": {
                        "cached_tokens": 10
                    }
                }
            })
            .to_string(),
            "[DONE]".to_string(),
        ];

        let sse_events: Vec<SseEvent> = chunks
            .into_iter()
            .map(|data| SseEvent {
                event: None,
                data,
            })
            .collect();

        let result = process_openai_sse_events(&model, &sse_events, &sender);

        assert_eq!(result.response_id.as_deref(), Some("chatcmpl-test01"));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        // prompt_tokens (55) includes 10 cached tokens; only 45 are billed as input.
        assert_eq!(result.usage.input, 45);
        assert_eq!(result.usage.output, 28);
        assert_eq!(result.usage.total_tokens, 83);
        assert_eq!(result.usage.cache_read, 10);
        assert_eq!(result.content.len(), 2);

        // Block 0: Text
        match &result.content[0] {
            AssistantContentBlock::Text(tc) => {
                assert_eq!(tc.text, "I will check the weather for you.");
            }
            _ => panic!("Expected Text block at index 0"),
        }

        // Block 1: Tool call
        match &result.content[1] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "call_weather99");
                assert_eq!(tc.name, "get_weather");
                assert_eq!(tc.arguments["city"], "Boston");
            }
            _ => panic!("Expected ToolCall block at index 1"),
        }
    }

    #[test]
    fn build_request_normalizes_pipe_ids_to_40_chars() {
        let model = Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };

        let pipe_id = "call_pAYbIr76hXIjncD9UE4eGfnS|t5nnb2qYMFWGSsr13fhCd1CaCu3t3qONEPuOudu4HSVEtA8YJSL6FAZUxvoOoD792VIJWl91g87EdqsCWp9krVsdBysQoDaf9lMCLb8BS4EYi4gQd5kBQBYLlgD71PYwvf+TbMD9J9/5OMD42oxSRj8H+vRf78/l2Xla33LWz4nOgsddBlbvabICRs8GHt5C9PK5keFtzyi3lsyVKNlfduK3iphsZqs4MLv4zyGJnvZo/+QzShyk5xnMSQX/f98+aEoNflEApCdEOXipipgeiNWnpFSHbcwmMkZoJhURNu+JEz3xCh1mrXeYoN5o+trLL3IXJacSsLYXDrYTipZZbJFRPAucgbnjYBC+/ZzJOfkwCs+Gkw7EoZR7ZQgJ8ma+9586n4tT4cI8DEhBSZsWMjrCt8dxKg==";

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Run echo".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: pipe_id.to_string(),
                        name: "echo".to_string(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    })],
                    api: "openai-responses".to_string(),
                    provider: "github-copilot".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 200,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: pipe_id.to_string(),
                    tool_name: "echo".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "hello".to_string(),
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

        let req = build_openai_request(&model, &context, &StreamOptions::default());
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);

        let assistant_tc = &messages[1]["tool_calls"][0];
        let normalized_id = assistant_tc["id"].as_str().unwrap();
        assert!(normalized_id.len() <= 40, "Normalized ID must be <= 40 chars, got {}", normalized_id.len());
        assert!(normalized_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));

        // Matching tool result in message 2 must use the same normalized ID
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], normalized_id);
    }

    #[test]
    fn test_normalize_openai_tool_call_id_non_ascii_utf8_truncation() {
        let model = test_model();
        // 39 ASCII characters + 4-byte UTF-8 character '🦀' (bytes 39..43) + additional characters
        let mut foreign_id = "a".repeat(39);
        foreign_id.push('🦀');
        foreign_id.push_str("extra_tail_after_40_chars");

        // Prior to fix, &id[..40] panicked with: byte index 40 is not a char boundary; it is inside '🦀'
        let normalized = normalize_openai_tool_call_id(&foreign_id, &model);
        // Long ids are truncated to 40 chars with a hash suffix to avoid collisions.
        assert_eq!(normalized.chars().count(), 40);
        assert!(normalized.starts_with(&"a".repeat(30)));
        assert!(normalized.contains('_'));
    }

    #[test]
    fn openai_sse_updates_partial_tool_arguments_incrementally() {
        let model = test_model();
        let mut state = OpenAiStreamState::new(&model);
        let mut events = Vec::new();

        // Chunk 1: Tool call start with initial partial argument chunk
        let chunk1 = serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": r#"{"query":"hel"#
                        }
                    }]
                }
            }]
        })
        .to_string();

        state.process_chunk_into(&chunk1, &mut events);

        // Find the ToolCallDelta event
        let delta1 = events.iter().find(|ev| matches!(ev, AssistantMessageEvent::ToolCallDelta { .. }));
        assert!(delta1.is_some(), "Expected ToolCallDelta event from initial chunk");
        match delta1.unwrap() {
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
            _ => unreachable!(),
        }

        events.clear();

        // Chunk 2: Second delta completing the JSON argument
        let chunk2 = serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": r#"lo"}"#
                        }
                    }]
                }
            }]
        })
        .to_string();

        state.process_chunk_into(&chunk2, &mut events);
        let delta2 = events.iter().find(|ev| matches!(ev, AssistantMessageEvent::ToolCallDelta { .. }));
        assert!(delta2.is_some(), "Expected ToolCallDelta event from second chunk");
        match delta2.unwrap() {
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
                            "ToolCallDelta partial AssistantMessage must reflect updated arguments before finish"
                        );
                    }
                    _ => panic!("Expected ToolCall at index 0"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_openai_error_chunk() {
        let model = test_model();
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "error": {
                    "message": "Rate limit exceeded: 500 requests per minute",
                    "type": "requests",
                    "code": "rate_limit_exceeded"
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

        let result = process_openai_sse_events(&model, &sse_events, &sender);
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Rate limit exceeded: 500 requests per minute")
        );
    }
}
