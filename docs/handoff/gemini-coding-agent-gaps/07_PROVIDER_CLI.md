# Dump 07 - Provider constants va CLI

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-ai/src/anthropic.rs | 95.7 | 2344 | 114309f2ac26ac73e26b71a14c0b38fa2a1a484147880906e27e58efc2b5624a |
| crates/mehh-cli/src/main.rs | 71 | 1940 | 1bd756bba92555f1db83f154171c96ff001c18010b981f24f95f671978a99aab |

## crates/mehh-ai/src/anthropic.rs

`````rust
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
    if base.contains("/messages") {
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

/// Converts a tool name to its Claude Code canonical casing if matched.
pub fn to_claude_code_name(name: &str) -> String {
    for &tool in CLAUDE_CODE_TOOLS {
        if tool.eq_ignore_ascii_case(name) {
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
        Some(base) => (base + thinking_budget).min(model_max_tokens),
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
        let mut identity_block = serde_json::json!({
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
            let (mut adj_max_tokens, adj_budget) = adjust_max_tokens_for_thinking(
                options.max_tokens,
                model.max_tokens,
                reasoning_level,
                options.thinking_budgets.as_ref(),
            );
            let mut final_budget = options
                .thinking_budget_tokens
                .unwrap_or(adj_budget)
                .max(MIN_ANSWER_TOKENS);
            // Anthropic rejects budget_tokens >= max_tokens. Keep at least one
            // token of answer room, raising max_tokens if the floor forces it.
            if final_budget >= adj_max_tokens {
                final_budget = adj_max_tokens.saturating_sub(1);
            }
            if final_budget < MIN_ANSWER_TOKENS {
                final_budget = MIN_ANSWER_TOKENS;
                adj_max_tokens = MIN_ANSWER_TOKENS + 1;
            }

            req_map.insert("max_tokens".to_string(), serde_json::json!(adj_max_tokens));

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
}
`````

## crates/mehh-cli/src/main.rs

`````rust
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use clap::Parser;
use mehh_ai::{
    catalog, faux, LlmProvider, Model, ModelCost, ThinkingLevel,
};
use mehh_coding_agent::auth::resolve_api_key_async;
use mehh_coding_agent::models_config::ModelRegistry;
use mehh_coding_agent::trust::{resolve_project_trust_with_override, TrustOverride};
use mehh_coding_agent::{
    process_file_arguments, ProcessedPromptArgs, list_sessions, perform_login, perform_logout, rpc, run_interactive_mode, run_print_mode_content,
    AgentSession, McpManager, OutputMode, SessionManager, SessionSummary, Settings,
    SettingsManager,
};
use tokio::io::AsyncBufReadExt;

#[derive(clap::Parser, Debug, Clone, PartialEq, Eq)]
#[command(name = "mehh", version, about = "mehh coding agent CLI")]
pub struct CliArgs {
    pub prompt: Vec<String>,

    #[arg(short = 'p', long = "print")]
    pub print: bool,

    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    #[arg(
        short = 'r',
        long = "resume",
        num_args = 0..=1,
        require_equals = true,
        value_name = "SESSION"
    )]
    pub resume: Option<Option<String>>,

    #[arg(long = "login", value_name = "PROVIDER")]
    pub login: Option<String>,

    #[arg(long = "logout", value_name = "PROVIDER")]
    pub logout: Option<String>,

    #[arg(long = "mode", default_value = "text")]
    pub mode: String,

    #[arg(long = "permission-mode", value_name = "MODE")]
    pub permission_mode: Option<String>,

    #[arg(long = "worker-model", value_name = "MODEL")]
    pub worker_model: Option<String>,

    #[arg(long = "lead-turns", value_name = "TURNS")]
    pub lead_turns: Option<usize>,

    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    #[arg(long = "provider")]
    pub provider: Option<String>,

    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    #[arg(long = "thinking")]
    pub thinking: Option<String>,

    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    #[arg(long = "append-system-prompt")]
    pub append_system_prompt: Option<String>,

    #[arg(long = "session")]
    pub session: Option<PathBuf>,

    #[arg(long = "no-session")]
    pub no_session: bool,

    #[arg(long = "no-tools")]
    pub no_tools: bool,

    #[arg(long = "no-mcp")]
    pub no_mcp: bool,

    #[arg(long = "codemode")]
    pub codemode: bool,

    #[arg(long = "list-models")]
    pub list_models: bool,

    #[arg(long = "trust")]
    pub trust: bool,

    #[arg(long = "name", value_name = "NAME")]
    pub name: Option<String>,

    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,

    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    #[arg(long = "fork", value_name = "ENTRY_ID")]
    pub fork: Option<String>,

    #[arg(long = "models", value_name = "MODELS")]
    pub models: Option<String>,

    #[arg(long = "tools", value_name = "TOOLS")]
    pub tools: Option<String>,

    #[arg(long = "exclude-tools", value_name = "TOOLS")]
    pub exclude_tools: Option<String>,

    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,

    #[arg(long = "export", value_name = "SESSION_FILE")]
    pub export: Option<PathBuf>,

    #[arg(long = "skills")]
    pub skills: bool,

    #[arg(long = "no-skills")]
    pub no_skills: bool,

    #[arg(long = "prompt-templates")]
    pub prompt_templates: bool,

    #[arg(long = "no-prompt-templates")]
    pub no_prompt_templates: bool,

    #[arg(long = "use-theme", value_name = "THEME")]
    pub use_theme: Option<String>,

    #[arg(long = "no-themes")]
    pub no_themes: bool,

    #[arg(long = "no-context-files")]
    pub no_context_files: bool,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[arg(long = "approve")]
    pub approve: bool,

    #[arg(long = "no-approve")]
    pub no_approve: bool,
}

impl CliArgs {
    pub fn validate(&self) -> anyhow::Result<()> {
        if (self.approve || self.trust) && self.no_approve {
            anyhow::bail!("Cannot specify both --approve/--trust and --no-approve");
        }
        if self.no_session
            && (self.continue_session || self.resume.is_some() || self.session.is_some())
        {
            anyhow::bail!(
                "Cannot specify --no-session with session resumption or session file flags"
            );
        }
        if self.continue_session && self.session.is_some() {
            anyhow::bail!("Cannot specify both -c/--continue and --session");
        }
        if self.continue_session && self.resume.is_some() {
            anyhow::bail!("Cannot specify both -c/--continue and -r/--resume");
        }
        if self.session_dir.is_some()
            && (self.no_session
                || self.session.is_some()
                || self.continue_session
                || self.resume.is_some())
        {
            anyhow::bail!(
                "--session-dir cannot be combined with --no-session, --session, -c/--continue, or -r/--resume"
            );
        }
        if self.skills && self.no_skills {
            anyhow::bail!("Cannot specify both --skills and --no-skills");
        }
        if self.prompt_templates && self.no_prompt_templates {
            anyhow::bail!("Cannot specify both --prompt-templates and --no-prompt-templates");
        }
        if self.use_theme.is_some() && self.no_themes {
            anyhow::bail!("Cannot specify both --use-theme and --no-themes");
        }
        Ok(())
    }

    pub fn trust_override(&self) -> TrustOverride {
        if self.no_approve {
            TrustOverride::NoApprove
        } else if self.approve || self.trust {
            TrustOverride::Approve
        } else {
            TrustOverride::None
        }
    }
}

pub fn resolve_cli_tools(
    args: &CliArgs,
    cwd: PathBuf,
) -> anyhow::Result<Vec<Arc<dyn mehh_agent::AgentTool>>> {
    if args.no_tools || args.no_builtin_tools {
        return Ok(Vec::new());
    }
    let include = args.tools.as_deref().map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
    });
    let exclude = args
        .exclude_tools
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    mehh_tools::select_coding_tools(cwd, include.as_deref(), &exclude)
}


pub fn handle_export(
    input_file: &Path,
    output_file: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<()> {
    mehh_coding_agent::session::export_session(input_file, output_file, cwd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Text,
    Json,
    Rpc,
    Interactive,
}

pub fn parse_cli_mode(s: &str) -> anyhow::Result<CliMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(CliMode::Text),
        "json" => Ok(CliMode::Json),
        "rpc" => Ok(CliMode::Rpc),
        "interactive" | "tui" => Ok(CliMode::Interactive),
        other => anyhow::bail!(
            "Invalid mode '{}'. Allowed: text, json, rpc, interactive",
            other
        ),
    }
}

pub fn should_run_interactive(
    args: &CliArgs,
    cli_mode: CliMode,
    stdin_is_terminal: bool,
) -> bool {
    cli_mode == CliMode::Interactive
        || (cli_mode == CliMode::Text && !args.print && args.prompt.is_empty() && stdin_is_terminal)
}

pub fn parse_thinking_level(s: &str) -> anyhow::Result<ThinkingLevel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" | "x-high" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => anyhow::bail!(
            "Invalid thinking level '{}'. Allowed: off, minimal, low, medium, high, xhigh, max",
            other
        ),
    }
}

/// Returns the request format used for a provider's API.
pub fn api_for_provider(provider: &str) -> &'static str {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => "anthropic-messages",
        "google" | "gemini" => "google-generative-ai",
        "google-vertex" => "google-vertex",
        "openai-responses" => "openai-responses",
        "mistral" => "mistral-conversations",
        "azure" | "azure-openai-responses" => "azure-openai-responses",
        "amazon-bedrock" | "bedrock" => "bedrock-converse-stream",
        _ => "openai-completions",
    }
}

/// Returns the default base URL for a provider.
///
/// Azure deployments carry the resource name in their URL and therefore have no
/// generic default; they fall back to the OpenAI endpoint.
pub fn base_url_for_provider(provider: &str) -> String {
    let provider = provider.to_ascii_lowercase();
    if let Some(url) = catalog::default_base_url(&provider) {
        return url.to_string();
    }
    match provider.as_str() {
        "gemini" => "https://generativelanguage.googleapis.com/v1beta".to_string(),
        "bedrock" => "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        "azure" | "azure-openai-responses" => std::env::var("AZURE_OPENAI_ENDPOINT")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        _ => "https://api.openai.com/v1".to_string(),
    }
}

/// Selects effective `(model_arg, provider_arg)` so an explicit `--model` is never overridden by `settings.default_provider`.
pub fn select_cli_model_and_provider<'a>(
    args_model: Option<&'a str>,
    args_provider: Option<&'a str>,
    settings: &'a Settings,
) -> (Option<&'a str>, Option<&'a str>) {
    if let Some(m) = args_model {
        (Some(m), args_provider)
    } else {
        (
            settings.default_model.as_deref(),
            args_provider.or(settings.default_provider.as_deref()),
        )
    }
}

/// Applies `--thinking` only when explicitly provided so `settings.default_thinking_level` is preserved otherwise.
pub fn apply_cli_thinking_override(
    session: &mut AgentSession,
    cli_thinking: Option<ThinkingLevel>,
) {
    if let Some(level) = cli_thinking {
        session.set_thinking_level(Some(level));
    }
}

/// Resolves a model from the built-in catalog or constructs a fallback custom `Model`.
pub fn resolve_or_custom_model(
    model_arg: Option<&str>,
    provider_arg: Option<&str>,
    default_provider: Option<&str>,
) -> Option<Model> {
    resolve_or_custom_model_with_registry(
        &ModelRegistry::builtin_only(),
        model_arg,
        provider_arg,
        default_provider,
    )
}

/// Resolves a model from `registry` (built-in catalog + `models.json`) or constructs a fallback custom `Model`.
pub fn resolve_or_custom_model_with_registry(
    registry: &ModelRegistry,
    model_arg: Option<&str>,
    provider_arg: Option<&str>,
    default_provider: Option<&str>,
) -> Option<Model> {
    if let Some(m) = model_arg {
        let resolved = if let Some(p) = provider_arg {
            registry.resolve(p, Some(m))
        } else {
            registry.resolve(m, None)
        };

        if let Some(model) = resolved {
            return Some(model);
        }

        // Custom / unlisted fallback model: infer provider from explicit flag, slash prefix, or model prefix
        // before falling back to `default_provider`.
        let lower_m = m.to_ascii_lowercase();
        let (provider, id) = if let Some(p) = provider_arg {
            (p.to_ascii_lowercase(), m.to_string())
        } else if let Some((p, rest)) = m.split_once('/') {
            (p.to_ascii_lowercase(), rest.to_string())
        } else if lower_m.starts_with("claude-") {
            ("anthropic".to_string(), m.to_string())
        } else if lower_m.starts_with("gpt-")
            || lower_m.starts_with("chatgpt-")
            || lower_m == "o1"
            || lower_m.starts_with("o1-")
            || lower_m == "o3"
            || lower_m.starts_with("o3-")
            || lower_m == "o4"
            || lower_m.starts_with("o4-")
        {
            ("openai".to_string(), m.to_string())
        } else if lower_m.starts_with("gemini-") {
            ("google".to_string(), m.to_string())
        } else if lower_m.starts_with("mistral-")
            || lower_m.starts_with("codestral-")
            || lower_m.starts_with("pixtral-")
            || lower_m.starts_with("magistral-")
        {
            ("mistral".to_string(), m.to_string())
        } else if lower_m.starts_with("grok-") {
            ("xai".to_string(), m.to_string())
        } else if lower_m.starts_with("deepseek-") {
            ("deepseek".to_string(), m.to_string())
        } else if let Some(dp) = default_provider {
            (dp.to_ascii_lowercase(), m.to_string())
        } else {
            ("openai".to_string(), m.to_string())
        };

        let custom_cfg = registry.provider_config(&provider);
        let api = custom_cfg
            .and_then(|c| c.api.clone())
            .unwrap_or_else(|| api_for_provider(&provider).to_string());
        let base_url = custom_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| base_url_for_provider(&provider));
        let headers = custom_cfg.and_then(|c| c.headers.clone());
        if base_url.trim().is_empty() {
            return None;
        }

        let lower_id = id.to_ascii_lowercase();
        let reasoning = lower_id.starts_with("o1")
            || lower_id.starts_with("o3")
            || lower_id.starts_with("o4")
            || lower_id.contains("claude")
            || lower_id.contains("gemini-2.5")
            || lower_id.contains("gemini-3")
            || lower_id.starts_with("gpt-5")
            || lower_id.starts_with("deepseek-r1")
            || lower_id.starts_with("deepseek-reasoner")
            || lower_id.starts_with("grok-3")
            || lower_id.contains("magistral");

        Some(Model {
            id: id.clone(),
            name: id,
            api,
            provider,
            base_url,
            reasoning,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers,
            ..Default::default()
        })
    } else if let Some(p) = provider_arg {
        registry.resolve(p, None)
    } else if let Some(dp) = default_provider {
        registry.resolve(dp, None)
    } else {
        registry.resolve("claude-sonnet-4-5", None)
    }
}

/// Selects and creates a SessionManager based on CLI flags and default persistence.
pub fn create_session_manager(
    no_session: bool,
    session_path: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<SessionManager> {
    if no_session {
        Ok(SessionManager::in_memory(cwd))
    } else if let Some(path) = session_path {
        SessionManager::open_file(path, cwd)
    } else {
        SessionManager::create_default_file(cwd)
    }
}

/// Returns the display label of a stored session.
pub fn session_label(summary: &SessionSummary) -> String {
    summary
        .name
        .clone()
        .or_else(|| summary.first_prompt.clone())
        .unwrap_or_else(|| summary.id.clone())
}

/// Renders a numbered list of stored sessions.
pub fn format_session_list(sessions: &[SessionSummary]) -> String {
    sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            format!(
                "  {}) {} — {} ({})",
                idx + 1,
                s.timestamp,
                session_label(s),
                s.id
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolves a session file from a path or a session id (exact or prefix).
pub fn resolve_resume_path(
    sessions: &[SessionSummary],
    spec: &str,
) -> anyhow::Result<PathBuf> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Session identifier cannot be empty");
    }

    let candidate = PathBuf::from(trimmed);
    if candidate.is_file() {
        return Ok(candidate);
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(found) = sessions
        .iter()
        .find(|s| s.id.to_ascii_lowercase() == lower)
    {
        return Ok(found.path.clone());
    }

    let prefixed: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| s.id.to_ascii_lowercase().starts_with(&lower))
        .collect();

    match prefixed.len() {
        1 => Ok(prefixed[0].path.clone()),
        0 => anyhow::bail!("No session found matching '{trimmed}'"),
        _ => anyhow::bail!(
            "Session id '{trimmed}' is ambiguous ({} sessions match)",
            prefixed.len()
        ),
    }
}

/// Parses a 1-based session selection typed by the user.
pub fn parse_session_choice(input: &str, count: usize) -> anyhow::Result<usize> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("No session selected (input closed)");
    }
    let idx: usize = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid session selection: '{trimmed}'"))?;
    if idx == 0 || idx > count {
        anyhow::bail!("Session selection out of range: {idx} (expected 1-{count})");
    }
    Ok(idx)
}

/// Builds the system prompt for a session from its final tool list (used by unit tests).
#[cfg(test)]
pub fn assemble_system_prompt(
    session: &AgentSession,
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
) -> String {
    let tool_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    mehh_coding_agent::build_system_prompt_with_skills(
        cwd,
        custom_prompt,
        append_prompt,
        &tool_names,
        session.skills(),
    )
}

/// Prints the stored sessions and reads a 1-based selection from stdin.
pub fn prompt_session_selection(sessions: &[SessionSummary]) -> anyhow::Result<PathBuf> {
    if sessions.is_empty() {
        anyhow::bail!("No saved sessions found for this directory.");
    }

    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "--resume needs a session id when stdin is not a terminal.\nAvailable sessions:\n{}",
            format_session_list(sessions)
        );
    }

    println!("Available sessions:");
    for (idx, s) in sessions.iter().enumerate() {
        println!("  {}) {} — {}", idx + 1, s.timestamp, session_label(s));
    }
    print!("Select session [1-{}]: ", sessions.len());
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = parse_session_choice(&line, sessions.len())?;
    Ok(sessions[choice - 1].path.clone())
}

/// Selects the session manager for this run, honouring the session related flags.
pub fn resolve_session_manager(args: &CliArgs, cwd: &Path) -> anyhow::Result<SessionManager> {
    let mut sm = if args.no_session {
        create_session_manager(true, None, cwd)?
    } else if let Some(path) = args.session.as_deref() {
        create_session_manager(false, Some(path), cwd)?
    } else if args.continue_session {
        let sessions = list_sessions(cwd)?;
        let latest = sessions.first().ok_or_else(|| {
            anyhow::anyhow!("No previous session found for {}", cwd.display())
        })?;
        create_session_manager(false, Some(&latest.path), cwd)?
    } else if let Some(spec) = args.resume.as_ref() {
        let sessions = list_sessions(cwd)?;
        let path = match spec {
            Some(target) => resolve_resume_path(&sessions, target)?,
            None => prompt_session_selection(&sessions)?,
        };
        create_session_manager(false, Some(&path), cwd)?
    } else if let Some(id) = args.session_id.as_deref() {
        // --session-id works on its own, not only together with --session-dir.
        if let Some(dir) = args.session_dir.as_deref() {
            let ts = mehh_ai::now_ms();
            let path = dir.join(format!("{ts}_{id}.jsonl"));
            SessionManager::open_file(path, cwd)?
        } else {
            let mut mgr = create_session_manager(false, None, cwd)?;
            mgr.set_session_name(Some(id.to_string()))?;
            mgr
        }
    } else if let Some(dir) = args.session_dir.as_deref() {
        let ts = mehh_ai::now_ms();
        let path = dir.join(format!("{ts}_default.jsonl"));
        SessionManager::open_file(path, cwd)?
    } else {
        create_session_manager(false, None, cwd)?
    };

    if let Some(ref fork_entry_id) = args.fork {
        sm = sm.fork_at(Some(fork_entry_id.as_str()))?;
    }
    if let Some(ref name) = args.name {
        sm.set_session_name(Some(name.clone()))?;
    }
    Ok(sm)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();
    args.validate()?;

    // Validate CLI mode and thinking level early before loading sessions, credentials, or MCP.
    let cli_mode = parse_cli_mode(&args.mode)?;
    let cli_thinking = args
        .thinking
        .as_deref()
        .map(parse_thinking_level)
        .transpose()?;

    // Credential management short-circuits before a session or trust check is created.
    if let Some(provider) = args.login.as_deref() {
        let message = perform_login(provider, args.api_key.as_deref(), None).await?;
        println!("{message}");
        return Ok(());
    }
    if let Some(provider) = args.logout.as_deref() {
        let removed = perform_logout(provider, None)?;
        if removed {
            println!("Removed stored credential for {provider}");
        } else {
            println!("No stored credential for {provider}");
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;

    if let Some(ref export_input) = args.export {
        let export_output = args.prompt.first().map(Path::new);
        return handle_export(export_input, export_output, &cwd);
    }

    let model_registry = ModelRegistry::load_default()?;
    if args.list_models {
        for m in model_registry.models() {
            println!(
                "{}/{} - {} ({} tokens)",
                m.provider, m.id, m.name, m.context_window
            );
        }
        return Ok(());
    }

    let is_interactive = should_run_interactive(&args, cli_mode, io::stdin().is_terminal());
    let trusted = resolve_project_trust_with_override(&cwd, args.trust_override(), is_interactive)?;
    let mut settings = SettingsManager::try_load_with_trust(&cwd, trusted)?;
    if let Some(ref m) = args.permission_mode {
        let mode = m
            .parse::<mehh_coding_agent::PermissionMode>()
            .map_err(|e| anyhow::anyhow!(e))?;
        settings.permission_mode = Some(mode);
    }
    if let Some(ref wm) = args.worker_model {
        settings.worker_model = Some(wm.clone());
    }
    if let Some(lt) = args.lead_turns {
        settings.lead_turns = Some(lt);
    }

    let (model, provider): (Model, Arc<dyn LlmProvider>) = {
        let is_faux = args
            .provider
            .as_deref()
            .map(|p| p.eq_ignore_ascii_case("faux"))
            .unwrap_or(false)
            || args
                .model
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("faux") || m.eq_ignore_ascii_case("faux-1"))
                .unwrap_or(false);

        if is_faux {
            let model = faux::faux_model();
            let faux_prov = faux::FauxProvider::new();
            faux_prov.add_response(faux::faux_assistant_message(
                "Hello from mehh Rust faux provider!",
            ));
            (model, Arc::new(faux_prov))
        } else {
            let (model_arg, provider_arg) = select_cli_model_and_provider(
                args.model.as_deref(),
                args.provider.as_deref(),
                &settings,
            );

            let model = resolve_or_custom_model_with_registry(
                &model_registry,
                model_arg,
                provider_arg,
                settings.default_provider.as_deref(),
            )
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to resolve model (model: {:?}, provider: {:?})",
                    args.model,
                    args.provider
                )
            })?;

            let provider = AgentSession::default_provider_for_model(&model);
            (model, provider)
        }
    };

    let session_mgr = resolve_session_manager(&args, &cwd)?;

    let mut agent_session = AgentSession::new_with_trust(
        cwd.clone(),
        model.clone(),
        provider.clone(),
        session_mgr,
        settings,
        trusted,
    );

    if args.no_context_files {
        agent_session.set_no_context_files(true);
    }
    if args.verbose {
        eprintln!(
            "model={} provider={} api={} base_url={}",
            model.id, model.provider, model.api, model.base_url
        );
    }
    if args.skills && agent_session.skills.is_empty() {
        eprintln!("note: --skills is enabled by default; no project skills were found");
    }
    if args.prompt_templates && agent_session.prompt_templates.is_empty() {
        eprintln!("note: --prompt-templates is enabled by default; no templates were found");
    }

    // Subagent tools are built after MCP connection so child agents see MCP tools.
    let subagent_models = if args.no_tools || args.no_builtin_tools {
        None
    } else {
        let enabled = match args.tools.as_deref() {
            Some(inc) => inc.split(',').any(|s| s.trim() == "subagent"),
            None => !args
                .exclude_tools
                .as_deref()
                .unwrap_or("")
                .split(',')
                .any(|s| s.trim() == "subagent"),
        };
        if enabled {
            let lw = agent_session.lead_worker.lock().unwrap();
            Some((
                lw.worker_model().cloned().unwrap_or_else(|| model.clone()),
                lw.worker_provider().cloned().unwrap_or_else(|| provider.clone()),
            ))
        } else {
            None
        }
    };
    if args.no_tools || args.no_builtin_tools || args.tools.is_some() || args.exclude_tools.is_some() {
        let mut cli_tools = resolve_cli_tools(&args, cwd.clone())?;
        let should_include_todo = if args.no_tools || args.no_builtin_tools {
            false
        } else if let Some(ref inc) = args.tools {
            inc.split(',').any(|s| s.trim() == "todo")
        } else {
            !args
                .exclude_tools
                .as_deref()
                .unwrap_or("")
                .split(',')
                .any(|s| s.trim() == "todo")
        };
        if should_include_todo {
            cli_tools.push(Arc::new(mehh_coding_agent::todo::TodoTool::new(agent_session.todo_state.clone())));
        }

        agent_session.set_tools(cli_tools);
    }

    if let Some(ref models_str) = args.models {
        let mut resolved = Vec::new();
        for token in models_str.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
            // IDs may contain multiple slashes (e.g. openrouter/anthropic/claude-sonnet-4.5).
            let model_opt = if let Some((provider, id)) = token.split_once('/') {
                model_registry
                    .resolve_exact(provider, id)
                    .or_else(|| model_registry.resolve(token, None))
            } else {
                model_registry.resolve(token, None)
            };
            match model_opt {
                Some(m) => resolved.push(m),
                None => eprintln!("warning: unknown model '{token}' in --models; skipped"),
            }
        }
        agent_session.set_scoped_models(resolved);
    }

    if args.no_skills {
        agent_session.skills.clear();
        agent_session.rebuild_system_prompt();
    }
    if args.no_prompt_templates {
        agent_session.prompt_templates.clear();
    }

    if args.no_themes {
        agent_session.settings.theme = Some("dark".to_string());
    } else if let Some(ref th) = args.use_theme {
        agent_session.settings.theme = Some(th.clone());
    }

    // Resolve the key asynchronously so expired OAuth tokens are refreshed first.
    let has_explicit_api_key = args
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    // Key refresh (which may perform HTTP) and MCP startup are independent.
    // join! (not try_join!) so a failing key refresh cannot drop an MCP
    // connect future that may already have spawned child processes.
    let (api_key_resolved, mcp_resolved) = tokio::join!(
        resolve_api_key_async(&model.provider, args.api_key.as_deref(), None::<&Path>),
        async {
            if !args.no_tools && !args.no_mcp {
                McpManager::connect_from_configs_with_trust(&cwd, trusted)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            }
        }
    );
    let mcp_manager = mcp_resolved?;
    let api_key_base = match api_key_resolved {
        Ok(key) => key,
        Err(err) => {
            // join! already let MCP finish connecting; close its child processes
            // before propagating the credential error.
            if let Some(ref mgr) = mcp_manager {
                mgr.close_all().await;
            }
            return Err(err);
        }
    };
    let api_key = api_key_base.or_else(|| {
        model_registry
            .provider_api_key(&model.provider)
            .map(str::to_string)
    });
    if has_explicit_api_key {
        agent_session.set_explicit_api_key(api_key);
    } else {
        agent_session.set_api_key(api_key);
    }
    apply_cli_thinking_override(&mut agent_session, cli_thinking);

    let mut mcp_manager = mcp_manager;
    let mcp_tools = if let Some(ref mgr) = mcp_manager {
        if !is_interactive {
            for summary in mgr.summaries() {
                if let Some(ref err) = summary.error {
                    eprintln!(
                        "Warning: MCP server '{}' failed to connect: {}",
                        summary.server, err
                    );
                }
            }
        }
        mgr.tools().to_vec()
    } else {
        Vec::new()
    };

    if !args.no_tools {
        agent_session.enable_mcp_and_codemode(mcp_tools, args.codemode);
    }

    if let Some((sub_model, sub_provider)) = subagent_models {
        // Rebuild the subagent tool with the final tool set (including MCP).
        let mut tools: Vec<Arc<dyn mehh_agent::AgentTool>> = agent_session
            .tools
            .iter()
            .filter(|t| t.name() != "subagent")
            .cloned()
            .collect();
        let subagent = Arc::new(
            mehh_coding_agent::subagent::SubagentTool::new(
                cwd.clone(),
                sub_model,
                sub_provider,
                None,
                tools.clone(),
                0,
            )
            .with_inspection_manager(agent_session.inspection_manager.clone())
            .with_approval_handler(agent_session.approval_handler.clone()),
        );
        tools.push(subagent);
        agent_session.set_tools(tools);
    }

    agent_session.set_custom_system_prompt(
        args.system_prompt.clone(),
        args.append_system_prompt.clone(),
    );

    if is_interactive {
        let summaries = mcp_manager
            .as_ref()
            .map(|mgr| mgr.summaries().to_vec())
            .unwrap_or_default();
        let res = run_interactive_mode(
            &mut agent_session,
            &summaries,
            mcp_manager.as_mut(),
            args.codemode,
        )
        .await;
        if let Some(mgr) = &mcp_manager {
            let _ = mgr.close_all().await;
        }
        return res;
    }

    let run_res = match cli_mode {
        CliMode::Rpc => {
            rpc::run_rpc_mode(
                &mut agent_session,
                tokio::io::BufReader::new(tokio::io::stdin()),
                tokio::io::stdout(),
            )
            .await
        }
        CliMode::Text | CliMode::Interactive => {
            run_print_flow(&mut agent_session, &args, OutputMode::Text).await
        }
        CliMode::Json => {
            run_print_flow(&mut agent_session, &args, OutputMode::Json).await
        }
    };

    if let Some(mgr) = &mcp_manager {
        let _ = mgr.close_all().await;
    }

    run_res
}

/// Reads piped stdin. When a prompt argument is present, stdin is supplementary,
/// so an idle pipe must not block the run forever; an idle window of 500ms is
/// enough to decide there is nothing to append. Without a prompt, stdin is the
/// prompt itself, so wait for EOF as before.
async fn read_piped_stdin(supplementary: bool) -> anyhow::Result<String> {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    loop {
        let fill = reader.fill_buf();
        let buf = if supplementary {
            match tokio::time::timeout(Duration::from_millis(500), fill).await {
                Ok(result) => result?,
                Err(_) => break,
            }
        } else {
            fill.await?
        };
        if buf.is_empty() {
            break;
        }
        let len = buf.len();
        bytes.extend_from_slice(buf);
        reader.consume(len);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn run_print_flow(
    agent_session: &mut AgentSession,
    args: &CliArgs,
    output_mode: OutputMode,
) -> anyhow::Result<()> {
    let processed = if args.no_context_files {
        ProcessedPromptArgs {
            text: args.prompt.join(" "),
            blocks: Vec::new(),
        }
    } else {
        process_file_arguments(&agent_session.cwd, &args.prompt)?
    };
    let mut prompt_text = processed.text;
    let mut blocks = processed.blocks;
    // Piped stdin is added to the prompt argument, never discarded. For
    // example `git diff | mehh -p "review"` must include both pieces.
    if !io::stdin().is_terminal() {
        let stdin_input = read_piped_stdin(args.prompt.is_empty()).await?;
        let piped = stdin_input.trim_start_matches('\u{feff}');
        if !piped.trim().is_empty() {
            if !prompt_text.trim().is_empty() {
                prompt_text.push_str("\n\n");
            }
            prompt_text.push_str(piped);
        }
    }

    if prompt_text.trim().is_empty() && blocks.is_empty() {
        anyhow::bail!(
            "Prompt is required. Pass a prompt argument, pipe input via stdin, or use --mode rpc."
        );
    }

    let user_content = if blocks.is_empty() {
        mehh_ai::UserContent::Text(prompt_text)
    } else {
        if !prompt_text.trim().is_empty() {
            let has_text = blocks.iter().any(|b| matches!(b, mehh_ai::UserContentBlock::Text(_)));
            if !has_text {
                blocks.insert(
                    0,
                    mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                        text: prompt_text,
                        text_signature: None,
                    }),
                );
            }
        }
        mehh_ai::UserContent::Blocks(blocks)
    };

    run_print_mode_content(agent_session, user_content, output_mode).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn make_session_summary(id: &str, path: &str) -> SessionSummary {
        SessionSummary {
            path: PathBuf::from(path),
            id: id.to_string(),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            entry_count: 3,
            name: None,
            first_prompt: Some("first prompt".to_string()),
        }
    }

    #[test]
    fn test_cli_args_print_and_prompt() {
        let args = CliArgs::try_parse_from(["mehh", "-p", "write a function"]).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt, vec!["write a function"]);
        assert_eq!(args.mode, "text");
        assert!(!args.list_models);
        assert!(!args.no_mcp);
        assert!(!args.codemode);
    }

    #[test]
    fn test_cli_args_no_mcp_and_codemode_flags() {
        let args_default = CliArgs::try_parse_from(["mehh", "test query"]).unwrap();
        assert!(!args_default.no_mcp);
        assert!(!args_default.codemode);

        let args_no_mcp = CliArgs::try_parse_from(["mehh", "--no-mcp", "test query"]).unwrap();
        assert!(args_no_mcp.no_mcp);
        assert!(!args_no_mcp.codemode);

        let args_codemode = CliArgs::try_parse_from(["mehh", "--codemode", "test query"]).unwrap();
        assert!(!args_codemode.no_mcp);
        assert!(args_codemode.codemode);

        let args_both = CliArgs::try_parse_from(["mehh", "--no-mcp", "--codemode", "test query"]).unwrap();
        assert!(args_both.no_mcp);
        assert!(args_both.codemode);
    }

    #[test]
    fn test_cli_args_mode_interactive_and_tui() {
        let args_inter = CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert_eq!(args_inter.mode, "interactive");
        assert_eq!(parse_cli_mode(&args_inter.mode).unwrap(), CliMode::Interactive);

        let args_tui = CliArgs::try_parse_from(["mehh", "--mode", "tui"]).unwrap();
        assert_eq!(args_tui.mode, "tui");
        assert_eq!(parse_cli_mode(&args_tui.mode).unwrap(), CliMode::Interactive);
    }

    #[test]
    fn test_cli_args_mode_json() {
        let args = CliArgs::try_parse_from(["mehh", "--mode", "json", "-p", "explain code"]).unwrap();
        assert_eq!(args.mode, "json");
        assert!(args.print);
        assert_eq!(args.prompt, vec!["explain code"]);
    }

    #[test]
    fn test_cli_args_mode_rpc() {
        let args = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        assert_eq!(args.mode, "rpc");
        assert!(args.prompt.is_empty());
    }

    #[test]
    fn test_cli_args_model_and_provider() {
        let args = CliArgs::try_parse_from(["mehh", "-m", "gpt-4o", "--provider", "openai", "hello"]).unwrap();
        assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert_eq!(args.prompt, vec!["hello"]);
    }

    #[test]
    fn test_cli_args_list_models() {
        let args = CliArgs::try_parse_from(["mehh", "--list-models"]).unwrap();
        assert!(args.list_models);
        assert!(args.prompt.is_empty());
    }

    #[test]
    fn test_cli_args_session_and_tools() {
        let args = CliArgs::try_parse_from([
            "mehh",
            "--session",
            "session.json",
            "--no-tools",
            "test query",
        ])
        .unwrap();
        assert_eq!(args.session, Some(PathBuf::from("session.json")));
        assert!(args.no_tools);
        assert!(!args.no_session);
        assert_eq!(args.prompt, vec!["test query"]);
    }

    #[test]
    fn test_cli_args_no_session() {
        let args = CliArgs::try_parse_from(["mehh", "--no-session", "test query"]).unwrap();
        assert!(args.no_session);
        assert!(args.session.is_none());
    }

    #[test]
    fn test_cli_args_no_session_vs_default() {
        let args_default = CliArgs::try_parse_from(["mehh", "test query"]).unwrap();
        assert!(!args_default.no_session);
        assert!(args_default.session.is_none());

        let args_no_session = CliArgs::try_parse_from(["mehh", "--no-session", "test query"]).unwrap();
        assert!(args_no_session.no_session);
        assert!(args_no_session.session.is_none());

        let args_session = CliArgs::try_parse_from(["mehh", "--session", "my_session.jsonl", "test query"]).unwrap();
        assert!(!args_session.no_session);
        assert_eq!(args_session.session, Some(PathBuf::from("my_session.jsonl")));
    }

    #[test]
    fn test_cli_args_extended_options() {
        let args = CliArgs::try_parse_from([
            "mehh",
            "--thinking",
            "high",
            "--system-prompt",
            "custom prompt",
            "--append-system-prompt",
            "extra rules",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        assert_eq!(args.thinking.as_deref(), Some("high"));
        assert_eq!(args.system_prompt.as_deref(), Some("custom prompt"));
        assert_eq!(args.append_system_prompt.as_deref(), Some("extra rules"));
        assert_eq!(args.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn test_cli_args_continue_flag() {
        let args = CliArgs::try_parse_from(["mehh", "-c"]).unwrap();
        assert!(args.continue_session);
        assert!(args.resume.is_none());
        assert!(args.login.is_none());
        assert!(args.logout.is_none());

        let args_long = CliArgs::try_parse_from(["mehh", "--continue"]).unwrap();
        assert!(args_long.continue_session);
    }

    #[test]
    fn test_cli_args_resume_flag() {
        let without_value = CliArgs::try_parse_from(["mehh", "--resume"]).unwrap();
        assert_eq!(without_value.resume, Some(None));

        let short_without_value = CliArgs::try_parse_from(["mehh", "-r"]).unwrap();
        assert_eq!(short_without_value.resume, Some(None));

        let with_value = CliArgs::try_parse_from(["mehh", "--resume=abc123"]).unwrap();
        assert_eq!(with_value.resume, Some(Some("abc123".to_string())));

        let short_with_value = CliArgs::try_parse_from(["mehh", "-r=abc123"]).unwrap();
        assert_eq!(short_with_value.resume, Some(Some("abc123".to_string())));

        // The session id needs `=`, so a following word stays a prompt.
        let spaced = CliArgs::try_parse_from(["mehh", "--resume", "abc123"]).unwrap();
        assert_eq!(spaced.resume, Some(None));
        assert_eq!(spaced.prompt, vec!["abc123"]);
    }

    #[test]
    fn test_cli_args_login_and_logout_flags() {
        let login = CliArgs::try_parse_from(["mehh", "--login", "anthropic"]).unwrap();
        assert_eq!(login.login.as_deref(), Some("anthropic"));
        assert!(login.logout.is_none());

        let login_with_key = CliArgs::try_parse_from([
            "mehh",
            "--login",
            "anthropic",
            "--api-key",
            "sk-test",
        ])
        .unwrap();
        assert_eq!(login_with_key.login.as_deref(), Some("anthropic"));
        assert_eq!(login_with_key.api_key.as_deref(), Some("sk-test"));

        let logout = CliArgs::try_parse_from(["mehh", "--logout", "openai"]).unwrap();
        assert_eq!(logout.logout.as_deref(), Some("openai"));
        assert!(logout.login.is_none());
    }

    #[test]
    fn test_parse_cli_mode() {
        assert_eq!(parse_cli_mode("text").unwrap(), CliMode::Text);
        assert_eq!(parse_cli_mode("TEXT").unwrap(), CliMode::Text);
        assert_eq!(parse_cli_mode("json").unwrap(), CliMode::Json);
        assert_eq!(parse_cli_mode("JSON").unwrap(), CliMode::Json);
        assert_eq!(parse_cli_mode("rpc").unwrap(), CliMode::Rpc);
        assert_eq!(parse_cli_mode("RPC").unwrap(), CliMode::Rpc);
        assert_eq!(parse_cli_mode("interactive").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("INTERACTIVE").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("tui").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("TUI").unwrap(), CliMode::Interactive);
        assert!(parse_cli_mode("yaml").is_err());
    }

    #[test]
    fn test_should_run_interactive_default_text_mode_with_tty() {
        // When mode is text, no prompt, print is false, and stdin is a terminal -> true
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        assert_eq!(args.mode, "text");
        assert!(args.prompt.is_empty());
        assert!(!args.print);

        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(should_run_interactive(&args, cli_mode, true));
    }

    #[test]
    fn test_should_run_interactive_piped_stdin() {
        // When mode is text and no prompt, but stdin is NOT a terminal (piped) -> false
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_with_prompt_arg() {
        // When prompt is supplied in text mode, even on TTY -> false
        let args = CliArgs::try_parse_from(["mehh", "do something"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true));
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_print_flag() {
        // When print flag -p is set in text mode, even on TTY with no prompt -> false
        let args = CliArgs::try_parse_from(["mehh", "-p"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true));
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_explicit_interactive_mode() {
        // Explicit interactive mode is always true regardless of TTY, prompt, or print
        let args1 = CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert!(should_run_interactive(&args1, CliMode::Interactive, true));
        assert!(should_run_interactive(&args1, CliMode::Interactive, false));

        let args2 = CliArgs::try_parse_from(["mehh", "--mode", "interactive", "-p", "some prompt"]).unwrap();
        assert!(should_run_interactive(&args2, CliMode::Interactive, true));
        assert!(should_run_interactive(&args2, CliMode::Interactive, false));

        let args3 = CliArgs::try_parse_from(["mehh", "--mode", "tui"]).unwrap();
        let mode3 = parse_cli_mode(&args3.mode).unwrap();
        assert_eq!(mode3, CliMode::Interactive);
        assert!(should_run_interactive(&args3, mode3, true));
        assert!(should_run_interactive(&args3, mode3, false));
    }

    #[test]
    fn test_should_run_interactive_json_and_rpc_modes() {
        // Json mode never runs interactive mode
        let args_json = CliArgs::try_parse_from(["mehh", "--mode", "json"]).unwrap();
        let mode_json = parse_cli_mode(&args_json.mode).unwrap();
        assert_eq!(mode_json, CliMode::Json);
        assert!(!should_run_interactive(&args_json, mode_json, true));
        assert!(!should_run_interactive(&args_json, mode_json, false));

        // Rpc mode never runs interactive mode
        let args_rpc = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        let mode_rpc = parse_cli_mode(&args_rpc.mode).unwrap();
        assert_eq!(mode_rpc, CliMode::Rpc);
        assert!(!should_run_interactive(&args_rpc, mode_rpc, true));
        assert!(!should_run_interactive(&args_rpc, mode_rpc, false));
    }

    #[test]
    fn test_parse_thinking_level() {
        assert_eq!(parse_thinking_level("off").unwrap(), ThinkingLevel::Off);
        assert_eq!(parse_thinking_level("minimal").unwrap(), ThinkingLevel::Minimal);
        assert_eq!(parse_thinking_level("low").unwrap(), ThinkingLevel::Low);
        assert_eq!(parse_thinking_level("medium").unwrap(), ThinkingLevel::Medium);
        assert_eq!(parse_thinking_level("high").unwrap(), ThinkingLevel::High);
        assert_eq!(parse_thinking_level("xhigh").unwrap(), ThinkingLevel::Xhigh);
        assert_eq!(parse_thinking_level("x-high").unwrap(), ThinkingLevel::Xhigh);
        assert_eq!(parse_thinking_level("max").unwrap(), ThinkingLevel::Max);
        assert!(parse_thinking_level("unknown").is_err());
    }

    #[test]
    fn test_resolve_or_custom_model_builtin() {
        let m = resolve_or_custom_model(Some("claude-sonnet-4-5"), None, None).unwrap();
        assert_eq!(m.id, "claude-sonnet-4-5");
        assert_eq!(m.provider, "anthropic");

        let m_openai = resolve_or_custom_model(Some("gpt-4o"), Some("openai"), None).unwrap();
        assert_eq!(m_openai.id, "gpt-4o");
        assert_eq!(m_openai.provider, "openai");
    }

    #[test]
    fn test_resolve_or_custom_model_fallback_with_explicit_provider() {
        let m = resolve_or_custom_model(Some("my-custom-model"), Some("anthropic"), None).unwrap();
        assert_eq!(m.id, "my-custom-model");
        assert_eq!(m.name, "my-custom-model");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.api, "anthropic-messages");
        assert_eq!(m.base_url, "https://api.anthropic.com/v1");
        assert_eq!(m.context_window, 128_000);
        assert_eq!(m.max_tokens, 16_384);
        assert!(!m.reasoning);

        let m_google = resolve_or_custom_model(Some("custom-gemini"), Some("google"), None).unwrap();
        assert_eq!(m_google.provider, "google");
        assert_eq!(m_google.api, "google-generative-ai");
        assert_eq!(m_google.base_url, "https://generativelanguage.googleapis.com/v1beta");

        let m_resp = resolve_or_custom_model(Some("my-model"), Some("openai-responses"), None).unwrap();
        assert_eq!(m_resp.provider, "openai-responses");
        assert_eq!(m_resp.api, "openai-responses");
    }

    #[test]
    fn test_resolve_or_custom_model_fallback_inferred_provider_and_reasoning() {
        // Claude prefix
        let m_claude = resolve_or_custom_model(Some("claude-future-v1"), None, None).unwrap();
        assert_eq!(m_claude.provider, "anthropic");
        assert_eq!(m_claude.api, "anthropic-messages");
        assert_eq!(m_claude.base_url, "https://api.anthropic.com/v1");
        assert!(m_claude.reasoning);

        // Gemini prefix
        let m_gemini = resolve_or_custom_model(Some("gemini-3.0-pro"), None, None).unwrap();
        assert_eq!(m_gemini.provider, "google");
        assert_eq!(m_gemini.api, "google-generative-ai");
        assert_eq!(m_gemini.base_url, "https://generativelanguage.googleapis.com/v1beta");
        assert!(m_gemini.reasoning);

        let m_gemini_25 = resolve_or_custom_model(Some("gemini-2.5-custom"), None, None).unwrap();
        assert!(m_gemini_25.reasoning);

        // Reasoning prefixes: o1, o3, o4
        let m_o1 = resolve_or_custom_model(Some("o1-custom"), None, None).unwrap();
        assert_eq!(m_o1.provider, "openai");
        assert_eq!(m_o1.api, "openai-completions");
        assert_eq!(m_o1.base_url, "https://api.openai.com/v1");
        assert!(m_o1.reasoning);

        let m_o3 = resolve_or_custom_model(Some("o3-custom"), None, None).unwrap();
        assert!(m_o3.reasoning);

        let m_o4 = resolve_or_custom_model(Some("o4-custom"), None, None).unwrap();
        assert!(m_o4.reasoning);

        // Inferred from settings default_provider
        let m_default_prov = resolve_or_custom_model(Some("custom-llm"), None, Some("gemini")).unwrap();
        assert_eq!(m_default_prov.provider, "gemini");
        assert_eq!(m_default_prov.api, "google-generative-ai");

        // Fallback to openai when no inference applies
        let m_openai_default = resolve_or_custom_model(Some("unlisted-model"), None, None).unwrap();
        assert_eq!(m_openai_default.provider, "openai");
        assert_eq!(m_openai_default.api, "openai-completions");
        assert_eq!(m_openai_default.base_url, "https://api.openai.com/v1");
        assert!(!m_openai_default.reasoning);

        // Slash notation
        let m_slash = resolve_or_custom_model(Some("custom-org/llama-70b"), None, None).unwrap();
        assert_eq!(m_slash.provider, "custom-org");
        assert_eq!(m_slash.id, "llama-70b");
        assert_eq!(m_slash.api, "openai-completions");
    }

    #[test]
    fn test_resolve_or_custom_model_none_model() {
        let m_default = resolve_or_custom_model(None, None, None).unwrap();
        assert_eq!(m_default.id, "claude-sonnet-4-5");

        let m_prov = resolve_or_custom_model(None, Some("openai"), None).unwrap();
        assert_eq!(m_prov.id, "gpt-4o");

        let m_dp = resolve_or_custom_model(None, None, Some("google")).unwrap();
        assert_eq!(m_dp.id, "gemini-2.5-pro");
    }

    #[test]
    fn test_api_for_provider_covers_every_request_format() {
        assert_eq!(api_for_provider("anthropic"), "anthropic-messages");
        assert_eq!(api_for_provider("google"), "google-generative-ai");
        assert_eq!(api_for_provider("gemini"), "google-generative-ai");
        assert_eq!(api_for_provider("google-vertex"), "google-vertex");
        assert_eq!(api_for_provider("openai"), "openai-completions");
        assert_eq!(api_for_provider("openai-responses"), "openai-responses");
        assert_eq!(api_for_provider("mistral"), "mistral-conversations");
        assert_eq!(
            api_for_provider("azure-openai-responses"),
            "azure-openai-responses"
        );
        assert_eq!(
            api_for_provider("amazon-bedrock"),
            "bedrock-converse-stream"
        );
        assert_eq!(api_for_provider("openrouter"), "openai-completions");
    }

    #[test]
    fn test_resolve_or_custom_model_maps_every_provider_to_its_api_and_base_url() {
        // (provider, expected api, expected base url)
        let expectations: &[(&str, &str, &str)] = &[
            (
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com/v1",
            ),
            ("openai", "openai-completions", "https://api.openai.com/v1"),
            (
                "openai-responses",
                "openai-responses",
                "https://api.openai.com/v1",
            ),
            (
                "google",
                "google-generative-ai",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                "google-vertex",
                "google-vertex",
                "https://us-central1-aiplatform.googleapis.com/v1",
            ),
            (
                "amazon-bedrock",
                "bedrock-converse-stream",
                "https://bedrock-runtime.us-east-1.amazonaws.com",
            ),
            (
                "mistral",
                "mistral-conversations",
                "https://api.mistral.ai/v1",
            ),
            ("openrouter", "openai-completions", "https://openrouter.ai/api/v1"),
            ("groq", "openai-completions", "https://api.groq.com/openai/v1"),
            (
                "deepseek",
                "openai-completions",
                "https://api.deepseek.com/v1",
            ),
            ("xai", "openai-completions", "https://api.x.ai/v1"),
            (
                "cerebras",
                "openai-completions",
                "https://api.cerebras.ai/v1",
            ),
            (
                "together",
                "openai-completions",
                "https://api.together.xyz/v1",
            ),
            (
                "fireworks",
                "openai-completions",
                "https://api.fireworks.ai/inference/v1",
            ),
            (
                "github-copilot",
                "openai-completions",
                "https://api.githubcopilot.com",
            ),
        ];

        for (provider, api, base_url) in expectations {
            let m = resolve_or_custom_model(Some("some-custom-model"), Some(provider), None).unwrap();
            assert_eq!(m.provider, *provider);
            assert_eq!(m.api, *api, "wrong api for {provider}");
            assert_eq!(m.base_url, *base_url, "wrong base url for {provider}");
            if let Some(catalog_url) = catalog::default_base_url(provider) {
                assert_eq!(m.base_url, catalog_url);
            }
        }
    }

    #[test]
    fn test_resolve_or_custom_model_normalizes_provider_case() {
        let m = resolve_or_custom_model(Some("my-model"), Some("Anthropic"), None).unwrap();
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.api, "anthropic-messages");
        assert_eq!(m.base_url, "https://api.anthropic.com/v1");
    }

    #[test]
    fn test_base_url_for_provider_aliases() {
        assert_eq!(
            base_url_for_provider("gemini"),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(
            base_url_for_provider("bedrock"),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert_eq!(
            base_url_for_provider("unknown-provider"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn test_resolve_resume_path_by_id_and_prefix() {
        let sessions = vec![
            make_session_summary("abcdef-1234", "session-alpha.jsonl"),
            make_session_summary("zzzz-9999", "session-beta.jsonl"),
        ];

        assert_eq!(
            resolve_resume_path(&sessions, "abcdef-1234").unwrap(),
            PathBuf::from("session-alpha.jsonl")
        );
        assert_eq!(
            resolve_resume_path(&sessions, "ABCD").unwrap(),
            PathBuf::from("session-alpha.jsonl")
        );
        assert_eq!(
            resolve_resume_path(&sessions, "zzzz").unwrap(),
            PathBuf::from("session-beta.jsonl")
        );

        assert!(resolve_resume_path(&sessions, "qqqq").is_err());
        assert!(resolve_resume_path(&sessions, "   ").is_err());
    }

    #[test]
    fn test_parse_session_choice() {
        assert_eq!(parse_session_choice("1", 3).unwrap(), 1);
        assert_eq!(parse_session_choice(" 3 ", 3).unwrap(), 3);
        assert!(parse_session_choice("0", 3).is_err());
        assert!(parse_session_choice("4", 3).is_err());
        assert!(parse_session_choice("not-a-number", 3).is_err());
        assert!(parse_session_choice("", 3).is_err());
    }

    #[test]
    fn test_assemble_system_prompt_keeps_custom_prompt_and_addendum() {
        let dir = make_temp_dir("mehh_cli_prompt");
        let session_mgr = SessionManager::in_memory(&dir);
        let mut session = AgentSession::new(
            dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            session_mgr,
            mehh_coding_agent::Settings::default(),
        );
        session.set_tools(Vec::new());

        let prompt = assemble_system_prompt(
            &session,
            &dir,
            Some("ALWAYS ANSWER IN FRENCH"),
            Some("EXTRA RULES"),
        );

        assert!(
            prompt.starts_with("ALWAYS ANSWER IN FRENCH"),
            "the custom prompt must lead the system prompt: {prompt}"
        );
        assert!(prompt.contains("<addendum>\nEXTRA RULES\n</addendum>"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_format_session_list_uses_labels() {
        let sessions = vec![make_session_summary("abc-1", "session-alpha.jsonl")];
        let rendered = format_session_list(&sessions);
        assert!(rendered.contains("1)"));
        assert!(rendered.contains("first prompt"));
        assert!(rendered.contains("abc-1"));
    }

    #[test]
    fn test_resolve_session_manager_no_session_is_in_memory() {
        let dir = make_temp_dir("mehh_cli_resolve");
        let args = CliArgs::try_parse_from(["mehh", "--no-session"]).unwrap();
        let sm = resolve_session_manager(&args, &dir).unwrap();
        assert!(sm.file_path().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_resolve_session_manager_continue_without_sessions_fails() {
        let dir = make_temp_dir("mehh_cli_continue");
        let args = CliArgs::try_parse_from(["mehh", "-c"]).unwrap();
        let err = match resolve_session_manager(&args, &dir) {
            Ok(_) => panic!("expected --continue to fail without a previous session"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("No previous session"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_resolve_session_manager_uses_explicit_session_path() {
        let dir = make_temp_dir("mehh_cli_explicit");
        let session_file = dir.join("explicit.jsonl");
        let args = CliArgs::try_parse_from(["mehh", "--session", "placeholder.jsonl"]).unwrap();
        let args = CliArgs {
            session: Some(session_file.clone()),
            ..args
        };
        let sm = resolve_session_manager(&args, &dir).unwrap();
        assert_eq!(sm.file_path(), Some(session_file.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_create_session_manager_no_session() {
        let temp_dir = std::env::temp_dir().join(format!(
            "mehh_cli_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);

        let sm = create_session_manager(true, None, &temp_dir).unwrap();
        assert!(sm.file_path().is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_create_session_manager_explicit_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "mehh_cli_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);
        let explicit_file = temp_dir.join("test_session.jsonl");

        let sm = create_session_manager(false, Some(&explicit_file), &temp_dir).unwrap();
        assert_eq!(sm.file_path(), Some(explicit_file.as_path()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cli_model_precedence_and_thinking_preservation() {
        let dir = make_temp_dir("mehh_cli_prec");
        let settings = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-sonnet-4-5".to_string()),
            default_thinking_level: Some(ThinkingLevel::High),
            ..Settings::default()
        };

        // Explicit --model gpt-4o without --provider ignores settings.default_provider ("anthropic")
        let (model_arg, prov_arg) =
            select_cli_model_and_provider(Some("gpt-4o"), None, &settings);
        assert_eq!(model_arg, Some("gpt-4o"));
        assert_eq!(prov_arg, None);
        let resolved = resolve_or_custom_model(
            model_arg,
            prov_arg,
            settings.default_provider.as_deref(),
        )
        .unwrap();
        assert_eq!(resolved.provider, "openai");
        assert_eq!(resolved.id, "gpt-4o");

        // Unlisted gpt model also infers openai before falling back to settings.default_provider
        let unlisted_gpt = resolve_or_custom_model(
            Some("gpt-5-future"),
            None,
            settings.default_provider.as_deref(),
        )
        .unwrap();
        assert_eq!(unlisted_gpt.provider, "openai");

        // Omitting --thinking preserves settings.default_thinking_level
        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            SessionManager::in_memory(&dir),
            settings,
            false,
        );
        apply_cli_thinking_override(&mut session, None);
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::High));

        apply_cli_thinking_override(&mut session, Some(ThinkingLevel::Low));
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::Low));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cli_rejects_conflicting_approve_and_no_approve() {
        let args1 = CliArgs::try_parse_from(["mehh", "--approve", "--no-approve"]).unwrap();
        assert!(args1.validate().is_err());

        let args2 = CliArgs::try_parse_from(["mehh", "--trust", "--no-approve"]).unwrap();
        assert!(args2.validate().is_err());

        let args3 = CliArgs::try_parse_from(["mehh", "--approve"]).unwrap();
        assert!(args3.validate().is_ok());
        assert_eq!(args3.trust_override(), mehh_coding_agent::TrustOverride::Approve);

        let args4 = CliArgs::try_parse_from(["mehh", "--no-approve"]).unwrap();
        assert!(args4.validate().is_ok());
        assert_eq!(args4.trust_override(), mehh_coding_agent::TrustOverride::NoApprove);
    }

    #[test]
    fn cli_parses_export_input_and_optional_output() {
        let args = CliArgs::try_parse_from(["mehh", "--export", "in_session.jsonl", "out.jsonl"]).unwrap();
        assert_eq!(args.export, Some(PathBuf::from("in_session.jsonl")));
        assert_eq!(args.prompt, vec!["out.jsonl"]);

        let args_no_out = CliArgs::try_parse_from(["mehh", "--export", "in_session.jsonl"]).unwrap();
        assert_eq!(args_no_out.export, Some(PathBuf::from("in_session.jsonl")));
        assert!(args_no_out.prompt.is_empty());
    }

    #[test]
    fn tool_selection_flags_respect_tools_exclude_tools_and_no_builtin_tools() {
        let temp_dir = make_temp_dir("mehh_tools_test");

        // 1. Default tools (9 tools with analyze and undo_edit, powershell is opt-in)
        let args_def = CliArgs::try_parse_from(["mehh"]).unwrap();
        let tools_def = resolve_cli_tools(&args_def, temp_dir.clone()).unwrap();
        assert_eq!(tools_def.len(), 9);
        assert!(!tools_def.iter().any(|t| t.name() == "powershell"));

        // 2. Opt-in powershell via --tools
        let args_ps = CliArgs::try_parse_from(["mehh", "--tools", "powershell"]).unwrap();
        let tools_ps = resolve_cli_tools(&args_ps, temp_dir.clone()).unwrap();
        assert_eq!(tools_ps.len(), 1);
        assert_eq!(tools_ps[0].name(), "powershell");

        // 3. Exclude tools
        let args_ex = CliArgs::try_parse_from(["mehh", "--exclude-tools", "bash,edit"]).unwrap();
        let tools_ex = resolve_cli_tools(&args_ex, temp_dir.clone()).unwrap();
        assert_eq!(tools_ex.len(), 7);
        assert!(!tools_ex.iter().any(|t| t.name() == "bash" || t.name() == "edit"));

        // 4. --no-builtin-tools
        let args_nobuiltin = CliArgs::try_parse_from(["mehh", "--no-builtin-tools"]).unwrap();
        let tools_nobuiltin = resolve_cli_tools(&args_nobuiltin, temp_dir.clone()).unwrap();
        assert!(tools_nobuiltin.is_empty());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn process_file_arguments_expands_at_text_with_bom_strip_and_at_images() {
        let temp_dir = make_temp_dir("mehh_file_args");

        let notes_path = temp_dir.join("notes.txt");
        let bom_notes = "\u{feff}notes content".to_string();
        std::fs::write(&notes_path, bom_notes.as_bytes()).unwrap();

        let img_path = temp_dir.join("sample.png");
        let fake_png_bytes = b"\x89PNG\r\n\x1a\nfakeimagebytes";
        std::fs::write(&img_path, fake_png_bytes).unwrap();

        let args = vec![
            "summarize".to_string(),
            "@notes.txt".to_string(),
            "with".to_string(),
            "@sample.png".to_string(),
        ];

        let processed = process_file_arguments(&temp_dir, &args).unwrap();
        assert!(processed.text.contains("summarize"));
        assert!(processed.text.contains("<file name=\"notes.txt\">"));
        assert!(processed.text.contains("notes content"));
        assert!(processed.text.contains("</file>"));
        assert!(processed.text.contains("with"));
        assert!(!processed.text.contains('\u{feff}'));

        let img_block = processed.blocks.iter().find(|b| matches!(b, mehh_ai::UserContentBlock::Image(_)));
        assert!(img_block.is_some());
        if let Some(mehh_ai::UserContentBlock::Image(img)) = img_block {
            assert_eq!(img.mime_type, "image/png");
            assert_eq!(img.data, mehh_coding_agent::base64_encode(fake_png_bytes));
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cli_rejects_conflicting_session_and_feature_flags() {
        let args_no_sess_cont = CliArgs::try_parse_from(["mehh", "--no-session", "-c"]).unwrap();
        assert!(args_no_sess_cont.validate().is_err());

        let args_no_sess_resume = CliArgs::try_parse_from(["mehh", "--no-session", "-r"]).unwrap();
        assert!(args_no_sess_resume.validate().is_err());

        let args_no_sess_sess = CliArgs::try_parse_from(["mehh", "--no-session", "--session", "foo.jsonl"]).unwrap();
        assert!(args_no_sess_sess.validate().is_err());

        let args_cont_sess = CliArgs::try_parse_from(["mehh", "-c", "--session", "foo.jsonl"]).unwrap();
        assert!(args_cont_sess.validate().is_err());

        let args_skills = CliArgs::try_parse_from(["mehh", "--skills", "--no-skills"]).unwrap();
        assert!(args_skills.validate().is_err());

        let args_prompt_tpl = CliArgs::try_parse_from(["mehh", "--prompt-templates", "--no-prompt-templates"]).unwrap();
        assert!(args_prompt_tpl.validate().is_err());

        let args_theme = CliArgs::try_parse_from(["mehh", "--use-theme", "dark", "--no-themes"]).unwrap();
        assert!(args_theme.validate().is_err());
    }

    #[test]
    fn resolve_resume_path_rejects_directories() {
        let temp_dir = make_temp_dir("mehh_resume_dir");
        let sub_dir = temp_dir.join("not_a_session_dir");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let sessions = vec![];
        let res = resolve_resume_path(&sessions, sub_dir.to_str().unwrap());
        assert!(res.is_err(), "resolve_resume_path must not return a directory: {:?}", res);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resolve_or_custom_model_reasoning_case_insensitive() {
        let m = resolve_or_custom_model(Some("O1-preview"), Some("openai"), None).unwrap();
        assert!(m.reasoning, "O1-preview must have reasoning: true regardless of casing");

        let m_claude = resolve_or_custom_model(Some("Claude-3-7-sonnet"), Some("anthropic"), None).unwrap();
        assert!(m_claude.reasoning, "Claude-3-7-sonnet must have reasoning: true regardless of casing");
    }
}

`````

