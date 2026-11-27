//! OpenAI Chat Completions API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::oauth::redact_error_body_with_limit;
use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry_with_secrets, RetryPolicy,
    DEFAULT_STREAM_CHUNK_TIMEOUT,
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

pub(crate) fn insert_default_copilot_headers(
    header_map: &mut reqwest::header::HeaderMap,
    provider: &str,
) {
    if provider.eq_ignore_ascii_case("github-copilot") {
        header_map.insert(
            reqwest::header::HeaderName::from_static("copilot-integration-id"),
            reqwest::header::HeaderValue::from_static("vscode-chat"),
        );
        header_map.insert(
            reqwest::header::HeaderName::from_static("editor-version"),
            reqwest::header::HeaderValue::from_static("vscode/1.99.0"),
        );
        header_map.insert(
            reqwest::header::HeaderName::from_static("editor-plugin-version"),
            reqwest::header::HeaderValue::from_static("copilot-chat/0.26.0"),
        );
        header_map.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("GitHubCopilotChat/0.26.0"),
        );
    }
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

fn openai_chat_completions_url(base_url: &str) -> String {
    const DEFAULT_BASE: &str = "https://api.openai.com/v1";
    let path = crate::http_safety::normalized_endpoint_path(base_url, DEFAULT_BASE);
    let extra: &[&str] = if path.ends_with("/chat/completions") {
        &[]
    } else {
        &["chat", "completions"]
    };
    crate::http_safety::append_endpoint_segments_preserving_query(base_url, DEFAULT_BASE, extra)
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
        let options = options.clone();
        let body_res = serde_json::to_vec(&build_openai_request(&model, context, &options))
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
            let url = openai_chat_completions_url(&model.base_url);

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
            insert_default_copilot_headers(&mut header_map, &model.provider);
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
            let mut state = OpenAiStreamState::new(&model);
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
                                for ev in &events {
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
    if let Some((raw_call_id, raw_item_id)) = id.split_once('|') {
        let call_id: String = raw_call_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let item_id: String = raw_item_id
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

fn expects_reasoning_content(model: &Model) -> bool {
    let provider = model.provider.trim();
    let id = model.id.trim().to_ascii_lowercase();
    let bare_id = id.rsplit('/').next().unwrap_or(&id);
    provider.eq_ignore_ascii_case("deepseek")
        || bare_id == "deepseek-reasoner"
        || bare_id.starts_with("deepseek-reasoner-")
}

fn supports_reasoning_effort(model: &Model) -> bool {
    let provider = model.provider.trim();
    provider.eq_ignore_ascii_case("openai")
        || provider.eq_ignore_ascii_case("openai-codex")
        || provider.eq_ignore_ascii_case("opencode")
        || provider.eq_ignore_ascii_case("azure")
        || provider.eq_ignore_ascii_case("azure-openai-responses")
        || provider.eq_ignore_ascii_case("github-copilot")
}

pub fn build_openai_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::transform_messages;

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
                "content": crate::unicode::sanitize_surrogates_cow(sys_prompt),
            }));
        }
    }

    let mut pending_tool_images: Vec<serde_json::Value> = Vec::new();

    for msg in &transformed_messages {
        if !matches!(msg, Message::ToolResult(_)) && !pending_tool_images.is_empty() {
            messages.push(serde_json::json!({
                "role": "user",
                "content": std::mem::take(&mut pending_tool_images),
            }));
        }
        match msg {
            Message::System(sys) => {
                messages.push(serde_json::json!({
                    "role": "system",
                    "content": crate::unicode::sanitize_surrogates_cow(&sys.content),
                }));
            }
            Message::User(u) => match &u.content {
                UserContent::Text(text) => {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": crate::unicode::sanitize_surrogates_cow(text),
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let json_blocks: Vec<serde_json::Value> = blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({
                                    "type": "text",
                                    "text": crate::unicode::sanitize_surrogates_cow(&tc.text),
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
                let mut text_content = String::new();
                let mut has_text = false;
                let mut thinking_content = String::new();
                let mut has_thinking = false;
                let mut tool_calls = Vec::new();

                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            if has_text {
                                text_content.push('\n');
                            }
                            text_content.push_str(&crate::unicode::sanitize_surrogates_cow(&tc.text));
                            has_text = true;
                        }
                        AssistantContentBlock::Thinking(th) => {
                            if expects_reasoning_content(model) && !th.thinking.is_empty() {
                                if has_thinking {
                                    thinking_content.push('\n');
                                }
                                thinking_content.push_str(&crate::unicode::sanitize_surrogates_cow(&th.thinking));
                                has_thinking = true;
                            }
                        }
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
                if has_text {
                    msg_obj.insert(
                        "content".to_string(),
                        serde_json::json!(text_content),
                    );
                } else if tool_calls.is_empty() {
                    msg_obj.insert("content".to_string(), serde_json::json!(""));
                } else {
                    msg_obj.insert("content".to_string(), serde_json::Value::Null);
                }

                if has_thinking {
                    msg_obj.insert(
                        "reasoning_content".to_string(),
                        serde_json::json!(thinking_content),
                    );
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
                let mut has_image = false;
                for block in &tr.content {
                    match block {
                        ToolResultContentBlock::Text(tc) => {
                            if !text_content.is_empty() {
                                text_content.push('\n');
                            }
                            text_content.push_str(&crate::unicode::sanitize_surrogates_cow(&tc.text));
                        }
                        ToolResultContentBlock::Image(img) => {
                            has_image = true;
                            pending_tool_images.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.mime_type, img.data)
                                }
                            }));
                        }
                    }
                }
                let content = if text_content.is_empty() && has_image {
                    "(image attached after the tool results)".to_string()
                } else {
                    text_content
                };
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "content": content,
                }));
            }
        }
    }
    if !pending_tool_images.is_empty() {
        messages.push(serde_json::json!({
            "role": "user",
            "content": pending_tool_images,
        }));
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

    let supports_temperature = model
        .compat
        .as_ref()
        .and_then(|c| c.supports_temperature)
        .unwrap_or(!is_reasoning);
    if supports_temperature {
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
        let is_openrouter = model.provider.eq_ignore_ascii_case("openrouter");
        if let Some(level) = &options.reasoning {
            if *level == ThinkingLevel::Off {
                if is_openrouter {
                    req_map.insert(
                        "reasoning".to_string(),
                        serde_json::json!({ "enabled": false }),
                    );
                }
            } else {
                let effort = match level {
                    ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
                    ThinkingLevel::Off => "low",
                };
                if is_openrouter {
                    req_map.insert(
                        "reasoning".to_string(),
                        serde_json::json!({ "effort": effort }),
                    );
                } else if supports_reasoning_effort(model) {
                    req_map.insert("reasoning_effort".to_string(), serde_json::json!(effort));
                }
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
    secrets: Vec<String>,
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

    /// Marks the stream state as failed with an error message before finalizing.
    pub fn mark_error(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Error;
        let raw = msg.into();
        self.output.error_message = Some(self.redact_error(&raw));
    }

    /// Marks the stream state as aborted before finalizing.
    pub fn mark_aborted(&mut self, msg: impl Into<String>) {
        self.output.stop_reason = StopReason::Aborted;
        let raw = msg.into();
        self.output.error_message = Some(self.redact_error(&raw));
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
        if self.done {
            return;
        }
        let trimmed = data.trim();
        if trimmed == "[DONE]" {
            self.received_terminal = true;
            self.finish_into(out);
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };

        if let Some(err_obj) = json.get("error").filter(|v| !v.is_null()) {
            self.received_terminal = true;
            let msg = err_obj
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err_obj.as_str())
                .unwrap_or("OpenAI API error");
            self.mark_error(msg);
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

        if let Some(usage) = json.get("usage").filter(|u| u.is_object()) {
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
                self.output.usage.total_tokens = self
                    .output
                    .usage
                    .input
                    .saturating_add(self.output.usage.output)
                    .saturating_add(cached);
            }
            self.output.usage.calculate_cost(&self.model.cost);
        }

        if let Some(choice) = json.get("choices").and_then(|c| c.get(0)) {
            if let Some(finish_reason_str) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                self.received_terminal = true;
                if !matches!(
                    self.output.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
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
                if let Some(tool_calls) = delta
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .filter(|a| !a.is_empty())
                {
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
                            if let Some(id) = tc_chunk
                                .get("id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
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
                            if let Some(args) = active.stream.append_if_changed(&delta) {
                                if let Some(AssistantContentBlock::ToolCall(tc)) =
                                    self.output.content.get_mut(active.content_index)
                                {
                                    tc.arguments = args.clone();
                                }
                            }
                            (active.content_index, delta)
                        } else {
                            let id = tc_chunk
                                .get("id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
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
                                partial: self.output.clone(),
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
        // 39 ASCII characters + 4-byte UTF-8 character U+1F980 (bytes 39..43) + additional characters
        let mut foreign_id = "a".repeat(39);
        foreign_id.push('\u{1F980}');
        foreign_id.push_str("extra_tail_after_40_chars");

        // Prior to fix, &id[..40] panicked with: byte index 40 is not a char boundary; it is inside U+1F980
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

    #[test]
    fn test_openrouter_reasoning_request_fields() {
        let mut model = test_model();
        model.provider = "openrouter".to_string();
        model.id = "anthropic/claude-sonnet-4.5".to_string();
        model.reasoning = true;
        let ctx = Context::default();

        let opts_high = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let req_high = build_openai_request(&model, &ctx, &opts_high);
        assert_eq!(req_high["reasoning"], serde_json::json!({ "effort": "high" }));
        assert!(req_high.get("reasoning_effort").is_none());

        let opts_off = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let req_off = build_openai_request(&model, &ctx, &opts_off);
        assert_eq!(req_off["reasoning"], serde_json::json!({ "enabled": false }));
        assert!(req_off.get("reasoning_effort").is_none());

        // Non-OpenRouter providers retain `reasoning_effort`
        model.provider = "openai".to_string();
        model.id = "o3".to_string();
        let req_o3 = build_openai_request(&model, &ctx, &opts_high);
        assert_eq!(req_o3["reasoning_effort"], serde_json::json!("high"));
        assert!(req_o3.get("reasoning").is_none());
    }

    #[test]
    fn openai_url_defaults_empty_and_handles_trailing_slash() {
        assert_eq!(
            openai_chat_completions_url(""),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            openai_chat_completions_url("https://api.openai.com/v1/chat/completions/"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn openai_moves_tool_images_out_of_tool_role_into_user_message() {
        let model = test_model();
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Read image".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_img_1".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::Map::new(),
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
                    tool_call_id: "call_img_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ToolResultContentBlock::Image(ImageContent {
                        mime_type: "image/png".to_string(),
                        data: "aGVsbG8=".to_string(),
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

        let req = build_openai_request(&model, &context, &StreamOptions::default());
        let msgs = req["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["role"], "tool");
        assert!(msgs[2]["content"].is_string());
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"][0]["type"], "image_url");
        assert_eq!(
            msgs[3]["content"][0]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn openai_null_error_empty_tool_calls_empty_id_and_multi_block_delta() {
        let model = test_model();
        let mut state = OpenAiStreamState::new(&model);
        let mut events = Vec::new();

        for chunk in [
            serde_json::json!({
                "id": "chatcmpl-1",
                "error": null,
                "usage": null,
                "choices": [{
                    "index": 0,
                    "delta": { "content": "Hello ", "tool_calls": [] }
                }]
            }),
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "world", "tool_calls": [] }
                }]
            }),
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_keep_me",
                            "function": { "name": "ping", "arguments": "{\"a\":" }
                        }]
                    }
                }]
            }),
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "",
                            "function": { "arguments": "1}" }
                        }]
                    }
                }]
            }),
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
                "usage": {
                    "prompt_tokens": u64::MAX,
                    "completion_tokens": 5,
                    "prompt_tokens_details": { "cached_tokens": 0 }
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
                assert!(matches!(partial.content[0], AssistantContentBlock::Text(_)));
            }
            _ => unreachable!(),
        }

        let final_msg = state.finish_into(&mut events);
        assert_eq!(final_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(final_msg.error_message, None);
        assert_eq!(final_msg.usage.total_tokens, u64::MAX);
        assert_eq!(final_msg.content.len(), 2);
        match &final_msg.content[0] {
            AssistantContentBlock::Text(t) => assert_eq!(t.text, "Hello world"),
            other => panic!("Expected single merged Text block, got {other:?}"),
        }
        match &final_msg.content[1] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "call_keep_me");
                assert_eq!(tc.arguments["a"], 1);
            }
            other => panic!("Expected ToolCall block, got {other:?}"),
        }
    }

    #[test]
    fn openai_url_preserves_query_params() {
        assert_eq!(
            openai_chat_completions_url("https://proxy.example.com/v1?api-version=2024"),
            "https://proxy.example.com/v1/chat/completions?api-version=2024"
        );
        assert_eq!(
            openai_chat_completions_url(
                "https://proxy.example.com/v1/chat/completions/?api-version=2024"
            ),
            "https://proxy.example.com/v1/chat/completions?api-version=2024"
        );
    }

    #[test]
    fn openai_ignores_chunks_after_terminal_error_and_preserves_stop_reason() {
        let model = test_model();
        let mut state = OpenAiStreamState::new(&model);
        let mut events = Vec::new();

        state.process_chunk_into(
            &serde_json::json!({ "error": { "message": "Rate limit exceeded" } }).to_string(),
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
                    "index": 0,
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
            Some("Rate limit exceeded")
        );

        // Even before `[DONE]` triggers `done = true`, a subsequent `finish_reason` cannot overwrite `Error`.
        let mut filter_state = OpenAiStreamState::new(&model);
        let mut filter_events = Vec::new();
        filter_state.process_chunk_into(
            &serde_json::json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "content_filter" }]
            })
            .to_string(),
            &mut filter_events,
        );
        filter_state.process_chunk_into(
            &serde_json::json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
            })
            .to_string(),
            &mut filter_events,
        );
        filter_state.process_chunk_into("[DONE]", &mut filter_events);
        assert_eq!(filter_state.output.stop_reason, StopReason::Error);
    }

    #[test]
    fn insert_default_copilot_headers_sets_defaults_and_allows_overrides() {
        let mut headers = reqwest::header::HeaderMap::new();
        insert_default_copilot_headers(&mut headers, "GitHub-Copilot");
        assert_eq!(headers.get("copilot-integration-id").unwrap(), "vscode-chat");
        assert_eq!(headers.get("editor-version").unwrap(), "vscode/1.99.0");
        assert_eq!(
            headers.get("editor-plugin-version").unwrap(),
            "copilot-chat/0.26.0"
        );
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            "GitHubCopilotChat/0.26.0"
        );

        let model_headers = HashMap::from([("Editor-Version".to_string(), "vscode/2.0".to_string())]);
        let opt_headers = HashMap::from([("User-Agent".to_string(), "MyAgent/1.0".to_string())]);
        insert_custom_headers(&mut headers, Some(&model_headers)).unwrap();
        insert_custom_headers(&mut headers, Some(&opt_headers)).unwrap();
        assert_eq!(headers.get("editor-version").unwrap(), "vscode/2.0");
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            "MyAgent/1.0"
        );

        let mut non_copilot = reqwest::header::HeaderMap::new();
        insert_default_copilot_headers(&mut non_copilot, "openai");
        assert!(non_copilot.is_empty());
    }

    #[tokio::test]
    async fn copilot_default_headers_inserted_and_overridden_by_custom_headers() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let raw_req = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = tx.send(raw_req);
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let mut model = test_model();
        model.provider = "github-copilot".to_string();
        model.base_url = format!("http://{addr}/v1");
        model.headers = Some(HashMap::from([(
            "Editor-Version".to_string(),
            "vscode/custom-1.0".to_string(),
        )]));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };
        let options = StreamOptions {
            api_key: Some("ghu_test".to_string()),
            headers: Some(HashMap::from([(
                "User-Agent".to_string(),
                "CustomCopilotAgent/2.0".to_string(),
            )])),
            ..Default::default()
        };

        let provider = OpenAiCompletionsProvider::default();
        let stream = provider.stream(
            &model,
            &context,
            &options,
            tokio_util::sync::CancellationToken::new(),
        );
        let raw_req = tokio::time::timeout(Duration::from_secs(3), rx)
            .await
            .expect("request timeout")
            .expect("request channel");
        drop(stream);

        let lower = raw_req.to_ascii_lowercase();
        assert!(
            lower.contains("copilot-integration-id: vscode-chat\r\n"),
            "missing Copilot-Integration-Id default in:\n{raw_req}"
        );
        assert!(
            lower.contains("editor-plugin-version: copilot-chat/0.26.0\r\n"),
            "missing Editor-Plugin-Version default in:\n{raw_req}"
        );
        assert!(
            lower.contains("editor-version: vscode/custom-1.0\r\n"),
            "model.headers must override Editor-Version in:\n{raw_req}"
        );
        assert!(
            lower.contains("user-agent: customcopilotagent/2.0\r\n"),
            "options.headers must override User-Agent in:\n{raw_req}"
        );
    }

    #[test]
    fn openai_only_sends_reasoning_effort_when_supported() {
        let ctx = Context::default();
        let opts = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };

        for unsupported_provider in ["deepseek", "xai", "together", "fireworks"] {
            let mut m = test_model();
            m.provider = unsupported_provider.to_string();
            m.id = "custom-reasoning-model".to_string();
            m.reasoning = true;
            let req = build_openai_request(&m, &ctx, &opts);
            assert!(
                req.get("reasoning_effort").is_none(),
                "provider '{unsupported_provider}' must not receive reasoning_effort: {req}"
            );
            assert!(
                req.get("reasoning").is_none(),
                "provider '{unsupported_provider}' must not receive reasoning: {req}"
            );
        }

        for supported_provider in ["openai", "azure", "github-copilot"] {
            let mut m = test_model();
            m.provider = supported_provider.to_string();
            m.id = "o3".to_string();
            m.reasoning = true;
            let req = build_openai_request(&m, &ctx, &opts);
            assert_eq!(
                req["reasoning_effort"], "high",
                "provider '{supported_provider}' should receive reasoning_effort"
            );
        }
    }

    #[test]
    fn openai_preserves_thinking_as_reasoning_content_when_expected() {
        let mut model = test_model();
        model.provider = "deepseek".to_string();
        model.id = "deepseek-reasoner".to_string();
        model.reasoning = true;

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Solve it".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::Thinking(ThinkingContent {
                            thinking: "Step 1: check constraints".to_string(),
                            thinking_signature: None,
                            redacted: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_ds_1".to_string(),
                            name: "calc".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
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
                    tool_call_id: "call_ds_1".to_string(),
                    tool_name: "calc".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "42".to_string(),
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

        let req = build_openai_request(&model, &context, &StreamOptions::default());
        let msgs = req["messages"].as_array().expect("messages array");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(
            msgs[1]["reasoning_content"], "Step 1: check constraints",
            "deepseek-reasoner must preserve Thinking as reasoning_content: {req}"
        );
    }

    #[test]
    fn openai_honors_compat_supports_temperature_on_reasoning_model() {
        let mut model = test_model();
        model.id = "o3-custom".to_string();
        model.reasoning = true;
        model.compat = Some(ModelCompat {
            supports_temperature: Some(true),
            ..Default::default()
        });
        let opts = StreamOptions {
            temperature: Some(0.6),
            ..Default::default()
        };
        let req = build_openai_request(&model, &Context::default(), &opts);
        assert_eq!(req["temperature"], 0.6);

        // Explicit false suppresses temperature even on non-reasoning models.
        model.reasoning = false;
        model.id = "gpt-4o".to_string();
        model.compat = Some(ModelCompat {
            supports_temperature: Some(false),
            ..Default::default()
        });
        let req_disabled = build_openai_request(&model, &Context::default(), &opts);
        assert!(req_disabled.get("temperature").is_none());
    }
}
