//! Google Generative AI API client, request builder, and SSE event processing.

use futures::StreamExt;

use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::types::*;

/// Provider for Google Generative AI API.
pub struct GoogleProvider {
    client: reqwest::Client,
}

impl GoogleProvider {
    /// Creates a new Google provider with default HTTP client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new Google provider with the given HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for GoogleProvider {
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
            let body = build_google_request(&model, &context, &options);
            let url = resolve_google_endpoint(&model);

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");

            if model.api == "google-vertex" {
                if let Some(api_key) = &options.api_key {
                    req = req.header("authorization", format!("Bearer {api_key}"));
                } else if let Ok(token) = std::env::var("GOOGLE_CLOUD_API_KEY") {
                    req = req.header("authorization", format!("Bearer {token}"));
                }
            } else if let Some(api_key) = &options.api_key {
                req = req.header("x-goog-api-key", api_key);
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
            let mut state = GoogleStreamState::new(&model);

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

/// Resolves the endpoint URL for Google Generative AI or Google Vertex AI.
pub fn resolve_google_endpoint(model: &Model) -> String {
    if model.api == "google-vertex" {
        if model.base_url.contains(":streamGenerateContent") {
            return model.base_url.clone();
        }
        let location = std::env::var("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|_| "us-central1".to_string());
        let project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .or_else(|_| std::env::var("GCLOUD_PROJECT"))
            .unwrap_or_else(|_| "default".to_string());

        if model.base_url.is_empty() {
            format!(
                "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
                model.id
            )
        } else {
            let base_url = model.base_url.trim_end_matches('/');
            format!("{base_url}/publishers/google/models/{}:streamGenerateContent?alt=sse", model.id)
        }
    } else {
        let base_url = if model.base_url.is_empty() {
            "https://generativelanguage.googleapis.com/v1beta"
        } else {
            model.base_url.trim_end_matches('/')
        };
        if model.base_url.contains(":streamGenerateContent") {
            model.base_url.clone()
        } else {
            format!("{base_url}/models/{}:streamGenerateContent?alt=sse", model.id)
        }
    }
}

/// Builds the JSON request payload for Google Generative AI API.
pub fn build_google_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
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

    let mut contents: Vec<serde_json::Value> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let parts: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => {
                        vec![serde_json::json!({ "text": text })]
                    }
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({ "text": tc.text })
                            }
                            UserContentBlock::Image(img) => {
                                serde_json::json!({
                                    "inlineData": {
                                        "mimeType": img.mime_type,
                                        "data": img.data,
                                    }
                                })
                            }
                        })
                        .collect(),
                };
                if !parts.is_empty() {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
            }
            Message::Assistant(a) => {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            if !tc.text.is_empty() || tc.text_signature.is_some() {
                                let mut p = serde_json::Map::new();
                                p.insert("text".to_string(), serde_json::json!(tc.text));
                                if let Some(sig) = &tc.text_signature {
                                    p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                                }
                                parts.push(serde_json::Value::Object(p));
                            }
                        }
                        AssistantContentBlock::Thinking(th) => {
                            let mut p = serde_json::Map::new();
                            p.insert("thought".to_string(), serde_json::json!(true));
                            p.insert("text".to_string(), serde_json::json!(th.thinking));
                            if let Some(sig) = &th.thinking_signature {
                                p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                            }
                            parts.push(serde_json::Value::Object(p));
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            let mut fc = serde_json::Map::new();
                            fc.insert("name".to_string(), serde_json::json!(tc.name));
                            fc.insert(
                                "args".to_string(),
                                serde_json::Value::Object(tc.arguments.clone()),
                            );
                            if !tc.id.is_empty() {
                                fc.insert("id".to_string(), serde_json::json!(tc.id));
                            }
                            let mut p = serde_json::Map::new();
                            p.insert("functionCall".to_string(), serde_json::Value::Object(fc));
                            if let Some(sig) = &tc.thought_signature {
                                p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                            }
                            parts.push(serde_json::Value::Object(p));
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(serde_json::json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ToolResultContentBlock::Text(tc) => Some(tc.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let response_obj = if tr.is_error {
                    serde_json::json!({ "error": text })
                } else {
                    serde_json::json!({ "result": text })
                };

                let mut fr = serde_json::Map::new();
                fr.insert("name".to_string(), serde_json::json!(tr.tool_name));
                fr.insert("response".to_string(), response_obj);
                if !tr.tool_call_id.is_empty() {
                    fr.insert("id".to_string(), serde_json::json!(tr.tool_call_id));
                }

                let part = serde_json::json!({
                    "functionResponse": fr
                });

                let mut merged = false;
                if let Some(last) = contents.last_mut() {
                    if last.get("role").and_then(|r| r.as_str()) == Some("user") {
                        if let Some(parts) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                            if parts.iter().any(|p| p.get("functionResponse").is_some()) {
                                parts.push(part.clone());
                                merged = true;
                            }
                        }
                    }
                }
                if !merged {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [part],
                    }));
                }
            }
        }
    }

    let mut req_map = serde_json::Map::new();

    if !system_texts.is_empty() {
        req_map.insert(
            "systemInstruction".to_string(),
            serde_json::json!({
                "parts": [{ "text": system_texts.join("\n\n") }]
            }),
        );
    }

    req_map.insert("contents".to_string(), serde_json::Value::Array(contents));

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let decls: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            req_map.insert(
                "tools".to_string(),
                serde_json::json!([{ "functionDeclarations": decls }]),
            );
        }
    }

    let mut gen_config = serde_json::Map::new();
    if let Some(temp) = options.temperature {
        gen_config.insert("temperature".to_string(), serde_json::json!(temp));
    }
    if let Some(max_tokens) = options.max_tokens {
        gen_config.insert("maxOutputTokens".to_string(), serde_json::json!(max_tokens));
    }
    if model.reasoning && options.reasoning != Some(ThinkingLevel::Off) {
        let budget = match options.reasoning {
            Some(ThinkingLevel::Minimal) => 128,
            Some(ThinkingLevel::Low) => 2048,
            Some(ThinkingLevel::Medium) => 8192,
            Some(ThinkingLevel::High) | Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => {
                32768
            }
            _ => -1,
        };
        let mut thinking_config = serde_json::Map::new();
        thinking_config.insert("includeThoughts".to_string(), serde_json::json!(true));
        if budget >= 0 {
            thinking_config.insert("thinkingBudget".to_string(), serde_json::json!(budget));
        }
        gen_config.insert(
            "thinkingConfig".to_string(),
            serde_json::Value::Object(thinking_config),
        );
    }

    if !gen_config.is_empty() {
        req_map.insert(
            "generationConfig".to_string(),
            serde_json::Value::Object(gen_config),
        );
    }

    serde_json::Value::Object(req_map)
}

/// Streaming state machine processing Google Generative AI SSE chunks.
pub struct GoogleStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    active_thinking_index: Option<usize>,
    active_text_index: Option<usize>,
    tool_call_counter: usize,
}

impl GoogleStreamState {
    /// Creates a new Google stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            active_thinking_index: None,
            active_text_index: None,
            tool_call_counter: 0,
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
            if let Some(id) = json.get("responseId").and_then(|v| v.as_str()) {
                self.output.response_id = Some(id.to_string());
            }
        }

        if !self.started {
            self.started = true;
            sender.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        if let Some(usage) = json.get("usageMetadata") {
            let prompt = usage
                .get("promptTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let candidates = usage
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let thoughts = usage
                .get("thoughtsTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cached = usage
                .get("cachedContentTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let total = usage
                .get("totalTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            self.output.usage.input = prompt.saturating_sub(cached);
            self.output.usage.output = candidates + thoughts;
            self.output.usage.cache_read = cached;
            self.output.usage.cache_write = 0;
            self.output.usage.total_tokens = if total > 0 {
                total
            } else {
                self.output.usage.input + self.output.usage.output + cached
            };
            self.output.usage.calculate_cost(&self.model.cost);
        }

        if let Some(candidate) = json.get("candidates").and_then(|c| c.get(0)) {
            if let Some(finish_reason) = candidate.get("finishReason").and_then(|r| r.as_str()) {
                self.output.stop_reason = match finish_reason {
                    "STOP" => StopReason::Stop,
                    "MAX_TOKENS" => StopReason::Length,
                    "SAFETY" | "RECITATION" => {
                        self.output.error_message =
                            Some(format!("Provider finishReason: {finish_reason}"));
                        StopReason::Error
                    }
                    _ => StopReason::Stop,
                };
            }

            if let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    let is_thought = part
                        .get("thought")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let thought_sig = part
                        .get("thoughtSignature")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string());

                    if is_thought {
                        if let Some(text_idx) = self.active_text_index.take() {
                            let text_content = if let Some(AssistantContentBlock::Text(tc)) =
                                self.output.content.get(text_idx)
                            {
                                tc.text.clone()
                            } else {
                                String::new()
                            };
                            sender.push(AssistantMessageEvent::TextEnd {
                                content_index: text_idx,
                                content: text_content,
                                partial: self.output.clone(),
                            });
                        }

                        let content_index = match self.active_thinking_index {
                            Some(idx) => idx,
                            None => {
                                self.output
                                    .content
                                    .push(AssistantContentBlock::Thinking(ThinkingContent {
                                        thinking: String::new(),
                                        thinking_signature: thought_sig.clone(),
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

                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                if let Some(AssistantContentBlock::Thinking(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    tc.thinking.push_str(text);
                                    if thought_sig.is_some() {
                                        tc.thinking_signature = thought_sig;
                                    }
                                }
                                sender.push(AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta: text.to_string(),
                                    partial: self.output.clone(),
                                });
                            }
                        }
                    } else if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if let Some(thinking_idx) = self.active_thinking_index.take() {
                            let thinking_content = if let Some(AssistantContentBlock::Thinking(tc)) =
                                self.output.content.get(thinking_idx)
                            {
                                tc.thinking.clone()
                            } else {
                                String::new()
                            };
                            sender.push(AssistantMessageEvent::ThinkingEnd {
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
                                        text_signature: thought_sig.clone(),
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

                        if !text.is_empty() {
                            if let Some(AssistantContentBlock::Text(tc)) =
                                self.output.content.get_mut(content_index)
                            {
                                tc.text.push_str(text);
                                if thought_sig.is_some() {
                                    tc.text_signature = thought_sig;
                                }
                            }
                            sender.push(AssistantMessageEvent::TextDelta {
                                content_index,
                                delta: text.to_string(),
                                partial: self.output.clone(),
                            });
                        }
                    } else if let Some(fc) = part.get("functionCall") {
                        if let Some(thinking_idx) = self.active_thinking_index.take() {
                            let thinking_content = if let Some(AssistantContentBlock::Thinking(tc)) =
                                self.output.content.get(thinking_idx)
                            {
                                tc.thinking.clone()
                            } else {
                                String::new()
                            };
                            sender.push(AssistantMessageEvent::ThinkingEnd {
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
                            sender.push(AssistantMessageEvent::TextEnd {
                                content_index: text_idx,
                                content: text_content,
                                partial: self.output.clone(),
                            });
                        }

                        let name = fc
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = fc
                            .get("args")
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        let id = if let Some(id_str) = fc.get("id").and_then(|v| v.as_str()) {
                            if !id_str.is_empty() {
                                id_str.to_string()
                            } else {
                                self.tool_call_counter += 1;
                                format!("{name}_{}_{}", now_ms(), self.tool_call_counter)
                            }
                        } else {
                            self.tool_call_counter += 1;
                            format!("{name}_{}_{}", now_ms(), self.tool_call_counter)
                        };

                        let tool_call = ToolCall {
                            id,
                            name,
                            arguments: args,
                            arguments_error: None,
                            thought_signature: thought_sig,
                        };

                        self.output
                            .content
                            .push(AssistantContentBlock::ToolCall(tool_call.clone()));
                        let content_index = self.output.content.len() - 1;

                        sender.push(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: self.output.clone(),
                        });
                        sender.push(AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta: serde_json::to_string(&tool_call.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                            partial: self.output.clone(),
                        });
                        sender.push(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial: self.output.clone(),
                        });
                    }
                }
            }
        }
    }

    /// Closes any remaining active blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        if let Some(thinking_idx) = self.active_thinking_index.take() {
            let content = if let Some(AssistantContentBlock::Thinking(tc)) =
                self.output.content.get(thinking_idx)
            {
                tc.thinking.clone()
            } else {
                String::new()
            };
            sender.push(AssistantMessageEvent::ThinkingEnd {
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
            sender.push(AssistantMessageEvent::TextEnd {
                content_index: text_idx,
                content,
                partial: self.output.clone(),
            });
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

/// Pure processor converting Google SSE events into an AssistantMessage.
pub fn process_google_sse_events(
    model: &Model,
    events: &[SseEvent],
    sender: &AssistantEventSender,
) -> AssistantMessage {
    let mut state = GoogleStreamState::new(model);
    for event in events {
        state.process_chunk(&event.data, sender);
    }
    state.finish(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_gemini_model() -> Model {
        Model {
            id: "gemini-2.5-flash".to_string(),
            name: "Gemini 2.5 Flash".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.15,
                output: 0.60,
                cache_read: 0.0375,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 8192,
            headers: None,
        }
    }

    #[test]
    fn test_build_google_request() {
        let model = test_gemini_model();
        let mut params = serde_json::Map::new();
        params.insert("type".to_string(), serde_json::json!("object"));

        let context = Context {
            system_prompt: Some("You are a helpful coding assistant.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("What is the sum of 10 and 20?".to_string()),
                    timestamp: 1000,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::Thinking(ThinkingContent {
                            thinking: "I need to add 10 and 20.".to_string(),
                            thinking_signature: Some("sig_think_123".to_string()),
                            redacted: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_add1".to_string(),
                            name: "add".to_string(),
                            arguments: {
                                let mut m = serde_json::Map::new();
                                m.insert("a".to_string(), serde_json::json!(10));
                                m.insert("b".to_string(), serde_json::json!(20));
                                m
                            },
                            arguments_error: None,
                            thought_signature: Some("sig_tool_123".to_string()),
                        }),
                    ],
                    api: "google-generative-ai".to_string(),
                    provider: "google".to_string(),
                    model: "gemini-2.5-flash".to_string(),
                    response_id: Some("resp-01".to_string()),
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 1001,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_add1".to_string(),
                    tool_name: "add".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "30".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 1002,
                }),
            ],
            tools: Some(vec![ToolDefinition {
                name: "add".to_string(),
                description: "Adds numbers".to_string(),
                parameters: serde_json::Value::Object(params),
            }]),
        };

        let options = StreamOptions {
            api_key: Some("google-key-123".to_string()),
            temperature: Some(0.4),
            max_tokens: Some(4096),
            reasoning: Some(ThinkingLevel::High),
            session_id: None,
            headers: None,
        };

        let req = build_google_request(&model, &context, &options);

        // System instruction
        assert_eq!(
            req["systemInstruction"]["parts"][0]["text"],
            "You are a helpful coding assistant."
        );

        // Contents
        let contents = req["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "What is the sum of 10 and 20?");

        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["thought"], true);
        assert_eq!(contents[1]["parts"][0]["text"], "I need to add 10 and 20.");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "sig_think_123");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "add");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["id"], "call_add1");
        assert_eq!(contents[1]["parts"][1]["thoughtSignature"], "sig_tool_123");

        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "add");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["result"], "30");

        // Tools
        let tools = req["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["functionDeclarations"][0]["name"], "add");

        // Generation config
        assert_eq!(req["generationConfig"]["temperature"], 0.4);
        assert_eq!(req["generationConfig"]["maxOutputTokens"], 4096);
        assert_eq!(req["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
        assert_eq!(req["generationConfig"]["thinkingConfig"]["thinkingBudget"], 32768);
    }

    #[test]
    fn test_process_google_sse_events() {
        let model = test_gemini_model();
        let (sender, _stream) = create_event_stream();

        let chunks = vec![
            serde_json::json!({
                "responseId": "gemini-resp-99",
                "candidates": [{
                    "content": {
                        "parts": [{
                            "thought": true,
                            "text": "Thinking through ",
                            "thoughtSignature": "sig-alpha"
                        }]
                    }
                }]
            })
            .to_string(),
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "thought": true,
                            "text": "the calculation."
                        }]
                    }
                }]
            })
            .to_string(),
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "text": "The calculation is "
                        }]
                    }
                }]
            })
            .to_string(),
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "text": "complete."
                        }]
                    }
                }]
            })
            .to_string(),
            serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "functionCall": {
                                "id": "call_fn_1",
                                "name": "notify",
                                "args": { "msg": "done" }
                            },
                            "thoughtSignature": "sig-beta"
                        }]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 50,
                    "candidatesTokenCount": 25,
                    "thoughtsTokenCount": 15,
                    "cachedContentTokenCount": 10,
                    "totalTokenCount": 90
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

        let result = process_google_sse_events(&model, &sse_events, &sender);

        assert_eq!(result.response_id.as_deref(), Some("gemini-resp-99"));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.usage.input, 40); // 50 - 10
        assert_eq!(result.usage.output, 40); // 25 + 15
        assert_eq!(result.usage.cache_read, 10);
        assert_eq!(result.usage.total_tokens, 90);
        assert_eq!(result.content.len(), 3);

        // Block 0: Thinking
        match &result.content[0] {
            AssistantContentBlock::Thinking(th) => {
                assert_eq!(th.thinking, "Thinking through the calculation.");
                assert_eq!(th.thinking_signature.as_deref(), Some("sig-alpha"));
            }
            _ => panic!("Expected Thinking block at index 0"),
        }

        // Block 1: Text
        match &result.content[1] {
            AssistantContentBlock::Text(tc) => {
                assert_eq!(tc.text, "The calculation is complete.");
            }
            _ => panic!("Expected Text block at index 1"),
        }

        // Block 2: ToolCall
        match &result.content[2] {
            AssistantContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "call_fn_1");
                assert_eq!(tc.name, "notify");
                assert_eq!(tc.arguments["msg"], "done");
                assert_eq!(tc.thought_signature.as_deref(), Some("sig-beta"));
            }
            _ => panic!("Expected ToolCall block at index 2"),
        }
    }

    #[test]
    fn test_vertex_url_and_request() {
        let model = Model {
            id: "gemini-2.5-flash".to_string(),
            name: "Gemini 2.5 Flash (Vertex)".to_string(),
            api: "google-vertex".to_string(),
            provider: "google-vertex".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 8192,
            headers: None,
        };

        let endpoint = resolve_google_endpoint(&model);
        assert!(endpoint.contains("-aiplatform.googleapis.com/v1/projects/"));
        assert!(endpoint.contains("/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"));

        let context = Context {
            system_prompt: Some("Vertex system prompt".to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Hello Vertex".to_string()),
                timestamp: 10,
            })],
            tools: None,
        };
        let options = StreamOptions {
            api_key: Some("vertex-oauth-bearer-token".to_string()),
            temperature: Some(0.5),
            max_tokens: Some(2048),
            reasoning: None,
            session_id: None,
            headers: None,
        };

        let req = build_google_request(&model, &context, &options);
        assert_eq!(req["systemInstruction"]["parts"][0]["text"], "Vertex system prompt");
        assert_eq!(req["contents"][0]["role"], "user");
        assert_eq!(req["contents"][0]["parts"][0]["text"], "Hello Vertex");
    }
}
