//! Anthropic Messages API client, request builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::retry::{
    default_http_client, send_with_retry, RetryFailure, RetryPolicy, DEFAULT_STREAM_CHUNK_TIMEOUT,
};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::*;

/// Provider for the Anthropic Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new(default_http_client())
    }
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
            let is_oauth = options.api_key.as_deref().map(is_oauth_token).unwrap_or(false);
            let body = build_anthropic_request(&model, &context, &options, is_oauth);
            let url = anthropic_messages_url(&model.base_url);

            let resp = match send_with_retry(
                || {
                    let mut req = client.post(&url);
                    let headers = build_anthropic_headers(&model, &options, is_oauth);
                    for (k, v) in headers {
                        req = req.header(k, v);
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
            let mut state = AnthropicStreamState::new(&model);
            if is_oauth {
                state.set_oauth(true);
                if let Some(tools) = &context.tools {
                    state.set_declared_tools(tools.clone());
                }
            }

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
                                    state.process_event_into(ev, &mut batch);
                                }
                                let done = state.done;
                                sender.send_batch(batch).await;
                                if done {
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
                state.process_event_into(ev, &mut batch);
            }
            if sse_parser.has_overflowed() {
                state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
            } else if sse_parser.has_pending_bytes() || !state.received_terminal {
                state.mark_error("Stream ended prematurely before Anthropic message_stop");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

/// Builds the Messages API endpoint from a base URL, ensuring the `/v1/messages` path.
fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    // Only a final `/messages` segment means the endpoint is already complete.
    // A base like `/messages-proxy/v1` must still get `/v1/messages` appended.
    if base.rsplit('/').next() == Some("messages") {
        return base.to_string();
    }
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn normalize_anthropic_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if sanitized.len() > 64 {
        sanitized[..64].to_string()
    } else {
        sanitized
    }
}

/// Constant minimum number of tokens reserved for the answer under a shared budget ceiling.
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// Canonical Claude Code CLI version string.
pub const CLAUDE_CODE_VERSION: &str = "2.1.322";

/// Canonical Claude Code 2.x tool names.
pub const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Checks if an API key is an Anthropic OAuth token (contains "sk-ant-oat").
pub fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn equals_ignoring_underscores_ascii(a: &str, b: &str) -> bool {
    let ia = a
        .bytes()
        .filter(|c| *c != b'_')
        .map(|c| c.to_ascii_lowercase());
    let ib = b
        .bytes()
        .filter(|c| *c != b'_')
        .map(|c| c.to_ascii_lowercase());
    ia.eq(ib)
}

/// Converts a tool name to its Claude Code canonical casing if matched.
pub fn to_claude_code_name(name: &str) -> String {
    for &tool in CLAUDE_CODE_TOOLS {
        if tool.eq_ignore_ascii_case(name) {
            return tool.to_string();
        }
    }
    for &tool in CLAUDE_CODE_TOOLS {
        if equals_ignoring_underscores_ascii(tool, name) {
            return tool.to_string();
        }
    }
    name.to_string()
}

/// Restores a Claude Code canonical tool name back to the declared tool name.
pub fn from_claude_code_name(name: &str, declared_tools: &[ToolDefinition]) -> String {
    for tool in declared_tools {
        if tool.name.eq_ignore_ascii_case(name) {
            return tool.name.clone();
        }
    }
    for tool in declared_tools {
        if equals_ignoring_underscores_ascii(&tool.name, name) {
            return tool.name.clone();
        }
    }
    name.to_string()
}

/// Cap a thinking budget so at least MIN_ANSWER_TOKENS remain under a shared response ceiling.
pub fn clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64 {
    thinking_budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

pub fn thinking_budget_for_level(
    reasoning_level: &ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> u64 {
    match reasoning_level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => custom_budgets.and_then(|b| b.minimal).unwrap_or(1024),
        ThinkingLevel::Low => custom_budgets.and_then(|b| b.low).unwrap_or(2048),
        ThinkingLevel::Medium => custom_budgets.and_then(|b| b.medium).unwrap_or(8192),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            custom_budgets.and_then(|b| b.high).unwrap_or(16384)
        }
    }
}

/// Adjusts max_tokens and thinking_budget according to reasoning level and custom budgets.
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: &ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> (u64, u64) {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => base.saturating_add(thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = clamp_thinking_budget_to_answer_room(thinking_budget, max_tokens);
    }

    (max_tokens, thinking_budget)
}

/// Resolves beta headers for Anthropic API requests.
pub fn resolve_anthropic_beta_headers(
    model: &Model,
    options: &StreamOptions,
    is_oauth: bool,
) -> String {
    let mut configured: Option<String> = None;
    if let Some(headers) = &model.headers {
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("anthropic-beta") {
                configured = Some(v.clone());
            }
        }
    }
    if let Some(headers) = &options.headers {
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("anthropic-beta") {
                configured = Some(v.clone());
            }
        }
    }
    let mut features: Vec<String> = Vec::new();
    if is_oauth {
        features.push("claude-code-20250219".to_string());
        features.push("oauth-2025-04-20".to_string());
    }
    if model.reasoning
        && options.reasoning.as_ref().is_some_and(|r| *r != ThinkingLevel::Off)
        && options.interleaved_thinking.unwrap_or(true)
        && model.compat.as_ref().and_then(|c| c.force_adaptive_thinking) != Some(true)
    {
        features.push("interleaved-thinking-2025-05-14".to_string());
    }
    if model.compat.as_ref().and_then(|c| c.supports_mid_convo_effort) == Some(true) {
        features.push("mid-conversation-output-config-2026-07-01".to_string());
        features.push("thinking-binding-controls-2026-08-01".to_string());
    }

    // User-supplied beta headers are additive: they must never drop the
    // OAuth and thinking features required for the request to work.
    if let Some(conf) = configured {
        for p in conf.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !features.iter().any(|f| f == p) {
                features.push(p.to_string());
            }
        }
    }

    let mut unique_features = Vec::new();
    for f in features {
        if !unique_features.contains(&f) {
            unique_features.push(f);
        }
    }
    unique_features.join(",")
}

/// Builds the headers for an Anthropic Messages API request.
pub fn build_anthropic_headers(
    model: &Model,
    options: &StreamOptions,
    is_oauth: bool,
) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));

    let beta_headers = resolve_anthropic_beta_headers(model, options, is_oauth);
    if !beta_headers.is_empty() {
        headers.push(("anthropic-beta".to_string(), beta_headers));
    }

    if let Some(api_key) = &options.api_key {
        if is_oauth {
            headers.push(("authorization".to_string(), format!("Bearer {api_key}")));
            headers.push(("user-agent".to_string(), format!("claude-cli/{CLAUDE_CODE_VERSION}")));
            headers.push(("x-app".to_string(), "cli".to_string()));
        } else {
            headers.push(("x-api-key".to_string(), api_key.clone()));
        }
    }

    if let Some(m_headers) = &model.headers {
        for (k, v) in m_headers {
            if !k.eq_ignore_ascii_case("anthropic-beta") {
                headers.push((k.clone(), v.clone()));
            }
        }
    }
    if let Some(o_headers) = &options.headers {
        for (k, v) in o_headers {
            if !k.eq_ignore_ascii_case("anthropic-beta") {
                headers.push((k.clone(), v.clone()));
            }
        }
    }

    headers
}

/// Builds the JSON request payload for the Anthropic Messages API.
pub fn build_anthropic_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    is_oauth: bool,
) -> serde_json::Value {
    use crate::transform_messages::{split_error_tool_result_images, transform_messages};
    use crate::unicode::sanitize_surrogates;

    let resolved_cache_retention = resolve_cache_retention(options.cache_retention);

    let cache_control = match resolved_cache_retention {
        CacheRetention::Short => Some(serde_json::json!({ "type": "ephemeral" })),
        CacheRetention::Long => {
            let supports_long = model
                .compat
                .as_ref()
                .and_then(|c| c.supports_long_cache_retention)
                .unwrap_or(true);
            if supports_long {
                Some(serde_json::json!({ "type": "ephemeral", "ttl": "1h" }))
            } else {
                Some(serde_json::json!({ "type": "ephemeral" }))
            }
        }
        CacheRetention::None => None,
    };

    let mut system_text = String::new();
    if let Some(sp) = &context.system_prompt {
        if !sp.is_empty() {
            system_text.push_str(&sanitize_surrogates(sp));
        }
    }

    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, _m: &Model, _a: &AssistantMessage| normalize_anthropic_tool_call_id(id)),
    );

    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut has_seen_non_system = false;

    let mut i = 0;
    while i < transformed_messages.len() {
        let msg = &transformed_messages[i];
        match msg {
            Message::System(sys) => {
                let sanitized_content = sanitize_surrogates(&sys.content);
                if !has_seen_non_system {
                    if !system_text.is_empty() {
                        system_text.push_str("\n\n");
                    }
                    system_text.push_str(&sanitized_content);
                } else {
                    push_user_block(&mut messages, serde_json::json!({
                        "type": "text",
                        "text": sanitized_content,
                    }));
                }
                i += 1;
            }
            Message::User(u) => {
                has_seen_non_system = true;
                match &u.content {
                    UserContent::Text(text) => {
                        push_user_block(&mut messages, serde_json::json!({
                            "type": "text",
                            "text": sanitize_surrogates(text),
                        }));
                    }
                    UserContent::Blocks(blocks) => {
                        for b in blocks {
                            match b {
                                UserContentBlock::Text(tc) => {
                                    push_user_block(&mut messages, serde_json::json!({
                                        "type": "text",
                                        "text": sanitize_surrogates(&tc.text),
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
                i += 1;
            }
            Message::ToolResult(_) => {
                has_seen_non_system = true;
                let mut tool_results: Vec<serde_json::Value> = Vec::new();
                let mut error_images: Vec<serde_json::Value> = Vec::new();
                let mut j = i;
                while j < transformed_messages.len() {
                    if let Message::ToolResult(tr) = &transformed_messages[j] {
                        let split = split_error_tool_result_images(tr);
                        let content_blocks = if let Some(ref s) = split {
                            &s.content
                        } else {
                            &tr.content
                        };
                        let mut tool_contents = Vec::new();
                        for b in content_blocks {
                            match b {
                                ToolResultContentBlock::Text(tc) => {
                                    tool_contents.push(serde_json::json!({
                                        "type": "text",
                                        "text": sanitize_surrogates(&tc.text),
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
                        tool_results.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tr.tool_call_id,
                            "content": tool_contents,
                            "is_error": tr.is_error,
                        }));
                        if let Some(s) = split {
                            error_images.push(serde_json::json!({
                                "type": "text",
                                "text": sanitize_surrogates(&s.caption),
                            }));
                            for img in s.images {
                                error_images.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": img.mime_type,
                                        "data": img.data,
                                    }
                                }));
                            }
                        }
                        j += 1;
                    } else {
                        break;
                    }
                }
                i = j;

                let mut combined_user_blocks = tool_results;
                combined_user_blocks.extend(error_images);
                for block in combined_user_blocks {
                    push_user_block(&mut messages, block);
                }
            }
            Message::Assistant(a) => {
                has_seen_non_system = true;
                let mut assistant_blocks = Vec::new();
                for b in &a.content {
                    match b {
                        AssistantContentBlock::Text(tc) => {
                            assistant_blocks.push(serde_json::json!({
                                "type": "text",
                                "text": sanitize_surrogates(&tc.text),
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
                                    "thinking": sanitize_surrogates(&tc.thinking),
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
                            let call_name = if is_oauth {
                                to_claude_code_name(&tc.name)
                            } else {
                                tc.name.clone()
                            };
                            assistant_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": call_name,
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
                            i += 1;
                            continue;
                        }
                    }
                }
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": assistant_blocks,
                }));
                i += 1;
            }
        }
    }

    // OAuth requests have a strict limit of four cache breakpoints. Spend
    // them on the system prompt only, never on the last tool/user message.
    if let Some(cc) = &cache_control {
        if !is_oauth {
            if let Some(last_msg) = messages.last_mut() {
                if last_msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                    if let Some(content_arr) = last_msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                        if let Some(last_block) = content_arr.last_mut() {
                            if let Some(obj) = last_block.as_object_mut() {
                                obj.insert("cache_control".to_string(), cc.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("model".to_string(), serde_json::json!(model.id));
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));
    req_map.insert("stream".to_string(), serde_json::json!(true));

    if is_oauth {
        let mut sys_blocks = Vec::new();
        let identity_block = serde_json::json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        });
        // Never put a breakpoint on the short identity block: it cannot reach
        // the 1024-token minimum cache threshold. If it is the only system
        // block, there is nothing worth caching here.
        sys_blocks.push(identity_block);

        if !system_text.is_empty() {
            let mut text_block = serde_json::json!({
                "type": "text",
                "text": system_text,
            });
            if let Some(cc) = &cache_control {
                text_block
                    .as_object_mut()
                    .unwrap()
                    .insert("cache_control".to_string(), cc.clone());
            }
            sys_blocks.push(text_block);
        }
        req_map.insert("system".to_string(), serde_json::Value::Array(sys_blocks));
    } else if let Some(cc) = &cache_control {
        if !system_text.is_empty() {
            req_map.insert(
                "system".to_string(),
                serde_json::json!([
                    {
                        "type": "text",
                        "text": system_text,
                        "cache_control": cc
                    }
                ]),
            );
        }
    } else if !system_text.is_empty() {
        req_map.insert("system".to_string(), serde_json::json!(system_text));
    }

    let tool_cache_control = if !is_oauth
        && model
            .compat
            .as_ref()
            .and_then(|c| c.supports_cache_control_on_tools)
            .unwrap_or(true)
    {
        cache_control.clone()
    } else {
        None
    };

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let tool_vals: Vec<serde_json::Value> = tools
                .iter()
                .enumerate()
                .map(|(idx, t)| {
                    let tool_name = if is_oauth {
                        to_claude_code_name(&t.name)
                    } else {
                        t.name.clone()
                    };
                    let mut tool_obj = serde_json::json!({
                        "name": tool_name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    });
                    if let Some(ref cc) = tool_cache_control {
                        if idx == tools.len() - 1 {
                            tool_obj.as_object_mut().unwrap().insert(
                                "cache_control".to_string(),
                                cc.clone(),
                            );
                        }
                    }
                    tool_obj
                })
                .collect();
            req_map.insert("tools".to_string(), serde_json::Value::Array(tool_vals));
        }
    }

    let is_reasoning_off = options.reasoning.as_ref() == Some(&ThinkingLevel::Off);
    let is_reasoning_enabled = model.reasoning
        && options.reasoning.as_ref().is_some_and(|r| *r != ThinkingLevel::Off);

    if is_reasoning_off && model.reasoning {
        let is_off_mapped_to_null = model
            .thinking_level_map
            .as_ref()
            .and_then(|m| m.get("off"))
            == Some(&None);
        if !is_off_mapped_to_null {
            req_map.insert(
                "thinking".to_string(),
                serde_json::json!({ "type": "disabled" }),
            );
        }
        let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
        req_map.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
        if let Some(temp) = options.temperature {
            req_map.insert("temperature".to_string(), serde_json::json!(temp));
        }
    } else if is_reasoning_enabled {
        let reasoning_level = options.reasoning.as_ref().unwrap();
        let is_force_adaptive = model
            .compat
            .as_ref()
            .and_then(|c| c.force_adaptive_thinking)
            == Some(true);

        if is_force_adaptive {
            let display_str = match options.thinking_display {
                Some(AnthropicThinkingDisplay::Omitted) => "omitted",
                _ => "summarized",
            };
            req_map.insert(
                "thinking".to_string(),
                serde_json::json!({
                    "type": "adaptive",
                    "display": display_str,
                }),
            );
            if let Some(effort) = options.effort {
                let effort_str = match effort {
                    AnthropicEffort::Low => "low",
                    AnthropicEffort::Medium => "medium",
                    AnthropicEffort::High => "high",
                    AnthropicEffort::Xhigh => "xhigh",
                    AnthropicEffort::Max => "max",
                };
                req_map.insert(
                    "output_config".to_string(),
                    serde_json::json!({ "effort": effort_str }),
                );
            }
            let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
            req_map.insert("max_tokens".to_string(), serde_json::json!(max_tokens));
        } else {
            let (adj_max_tokens, adj_budget) = adjust_max_tokens_for_thinking(
                options.max_tokens,
                model.max_tokens,
                reasoning_level,
                options.thinking_budgets.as_ref(),
            );
            req_map.insert("max_tokens".to_string(), serde_json::json!(adj_max_tokens));

            // The API requires budget_tokens >= MIN_ANSWER_TOKENS and
            // budget_tokens < max_tokens. If the ceiling cannot fit both, omit
            // the thinking block entirely instead of emitting a sub-floor
            // budget that makes every request fail with HTTP 400.
            if adj_max_tokens > MIN_ANSWER_TOKENS {
                let final_budget = options
                    .thinking_budget_tokens
                    .unwrap_or(adj_budget)
                    .max(MIN_ANSWER_TOKENS)
                    .min(adj_max_tokens.saturating_sub(MIN_ANSWER_TOKENS));

                let display_str = match options.thinking_display {
                    Some(AnthropicThinkingDisplay::Omitted) => "omitted",
                    _ => "summarized",
                };

                let mut thinking_obj = serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": final_budget,
                });
                if options.thinking_display.is_some() {
                    thinking_obj
                        .as_object_mut()
                        .unwrap()
                        .insert("display".to_string(), serde_json::json!(display_str));
                }
                req_map.insert("thinking".to_string(), thinking_obj);
            }
        }
    } else {
        let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
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
        stream: Box<ToolCallArgumentsStream>,
    },
}

/// Streaming state machine processing Anthropic SSE events.
pub struct AnthropicStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    received_terminal: bool,
    open_blocks: HashMap<usize, AnthropicOpenBlock>,
    pub is_oauth: bool,
    pub declared_tools: Vec<ToolDefinition>,
}

impl AnthropicStreamState {
    /// Creates a new streaming state initialized for the model.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            received_terminal: false,
            open_blocks: HashMap::new(),
            is_oauth: false,
            declared_tools: Vec::new(),
        }
    }

    /// Configures whether the stream should mimic Claude Code OAuth tool name mapping.
    pub fn set_oauth(&mut self, is_oauth: bool) {
        self.is_oauth = is_oauth;
    }

    /// Sets declared tools for restoring original tool names.
    pub fn set_declared_tools(&mut self, tools: Vec<ToolDefinition>) {
        self.declared_tools = tools;
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

    /// Processes a slice of SSE events and emits corresponding AssistantMessageEvents.
    pub fn process_events(&mut self, events: &[SseEvent], sender: &AssistantEventSender) {
        let mut out = Vec::new();
        for event in events {
            self.process_event_into(event, &mut out);
        }
        for ev in out {
            sender.push(ev);
        }
    }

    /// Processes a single SSE event.
    pub fn process_event(&mut self, sse_event: &SseEvent, sender: &AssistantEventSender) {
        let mut out = Vec::new();
        self.process_event_into(sse_event, &mut out);
        for ev in out {
            sender.push(ev);
        }
    }

    /// Processes a single SSE event and appends emitted `AssistantMessageEvent`s into `out`.
    pub fn process_event_into(&mut self, sse_event: &SseEvent, out: &mut Vec<AssistantMessageEvent>) {
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
                    out.push(AssistantMessageEvent::Start {
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
                            out.push(AssistantMessageEvent::TextStart {
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
                            out.push(AssistantMessageEvent::ThinkingStart {
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
                            out.push(AssistantMessageEvent::ThinkingStart {
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
                            let raw_name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = if self.is_oauth {
                                from_claude_code_name(&raw_name, &self.declared_tools)
                            } else {
                                raw_name
                            };
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
                                    stream: Box::new(ToolCallArgumentsStream::new()),
                                },
                            );
                            out.push(AssistantMessageEvent::ToolCallStart {
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
                                out.push(AssistantMessageEvent::TextDelta {
                                    content_index,
                                    delta: text_delta,
                                    partial: self.output.metadata_clone(),
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
                                out.push(AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta: thinking_delta,
                                    partial: self.output.metadata_clone(),
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
                                stream,
                            }) = self.open_blocks.get_mut(&index)
                            {
                                let content_index = *content_index;
                                let partial_json = delta
                                    .get("partial_json")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = stream.append(&partial_json);
                                if let Some(AssistantContentBlock::ToolCall(tc)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    tc.arguments = args;
                                }
                                out.push(AssistantMessageEvent::ToolCallDelta {
                                    content_index,
                                    delta: partial_json,
                                    partial: self.output.metadata_clone_with_block(content_index),
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
                            out.push(AssistantMessageEvent::TextEnd {
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
                            out.push(AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content,
                                partial: self.output.clone(),
                            });
                        }
                        AnthropicOpenBlock::ToolCall {
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

                            out.push(AssistantMessageEvent::ToolCallEnd {
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
                self.received_terminal = true;
                if !self.done {
                    self.done = true;
                    out.push(AssistantMessageEvent::Done {
                        reason: self.output.stop_reason,
                        message: self.output.clone(),
                    });
                }
            }
            "error" => {
                self.received_terminal = true;
                self.output.stop_reason = StopReason::Error;
                let msg = json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Anthropic API error");
                self.output.error_message = Some(msg.to_string());
                self.done = true;
                out.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: self.output.clone(),
                });
            }
            _ => {}
        }
    }

    /// Closes any unclosed content blocks and appends the terminal completion event into `out`.
    pub fn finish_into(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
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
                        out.push(AssistantMessageEvent::TextEnd {
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
                        out.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    AnthropicOpenBlock::ToolCall {
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

                        out.push(AssistantMessageEvent::ToolCallEnd {
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

    /// Closes any unclosed content blocks and emits the terminal completion event if needed.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut out = Vec::new();
        let res = self.finish_into(&mut out);
        for ev in out {
            sender.push(ev);
        }
        res
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
        // Provider-reported counts are untrusted; wrapping them would corrupt
        // accounting or panic in debug builds.
        self.output.usage.total_tokens = self
            .output
            .usage
            .input
            .saturating_add(self.output.usage.output)
            .saturating_add(self.output.usage.cache_read)
            .saturating_add(self.output.usage.cache_write);
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

    #[test]
    fn messages_url_always_targets_v1_messages() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

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
            ..Default::default()
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
            ..Default::default()
        };

        let req = build_anthropic_request(&model, &context, &options, false);

        assert_eq!(req["model"], "claude-3-7-sonnet-20250219");
        assert_eq!(req["stream"], true);
        assert_eq!(
            req["system"],
            serde_json::json!([{
                "type": "text",
                "text": "You are a helpful assistant.",
                "cache_control": { "type": "ephemeral" }
            }])
        );
        assert_eq!(req["thinking"]["type"], "enabled");
        assert_eq!(req["thinking"]["budget_tokens"], 7168);
        assert_eq!(req["max_tokens"], 8192);

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

    #[tokio::test]
    async fn test_anthropic_stream_fails_on_premature_eof() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Partial\"}}\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let mut model = test_model();
        model.base_url = format!("http://{addr}/v1");
        let provider = AnthropicProvider::default();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        };
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stream = provider.stream(&model, &context, &options, cancel);
        let msg = stream.collect_final_message().await.unwrap();
        assert_eq!(msg.stop_reason, StopReason::Error);
        assert!(msg
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("prematurely"));
    }

    #[test]
    fn build_request_transforms_cross_model_and_splits_error_images() {
        let model = test_model();
        let context = Context {
            system_prompt: Some("You are helpful.".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Run tool".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::Thinking(crate::types::ThinkingContent {
                            thinking: "Foreign thinking to be dropped".to_string(),
                            thinking_signature: None,
                            redacted: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_abc123|foreign_item_def456".to_string(),
                            name: "take_screenshot".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "openai-responses".to_string(),
                    provider: "openai".to_string(),
                    model: "gpt-5".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 200,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_abc123|foreign_item_def456".to_string(),
                    tool_name: "take_screenshot".to_string(),
                    content: vec![
                        ToolResultContentBlock::Text(TextContent {
                            text: "Error running tool".to_string(),
                            text_signature: None,
                        }),
                        ToolResultContentBlock::Image(ImageContent {
                            mime_type: "image/png".to_string(),
                            data: "base64data".to_string(),
                        }),
                    ],
                    details: None,
                    is_error: true,
                    timestamp: 300,
                    duration_ms: None,
                    usage: None,
                }),
            ],
            tools: None,
        };

        let req = build_anthropic_request(&model, &context, &StreamOptions::default(), false);
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);

        let assistant_msg = &messages[1];
        assert_eq!(assistant_msg["role"], "assistant");
        let assistant_blocks = assistant_msg["content"].as_array().unwrap();
        assert_eq!(assistant_blocks.len(), 1, "Foreign thinking block must be dropped");
        assert_eq!(assistant_blocks[0]["type"], "tool_use");
        assert_eq!(assistant_blocks[0]["id"], "call_abc123_foreign_item_def456");

        let tool_user_msg = &messages[2];
        assert_eq!(tool_user_msg["role"], "user");
        let user_blocks = tool_user_msg["content"].as_array().unwrap();
        assert_eq!(user_blocks.len(), 3, "Expected tool_result, caption, and image");
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "call_abc123_foreign_item_def456");
        let tool_result_content = user_blocks[0]["content"].as_array().unwrap();
        assert_eq!(tool_result_content[0]["text"], "Error running tool");
        assert_eq!(tool_result_content[1]["text"], "(image attached after the tool results)");

        assert_eq!(user_blocks[1]["type"], "text");
        assert!(user_blocks[1]["text"].as_str().unwrap().contains("Images from the error result of tool call call_abc123_foreign_item_def456"));

        assert_eq!(user_blocks[2]["type"], "image");
        assert_eq!(user_blocks[2]["source"]["data"], "base64data");
    }

    #[test]
    fn anthropic_sse_updates_partial_tool_arguments_incrementally() {
        let model = Model {
            id: "claude-3-7-sonnet".to_string(),
            name: "Claude 3.7 Sonnet".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200000,
            max_tokens: 8192,
            headers: None,
            compat: None,
            ..Default::default()
        };

        let mut state = AnthropicStreamState::new(&model);
        let mut events = Vec::new();

        state.process_event_into(
            &SseEvent {
                event: Some("message_start".to_string()),
                data: serde_json::json!({
                    "type": "message_start",
                    "message": { "id": "msg_123", "usage": { "input_tokens": 10, "output_tokens": 0 } }
                }).to_string(),
            },
            &mut events,
        );

        state.process_event_into(
            &SseEvent {
                event: Some("content_block_start".to_string()),
                data: serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_01",
                        "name": "search"
                    }
                }).to_string(),
            },
            &mut events,
        );

        events.clear();
        state.process_event_into(
            &SseEvent {
                event: Some("content_block_delta".to_string()),
                data: serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": r#"{"query":"hel"#
                    }
                }).to_string(),
            },
            &mut events,
        );

        assert_eq!(events.len(), 1);
        let delta_event = &events[0];
        match delta_event {
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(delta, r#"{"query":"hel"#);
                assert!(!partial.content.is_empty(), "partial.content must not be empty in ToolCallDelta");
                match &partial.content[0] {
                    AssistantContentBlock::ToolCall(tc) => {
                        assert_eq!(
                            tc.arguments.get("query").and_then(|v| v.as_str()),
                            Some("hel"),
                            "ToolCallDelta partial AssistantMessage must reflect parsed partial arguments before content_block_stop"
                        );
                    }
                    _ => panic!("Expected ToolCall at index 0"),
                }
            }
            _ => panic!("Expected ToolCallDelta event"),
        }
    }

    #[test]
    fn test_cache_control_placed_on_system_last_tool_and_last_turn_message() {
        let mut model = test_model();
        model.compat = Some(ModelCompat {
            supports_cache_control_on_tools: Some(true),
            supports_long_cache_retention: Some(true),
            ..Default::default()
        });

        let context = Context {
            system_prompt: Some("System instructions".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Hello".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Hi".to_string(),
                        text_signature: None,
                    })],
                    api: "anthropic-messages".to_string(),
                    provider: "anthropic".to_string(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 2,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Second turn".to_string()),
                    timestamp: 3,
                }),
            ],
            tools: Some(vec![
                ToolDefinition {
                    name: "tool_a".to_string(),
                    description: "Tool A".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                ToolDefinition {
                    name: "tool_b".to_string(),
                    description: "Tool B".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            ]),
        };

        // Case 1: CacheRetention::Short
        let short_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Short),
            ..Default::default()
        };
        let req_short = build_anthropic_request(&model, &context, &short_opts, false);

        // System block has {"type": "ephemeral"}
        let sys_short = req_short["system"].as_array().expect("system array");
        assert_eq!(sys_short[0]["cache_control"], serde_json::json!({"type": "ephemeral"}));

        // Only final tool has cache_control
        let tools_short = req_short["tools"].as_array().expect("tools array");
        assert!(tools_short[0].get("cache_control").is_none());
        assert_eq!(tools_short[1]["cache_control"], serde_json::json!({"type": "ephemeral"}));

        // Final block of last user message has cache_control
        let msgs_short = req_short["messages"].as_array().expect("messages array");
        let last_msg_short = &msgs_short[msgs_short.len() - 1];
        let last_content_short = last_msg_short["content"].as_array().expect("content array");
        assert_eq!(
            last_content_short[last_content_short.len() - 1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );

        // Case 2: CacheRetention::Long with supports_long_cache_retention = true
        let long_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let req_long = build_anthropic_request(&model, &context, &long_opts, false);
        let sys_long = req_long["system"].as_array().expect("system array");
        assert_eq!(
            sys_long[0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
        let tools_long = req_long["tools"].as_array().expect("tools array");
        assert_eq!(
            tools_long[1]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
        let msgs_long = req_long["messages"].as_array().expect("messages array");
        let last_msg_long = &msgs_long[msgs_long.len() - 1];
        let last_content_long = last_msg_long["content"].as_array().expect("content array");
        assert_eq!(
            last_content_long[last_content_long.len() - 1]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );

        // Case 3: CacheRetention::None
        let none_opts = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let req_none = build_anthropic_request(&model, &context, &none_opts, false);
        if let Some(sys_arr) = req_none["system"].as_array() {
            assert!(sys_arr[0].get("cache_control").is_none());
        }
        let tools_none = req_none["tools"].as_array().expect("tools array");
        assert!(tools_none[0].get("cache_control").is_none());
        assert!(tools_none[1].get("cache_control").is_none());
        let msgs_none = req_none["messages"].as_array().expect("messages array");
        let last_msg_none = &msgs_none[msgs_none.len() - 1];
        let last_content_none = last_msg_none["content"].as_array().expect("content array");
        assert!(last_content_none[last_content_none.len() - 1].get("cache_control").is_none());
    }

    #[test]
    fn test_thinking_budgets_adaptive_and_disabled_modes() {
        let mut model = test_model();
        model.max_tokens = 4096;
        model.reasoning = true;

        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Hello".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };

        // 1. Clamping test
        let (adj_max, adj_budget) = adjust_max_tokens_for_thinking(
            Some(4096),
            4096,
            &ThinkingLevel::High,
            None,
        );
        assert_eq!(adj_max, 4096);
        assert_eq!(adj_budget, 4096 - MIN_ANSWER_TOKENS);
        assert_eq!(clamp_thinking_budget_to_answer_room(16384, 4096), 3072);

        let budget_opts = StreamOptions {
            max_tokens: Some(4096),
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let req_budget = build_anthropic_request(&model, &context, &budget_opts, false);
        assert_eq!(req_budget["max_tokens"], 4096);
        assert_eq!(req_budget["thinking"]["type"], "enabled");
        assert_eq!(req_budget["thinking"]["budget_tokens"], 3072);

        // 2. force_adaptive_thinking = Some(true)
        model.compat = Some(ModelCompat {
            force_adaptive_thinking: Some(true),
            ..Default::default()
        });
        let adaptive_opts = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            effort: Some(AnthropicEffort::High),
            thinking_display: Some(AnthropicThinkingDisplay::Summarized),
            ..Default::default()
        };
        let req_adaptive = build_anthropic_request(&model, &context, &adaptive_opts, false);
        assert_eq!(
            req_adaptive["thinking"],
            serde_json::json!({
                "type": "adaptive",
                "display": "summarized"
            })
        );
        assert_eq!(
            req_adaptive["output_config"],
            serde_json::json!({
                "effort": "high"
            })
        );

        // 3. Explicit off emitting {"type": "disabled"}
        let off_opts = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let req_off = build_anthropic_request(&model, &context, &off_opts, false);
        assert_eq!(
            req_off["thinking"],
            serde_json::json!({
                "type": "disabled"
            })
        );
        assert!(req_off.get("output_config").is_none());
    }

    #[test]
    fn test_oauth_token_switches_to_bearer_and_maps_claude_code_tool_names() {
        let oauth_key = "sk-ant-oat-test-token-12345";
        assert!(is_oauth_token(oauth_key));
        assert!(!is_oauth_token("sk-ant-api-regular-key"));

        let model = test_model();
        let declared_tools = vec![
            ToolDefinition {
                name: "read".to_string(),
                description: "Read file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "custom_tool".to_string(),
                description: "Custom".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];

        let context = Context {
            system_prompt: Some("Custom system prompt".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Read file please".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_123".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    })],
                    api: "anthropic-messages".to_string(),
                    provider: "anthropic".to_string(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: 2,
                }),
            ],
            tools: Some(declared_tools.clone()),
        };

        let opts = StreamOptions {
            api_key: Some(oauth_key.to_string()),
            ..Default::default()
        };

        // Tool name mapping: read -> Read in request
        assert_eq!(to_claude_code_name("read"), "Read");
        assert_eq!(to_claude_code_name("BASH"), "Bash");
        assert_eq!(to_claude_code_name("custom_tool"), "custom_tool");

        let req = build_anthropic_request(&model, &context, &opts, true);

        // System prompt starts with Claude Code identity
        let sys_arr = req["system"].as_array().expect("system array");
        assert_eq!(
            sys_arr[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(sys_arr[1]["text"], "Custom system prompt");

        // Tool name mapped to "Read" in tools definition
        let tools = req["tools"].as_array().expect("tools array");
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[1]["name"], "custom_tool");

        // Tool name in assistant message mapped to "Read"
        let msgs = req["messages"].as_array().expect("messages array");
        assert_eq!(msgs[1]["content"][0]["name"], "Read");

        // Beta headers: claude-code-20250219,oauth-2025-04-20
        let beta_headers = resolve_anthropic_beta_headers(&model, &opts, true);
        assert!(beta_headers.contains("claude-code-20250219"));
        assert!(beta_headers.contains("oauth-2025-04-20"));

        // Reverse tool name mapping in streamed ToolCall: Read -> read
        assert_eq!(from_claude_code_name("Read", &declared_tools), "read");
        assert_eq!(from_claude_code_name("custom_tool", &declared_tools), "custom_tool");

        let mut state = AnthropicStreamState::new(&model);
        state.set_oauth(true);
        state.set_declared_tools(declared_tools);

        let sse_event = SseEvent {
            event: Some("content_block_start".to_string()),
            data: serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "Read",
                    "input": {}
                }
            }).to_string(),
        };
        let mut batch = Vec::new();
        state.process_event_into(&sse_event, &mut batch);
        assert_eq!(batch.len(), 1);
        match &batch[0] {
            AssistantMessageEvent::ToolCallStart { partial, .. } => {
                match &partial.content[0] {
                    AssistantContentBlock::ToolCall(tc) => {
                        assert_eq!(tc.name, "read");
                    }
                    _ => panic!("expected ToolCall block"),
                }
            }
            _ => panic!("expected ToolCallStart event"),
        }
    }

    #[test]
    fn test_build_anthropic_headers_avoids_duplicate_beta_header() {
        let model = test_model();
        let mut model_with_headers = model.clone();
        let mut m_headers = HashMap::new();
        m_headers.insert("anthropic-beta".to_string(), "custom-beta-1".to_string());
        m_headers.insert("x-custom-model-header".to_string(), "val1".to_string());
        model_with_headers.headers = Some(m_headers);

        let mut opts_headers = HashMap::new();
        opts_headers.insert("anthropic-beta".to_string(), "custom-beta-2".to_string());
        opts_headers.insert("x-custom-opt-header".to_string(), "val2".to_string());
        let options = StreamOptions {
            api_key: Some("test-key".to_string()),
            headers: Some(opts_headers),
            ..Default::default()
        };

        let headers = build_anthropic_headers(&model_with_headers, &options, false);

        let beta_headers: Vec<&(String, String)> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .collect();

        assert_eq!(beta_headers.len(), 1, "anthropic-beta header must appear exactly once");
        assert!(headers.iter().any(|(k, v)| k == "x-custom-model-header" && v == "val1"));
        assert!(headers.iter().any(|(k, v)| k == "x-custom-opt-header" && v == "val2"));
    }

    #[test]
    fn test_resolve_anthropic_beta_headers_deduplicates_non_consecutive() {
        let model = test_model();
        let mut opts_headers = HashMap::new();
        opts_headers.insert(
            "anthropic-beta".to_string(),
            "beta-1,beta-2,beta-1,beta-3,beta-2,beta-1".to_string(),
        );
        let options = StreamOptions {
            headers: Some(opts_headers),
            ..Default::default()
        };

        let result = resolve_anthropic_beta_headers(&model, &options, false);
        assert_eq!(result, "beta-1,beta-2,beta-3");
    }

    #[test]
    fn thinking_budget_never_exceeds_model_max_tokens() {
        let mut model = test_model();
        model.max_tokens = 1000;
        model.reasoning = true;

        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Hello".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };

        let req = build_anthropic_request(&model, &context, &options, false);
        assert_eq!(req["max_tokens"], 1000);
        assert!(
            req.get("thinking").is_none(),
            "a ceiling that cannot fit the 1024-token floor must omit thinking, got: {}",
            req["thinking"]
        );
    }

    #[test]
    fn adjust_max_tokens_for_thinking_saturates_on_overflow() {
        let (max_tokens, thinking_budget) = adjust_max_tokens_for_thinking(
            Some(u64::MAX),
            8_192,
            &ThinkingLevel::High,
            None,
        );
        assert_eq!(max_tokens, 8_192);
        assert_eq!(thinking_budget, 8_192 - MIN_ANSWER_TOKENS);
    }

    #[test]
    fn test_claude_code_name_roundtrip_snake_case() {
        assert_eq!(to_claude_code_name("web_search"), "WebSearch");
        assert_eq!(to_claude_code_name("web_fetch"), "WebFetch");

        let declared = vec![
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search".to_string(),
                parameters: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_fetch".to_string(),
                description: "Fetch".to_string(),
                parameters: serde_json::json!({}),
            },
        ];
        assert_eq!(from_claude_code_name("WebSearch", &declared), "web_search");
        assert_eq!(from_claude_code_name("WebFetch", &declared), "web_fetch");
    }
}
