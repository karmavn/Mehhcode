//! Google Generative AI API client, request builder, and SSE event processing.

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
            client: default_http_client(),
        }
    }
}

fn non_empty_trimmed(val: Option<&str>) -> Option<String> {
    val.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
        let options = options.clone();
        let body_res = serde_json::to_vec(&build_google_request(&model, context, &options))
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
            let url = match try_resolve_google_endpoint(&model) {
                Ok(u) => u,
                Err(e) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(e.to_string());
                    let _ = sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };

            let api_key = if model.api == "google-vertex" {
                non_empty_trimmed(options.api_key.as_deref())
                    .or_else(|| non_empty_env("GOOGLE_CLOUD_API_KEY"))
            } else {
                non_empty_trimmed(options.api_key.as_deref())
            };
            let secrets_owned: Vec<String> = api_key.clone().into_iter().collect();
            let secret_refs: Vec<&str> = secrets_owned.iter().map(String::as_str).collect();

            let mut header_map = reqwest::header::HeaderMap::new();
            header_map.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            if let Some(ref key) = api_key {
                let (h_name, h_val_res) = if model.api == "google-vertex" {
                    (
                        reqwest::header::AUTHORIZATION,
                        reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")),
                    )
                } else {
                    (
                        reqwest::header::HeaderName::from_static("x-goog-api-key"),
                        reqwest::header::HeaderValue::from_str(key),
                    )
                };
                match h_val_res {
                    Ok(val) => {
                        header_map.insert(h_name, val);
                    }
                    Err(e) => {
                        let mut err_msg = AssistantMessage::empty_for_model(&model);
                        err_msg.stop_reason = StopReason::Error;
                        err_msg.error_message = Some(redact_error_body_with_limit(
                            &format!("Invalid auth header: {e}"),
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
            let mut state = GoogleStreamState::new(&model);
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
                state.mark_error("Stream ended prematurely before Google completion");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

fn ensure_alt_sse(raw_url: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(raw_url.trim())
        .map_err(|e| anyhow::anyhow!("Invalid Google endpoint URL '{raw_url}': {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        anyhow::bail!("Unsupported scheme '{}' in Google endpoint URL", url.scheme());
    }
    if !url.query_pairs().any(|(k, _)| k == "alt") {
        let new_query = match url.query() {
            Some(existing) if !existing.is_empty() => format!("{existing}&alt=sse"),
            _ => "alt=sse".to_string(),
        };
        url.set_query(Some(&new_query));
    }
    Ok(url.to_string())
}

fn validate_gcp_location(location: &str) -> anyhow::Result<()> {
    if location.is_empty()
        || !location
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        anyhow::bail!("Invalid Google Cloud location '{location}'");
    }
    Ok(())
}

fn validate_gcp_project(project: &str) -> anyhow::Result<()> {
    if project.is_empty()
        || project.contains("..")
        || !project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':'))
    {
        anyhow::bail!("Invalid Google Cloud project '{project}'");
    }
    Ok(())
}

/// Appends `suffix_path` onto `trimmed_base` while preserving query parameters, then ensures `?alt=sse`.
fn resolve_google_base_with_path(trimmed_base: &str, suffix_path: &str) -> anyhow::Result<String> {
    if let Ok(mut parsed) = reqwest::Url::parse(trimmed_base) {
        let base_path = parsed.path().trim_end_matches('/').to_string();
        parsed.set_path(&format!("{base_path}/{suffix_path}"));
        return ensure_alt_sse(parsed.as_str());
    }
    let base_url = trimmed_base.trim_end_matches('/');
    ensure_alt_sse(&format!("{base_url}/{suffix_path}?alt=sse"))
}

/// Resolves and validates the endpoint URL for Google Generative AI or Google Vertex AI.
pub fn try_resolve_google_endpoint(model: &Model) -> anyhow::Result<String> {
    let encoded_model = crate::oauth::percent_encode(&model.id);
    let trimmed_base = model.base_url.trim();

    if model.api == "google-vertex" {
        if trimmed_base.contains(":streamGenerateContent") {
            return ensure_alt_sse(trimmed_base);
        }
        if trimmed_base.is_empty() {
            let location = non_empty_env("GOOGLE_CLOUD_LOCATION")
                .unwrap_or_else(|| "us-central1".to_string());
            validate_gcp_location(&location)?;

            let project = non_empty_env("GOOGLE_CLOUD_PROJECT")
                .or_else(|| non_empty_env("GCLOUD_PROJECT"))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Google Vertex AI project is not configured: set GOOGLE_CLOUD_PROJECT or GCLOUD_PROJECT"
                    )
                })?;
            validate_gcp_project(&project)?;

            let host = if location.eq_ignore_ascii_case("global") {
                "aiplatform.googleapis.com".to_string()
            } else {
                format!("{location}-aiplatform.googleapis.com")
            };
            Ok(format!(
                "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{encoded_model}:streamGenerateContent?alt=sse"
            ))
        } else {
            resolve_google_base_with_path(
                trimmed_base,
                &format!("publishers/google/models/{encoded_model}:streamGenerateContent"),
            )
        }
    } else if trimmed_base.contains(":streamGenerateContent") {
        ensure_alt_sse(trimmed_base)
    } else {
        let base_url = if trimmed_base.is_empty() {
            "https://generativelanguage.googleapis.com/v1beta"
        } else {
            trimmed_base
        };
        resolve_google_base_with_path(
            base_url,
            &format!("models/{encoded_model}:streamGenerateContent"),
        )
    }
}

/// Resolves the endpoint URL for Google Generative AI or Google Vertex AI.
pub fn resolve_google_endpoint(model: &Model) -> String {
    try_resolve_google_endpoint(model).unwrap_or_default()
}

fn get_gemini_major_version(model_id: &str) -> Option<u32> {
    let lower = model_id.to_lowercase();
    let prefix = if let Some(stripped) = lower.strip_prefix("gemini-live-") {
        stripped
    } else {
        lower.strip_prefix("gemini-")?
    };
    let num_str: String = prefix.chars().take_while(|c| c.is_ascii_digit()).collect();
    num_str.parse().ok()
}

pub fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || get_gemini_major_version(model_id).map(|v| v >= 3).unwrap_or(false)
}

fn normalize_google_tool_call_id(id: &str, model: &Model) -> String {
    if !requires_tool_call_id(&model.id) {
        return id.to_string();
    }
    crate::transform_messages::sanitize_and_cap_tool_call_id(id, 64)
}

/// Strips JSON Schema keywords Gemini rejects from a tool parameter schema.
fn gemini_tool_parameters(value: &serde_json::Value, is_property_map: bool) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if !is_property_map
                    && matches!(
                        k.as_str(),
                        "$schema" | "additionalProperties" | "default" | "$defs" | "definitions"
                    )
                {
                    continue;
                }
                let child_is_property_map = !is_property_map
                    && (k == "properties" || k == "patternProperties");
                out.insert(k.clone(), gemini_tool_parameters(v, child_is_property_map));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter().map(|v| gemini_tool_parameters(v, false)).collect(),
        ),
        other => other.clone(),
    }
}

fn push_google_turn(
    contents: &mut Vec<serde_json::Value>,
    role: &str,
    parts: Vec<serde_json::Value>,
) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(serde_json::json!({
        "role": role,
        "parts": parts,
    }));
}

fn google_model_can_disable_thinking(model: &Model) -> bool {
    let id_lower = model.id.trim().to_ascii_lowercase();
    let bare_id = id_lower.rsplit('/').next().unwrap_or(&id_lower);
    !(bare_id.starts_with("gemini-2.5-pro") || bare_id.starts_with("gemini-3-pro"))
}

/// Builds the JSON request payload for Google Generative AI API.
pub fn build_google_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::transform_messages;
    use crate::unicode::sanitize_surrogates_cow;

    let mut system_text = String::new();
    let mut push_sys = |s: &str| {
        if !s.is_empty() {
            if !system_text.is_empty() {
                system_text.push_str("\n\n");
            }
            system_text.push_str(&sanitize_surrogates_cow(s));
        }
    };
    if let Some(sys) = &context.system_prompt {
        push_sys(sys);
    }
    for msg in &context.messages {
        if let Message::System(sys) = msg {
            push_sys(&sys.content);
        }
    }

    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, m: &Model, _a: &AssistantMessage| normalize_google_tool_call_id(id, m)),
    );

    let mut contents: Vec<serde_json::Value> = Vec::new();

    for msg in &transformed_messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let parts: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => {
                        vec![serde_json::json!({ "text": sanitize_surrogates_cow(text) })]
                    }
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => {
                                serde_json::json!({ "text": sanitize_surrogates_cow(&tc.text) })
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
                // Merge consecutive user turns (e.g. steering after tool results).
                push_google_turn(&mut contents, "user", parts);
            }
            Message::Assistant(a) => {
                let is_same_model = a.provider == model.provider && a.model == model.id;
                let mut parts: Vec<serde_json::Value> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            let thought_sig = if is_same_model {
                                tc.text_signature.as_deref()
                            } else {
                                None
                            };
                            let sanitized = sanitize_surrogates_cow(&tc.text);
                            if !sanitized.trim().is_empty() || thought_sig.is_some() {
                                let mut p = serde_json::Map::new();
                                p.insert("text".to_string(), serde_json::json!(sanitized));
                                if let Some(sig) = thought_sig {
                                    p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                                }
                                parts.push(serde_json::Value::Object(p));
                            }
                        }
                        AssistantContentBlock::Thinking(th) => {
                            if is_same_model {
                                let sanitized = sanitize_surrogates_cow(&th.thinking);
                                let mut p = serde_json::Map::new();
                                p.insert("thought".to_string(), serde_json::json!(true));
                                p.insert("text".to_string(), serde_json::json!(sanitized));
                                if let Some(sig) = &th.thinking_signature {
                                    p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                                }
                                parts.push(serde_json::Value::Object(p));
                            } else {
                                let sanitized = sanitize_surrogates_cow(&th.thinking);
                                if !sanitized.trim().is_empty() {
                                    let mut p = serde_json::Map::new();
                                    p.insert("text".to_string(), serde_json::json!(sanitized));
                                    parts.push(serde_json::Value::Object(p));
                                }
                            }
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            let mut fc = serde_json::Map::new();
                            fc.insert("name".to_string(), serde_json::json!(tc.name));
                            fc.insert(
                                "args".to_string(),
                                serde_json::Value::Object(tc.arguments.clone()),
                            );
                            if requires_tool_call_id(&model.id) && !tc.id.is_empty() {
                                fc.insert("id".to_string(), serde_json::json!(tc.id));
                            }
                            let mut p = serde_json::Map::new();
                            p.insert("functionCall".to_string(), serde_json::Value::Object(fc));
                            if is_same_model {
                                if let Some(sig) = &tc.thought_signature {
                                    p.insert("thoughtSignature".to_string(), serde_json::json!(sig));
                                }
                            }
                            parts.push(serde_json::Value::Object(p));
                        }
                    }
                }
                push_google_turn(&mut contents, "model", parts);
            }
            Message::ToolResult(tr) => {
                let mut text = String::new();
                let mut has_text = false;
                for b in &tr.content {
                    if let ToolResultContentBlock::Text(tc) = b {
                        if has_text {
                            text.push('\n');
                        }
                        text.push_str(&sanitize_surrogates_cow(&tc.text));
                        has_text = true;
                    }
                }

                let response_obj = if tr.is_error {
                    serde_json::json!({ "error": text })
                } else {
                    serde_json::json!({ "result": text })
                };

                let mut fr = serde_json::Map::new();
                fr.insert("name".to_string(), serde_json::json!(tr.tool_name));
                fr.insert("response".to_string(), response_obj);
                if requires_tool_call_id(&model.id) && !tr.tool_call_id.is_empty() {
                    fr.insert("id".to_string(), serde_json::json!(tr.tool_call_id));
                }

                let mut parts = vec![serde_json::json!({ "functionResponse": fr })];
                for block in &tr.content {
                    if let ToolResultContentBlock::Image(img) = block {
                        parts.push(serde_json::json!({
                            "inlineData": {
                                "mimeType": img.mime_type,
                                "data": img.data,
                            }
                        }));
                    }
                }

                push_google_turn(&mut contents, "user", parts);
            }
        }
    }

    let mut req_map = serde_json::Map::new();

    if !system_text.is_empty() {
        req_map.insert(
            "systemInstruction".to_string(),
            serde_json::json!({
                "parts": [{ "text": system_text }]
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
                        "parameters": gemini_tool_parameters(&t.parameters, false),
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
    if model.reasoning && options.reasoning == Some(ThinkingLevel::Off) {
        // Omitting thinkingConfig leaves Gemini 2.5 Flash thinking enabled by default,
        // whereas Pro models cannot disable thinking and reject thinkingBudget: 0.
        if google_model_can_disable_thinking(model) {
            gen_config.insert(
                "thinkingConfig".to_string(),
                serde_json::json!({ "thinkingBudget": 0 }),
            );
        }
    } else if model.reasoning {
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
    pub received_terminal: bool,
    active_thinking_index: Option<usize>,
    active_text_index: Option<usize>,
    tool_call_counter: usize,
    secrets: Vec<String>,
}

impl GoogleStreamState {
    /// Creates a new Google stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            received_terminal: false,
            active_thinking_index: None,
            active_text_index: None,
            tool_call_counter: 0,
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
                .unwrap_or("Google API error");
            self.mark_error(msg);
            self.finish_into(events);
            return;
        }

        if self.output.response_id.is_none() {
            if let Some(id) = json.get("responseId").and_then(|v| v.as_str()) {
                self.output.response_id = Some(id.to_string());
            }
        }

        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        if let Some(usage) = json.get("usageMetadata").filter(|u| u.is_object()) {
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
            self.output.usage.output = candidates.saturating_add(thoughts);
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

        if let Some(block_reason) = json
            .get("promptFeedback")
            .and_then(|pf| pf.get("blockReason"))
            .and_then(|r| r.as_str())
            .filter(|s| !s.is_empty())
        {
            self.received_terminal = true;
            let msg = json
                .get("promptFeedback")
                .and_then(|pf| pf.get("blockReasonMessage"))
                .and_then(|m| m.as_str())
                .filter(|s| !s.is_empty())
                .map(|detail| format!("Prompt blocked ({block_reason}): {detail}"))
                .unwrap_or_else(|| format!("Prompt blocked: {block_reason}"));
            self.mark_error(msg);
            self.finish_into(events);
            return;
        }

        if let Some(candidate) = json.get("candidates").and_then(|c| c.get(0)) {
            if let Some(finish_reason) = candidate.get("finishReason").and_then(|r| r.as_str()) {
                if !finish_reason.is_empty() {
                    self.received_terminal = true;
                    if !matches!(
                        self.output.stop_reason,
                        StopReason::Error | StopReason::Aborted
                    ) {
                        self.output.stop_reason = match finish_reason {
                            "STOP" => StopReason::Stop,
                            "MAX_TOKENS" => StopReason::Length,
                            "SAFETY"
                            | "RECITATION"
                            | "BLOCKLIST"
                            | "PROHIBITED_CONTENT"
                            | "SPII"
                            | "MALFORMED_FUNCTION_CALL" => {
                                self.output.error_message =
                                    Some(format!("Provider finishReason: {finish_reason}"));
                                StopReason::Error
                            }
                            _ => StopReason::Stop,
                        };
                    }
                }
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
                            events.push(AssistantMessageEvent::TextEnd {
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
                                events.push(AssistantMessageEvent::ThinkingStart {
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
                                }
                                events.push(AssistantMessageEvent::ThinkingDelta {
                                    content_index,
                                    delta: text.to_string(),
                                    partial: self.output.metadata_clone(),
                                });
                            }
                        }
                        // Gemini can send the signature in a trailing part whose
                        // text is empty; never drop it just because text is empty.
                        if let Some(sig) = thought_sig {
                            if let Some(AssistantContentBlock::Thinking(tc)) =
                                self.output.content.get_mut(content_index)
                            {
                                tc.thinking_signature = Some(sig);
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
                            events.push(AssistantMessageEvent::ThinkingEnd {
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
                                events.push(AssistantMessageEvent::TextStart {
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
                            }
                            events.push(AssistantMessageEvent::TextDelta {
                                content_index,
                                delta: text.to_string(),
                                partial: self.output.metadata_clone(),
                            });
                        }
                        if let Some(sig) = thought_sig {
                            if let Some(AssistantContentBlock::Text(tc)) =
                                self.output.content.get_mut(content_index)
                            {
                                tc.text_signature = Some(sig);
                            }
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
                            events.push(AssistantMessageEvent::ThinkingEnd {
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
                            events.push(AssistantMessageEvent::TextEnd {
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

                        events.push(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: self.output.clone(),
                        });
                        events.push(AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta: serde_json::to_string(&tool_call.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                            partial: self.output.clone(),
                        });
                        events.push(AssistantMessageEvent::ToolCallEnd {
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
        let mut batch = Vec::new();
        let out = self.finish_into(&mut batch);
        for ev in batch {
            sender.push(ev);
        }
        out
    }

    /// Closes any remaining active blocks and appends the terminal completion event to `events`.
    pub fn finish_into(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
        if let Some(thinking_idx) = self.active_thinking_index.take() {
            let content = if let Some(AssistantContentBlock::Thinking(tc)) =
                self.output.content.get(thinking_idx)
            {
                tc.thinking.clone()
            } else {
                String::new()
            };
            events.push(AssistantMessageEvent::ThinkingEnd {
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
            events.push(AssistantMessageEvent::TextEnd {
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

    static GOOGLE_ENV_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(key: &'static str, val: Option<&str>) -> Self {
            let prev = std::env::var_os(key);
            unsafe {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self(key, prev)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
    }

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
            ..Default::default()
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
                    duration_ms: None,
                    usage: None,
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
            ..Default::default()
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
        // Gemini 2.x does not accept an `id` on functionCall.
        assert!(contents[1]["parts"][1]["functionCall"]["id"].is_null());
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
        let _lock = GOOGLE_ENV_MUTEX.lock().unwrap();
        let _g1 = EnvGuard::set("GOOGLE_CLOUD_PROJECT", Some("test-project"));

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
            ..Default::default()
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
            ..Default::default()
        };

        let req = build_google_request(&model, &context, &options);
        assert_eq!(req["systemInstruction"]["parts"][0]["text"], "Vertex system prompt");
        assert_eq!(req["contents"][0]["role"], "user");
        assert_eq!(req["contents"][0]["parts"][0]["text"], "Hello Vertex");
    }

    #[test]
    fn build_request_normalizes_ids_for_gemini_3_and_drops_foreign_thinking() {
        let model = Model {
            id: "gemini-3-pro".to_string(),
            name: "Gemini 3 Pro".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        };

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Calculate something".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::Thinking(crate::types::ThinkingContent {
                            thinking: "Foreign reasoning that must be dropped".to_string(),
                            thinking_signature: Some("foreign_sig".to_string()),
                            redacted: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_abc123|item_xyz789".to_string(),
                            name: "calculator".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: Some("foreign_tool_sig".to_string()),
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
                    tool_call_id: "call_abc123|item_xyz789".to_string(),
                    tool_name: "calculator".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "42".to_string(),
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

        let req = build_google_request(&model, &context, &StreamOptions::default());
        let contents = req["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 3);

        let model_msg = &contents[1];
        assert_eq!(model_msg["role"], "model");
        let parts = model_msg["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "Foreign thinking block must be dropped");
        let fc = &parts[0]["functionCall"];
        assert_eq!(fc["name"], "calculator");
        assert_eq!(fc["id"], "call_abc123_item_xyz789");
        assert!(parts[0].get("thoughtSignature").is_none(), "foreign thought_signature must be stripped");

        let user_msg = &contents[2];
        assert_eq!(user_msg["role"], "user");
        let fr = &user_msg["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "calculator");
        assert_eq!(fr["id"], "call_abc123_item_xyz789");
    }

    #[test]
    fn test_google_error_chunk() {
        let model = Model {
            id: "gemini-2.5-flash".to_string(),
            name: "Gemini 2.5 Flash".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1_000_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let chunks = vec![
            serde_json::json!({
                "error": {
                    "code": 400,
                    "message": "User location is not supported for the API use.",
                    "status": "FAILED_PRECONDITION"
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
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("User location is not supported for the API use.")
        );
    }

    #[test]
    fn google_usage_saturates_overflowing_totals() {
        let model = test_gemini_model();
        let (sender, _stream) = create_event_stream();
        // totalTokenCount omitted so the fallback sum path is exercised too.
        let data = serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "done" }] }, "finishReason": "STOP" }],
            "usageMetadata": {
                "promptTokenCount": u64::MAX,
                "candidatesTokenCount": u64::MAX,
                "thoughtsTokenCount": u64::MAX,
                "cachedContentTokenCount": 0
            }
        })
        .to_string();
        let events = vec![SseEvent { event: None, data }];
        let result = process_google_sse_events(&model, &events, &sender);
        assert_eq!(result.usage.output, u64::MAX);
        assert_eq!(result.usage.total_tokens, u64::MAX);
    }

    #[test]
    fn google_null_error_prompt_feedback_block_and_multi_block_delta() {
        let model = test_gemini_model();

        // 1. "error": null and "usageMetadata": null must be ignored
        let mut state = GoogleStreamState::new(&model);
        let mut events = Vec::new();
        state.process_chunk_into(
            &serde_json::json!({
                "error": null,
                "usageMetadata": null,
                "candidates": [{
                    "content": { "parts": [{ "text": "Hello" }] }
                }]
            })
            .to_string(),
            &mut events,
        );
        state.process_chunk_into(
            &serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "functionCall": { "id": "fc_1", "name": "ping", "args": { "ok": true } }
                        }]
                    },
                    "finishReason": "STOP"
                }]
            })
            .to_string(),
            &mut events,
        );
        let delta = events
            .iter()
            .find_map(|ev| match ev {
                AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    partial,
                    ..
                } => Some((*content_index, partial.clone())),
                _ => None,
            })
            .expect("ToolCallDelta emitted");
        assert_eq!(delta.0, 1);
        assert_eq!(delta.1.content.len(), 2);

        let final_msg = state.finish_into(&mut events);
        assert_eq!(final_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(final_msg.error_message, None);

        // 2. promptFeedback.blockReason sets StopReason::Error and received_terminal = true
        let mut blocked_state = GoogleStreamState::new(&model);
        let mut blocked_events = Vec::new();
        blocked_state.process_chunk_into(
            &serde_json::json!({
                "promptFeedback": {
                    "blockReason": "SAFETY",
                    "blockReasonMessage": "Blocked by safety policy"
                }
            })
            .to_string(),
            &mut blocked_events,
        );
        assert!(blocked_state.received_terminal);
        assert!(matches!(
            blocked_events.last(),
            Some(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error,
            }) if error.error_message.as_deref().unwrap_or("").contains("SAFETY")
        ));
    }

    #[test]
    fn google_endpoint_encodes_model_id_and_ensures_alt_sse() {
        let model = Model {
            id: "models/gemini 2.5".to_string(),
            api: "google-generative-ai".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/models/custom:streamGenerateContent".to_string(),
            ..Default::default()
        };
        let url = try_resolve_google_endpoint(&model).expect("valid endpoint");
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/custom:streamGenerateContent?alt=sse"
        );

        let encoded_model = Model {
            id: "gemini/custom:v1".to_string(),
            api: "google-generative-ai".to_string(),
            base_url: String::new(),
            ..Default::default()
        };
        let encoded_url = try_resolve_google_endpoint(&encoded_model).expect("encoded endpoint");
        assert_eq!(
            encoded_url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini%2Fcustom%3Av1:streamGenerateContent?alt=sse"
        );

        assert!(validate_gcp_location("global").is_ok());
        assert!(validate_gcp_location("us-central1").is_ok());
        assert!(validate_gcp_location("../evil").is_err());
        assert!(validate_gcp_location("us.evil.com").is_err());
        assert!(validate_gcp_project("my-project:123").is_ok());
        assert!(validate_gcp_project("proj/../../x").is_err());
    }

    #[test]
    fn google_merges_consecutive_same_role_turns() {
        let model = test_gemini_model();
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("First".to_string()),
                    timestamp: 1,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Second".to_string()),
                    timestamp: 2,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Reply 1".to_string(),
                        text_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 3,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Reply 2".to_string(),
                        text_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 4,
                }),
            ],
            tools: None,
        };
        let req = build_google_request(&model, &context, &StreamOptions::default());
        let contents = req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn google_endpoint_preserves_query_params() {
        let model = Model {
            id: "gemini-2.5-flash".to_string(),
            api: "google-generative-ai".to_string(),
            base_url: "https://proxy.example.com/v1beta?api-version=2024".to_string(),
            ..Default::default()
        };
        assert_eq!(
            try_resolve_google_endpoint(&model).expect("proxy endpoint"),
            "https://proxy.example.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?api-version=2024&alt=sse"
        );

        let vertex_model = Model {
            id: "gemini-2.5-flash".to_string(),
            api: "google-vertex".to_string(),
            base_url: "https://proxy.example.com/vertex/?api-version=2024".to_string(),
            ..Default::default()
        };
        assert_eq!(
            try_resolve_google_endpoint(&vertex_model).expect("vertex proxy endpoint"),
            "https://proxy.example.com/vertex/publishers/google/models/gemini-2.5-flash:streamGenerateContent?api-version=2024&alt=sse"
        );
    }

    #[test]
    fn google_ignores_chunks_after_terminal_error_and_preserves_stop_reason() {
        let model = test_gemini_model();
        let mut state = GoogleStreamState::new(&model);
        let mut events = Vec::new();

        state.process_chunk_into(
            &serde_json::json!({ "error": { "message": "Quota exceeded" } }).to_string(),
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
                "candidates": [{
                    "content": { "parts": [{ "text": "trailing" }] },
                    "finishReason": "STOP"
                }]
            })
            .to_string(),
            &mut events,
        );
        state.process_chunk_into("[DONE]", &mut events);

        assert_eq!(events.len(), len_after_error);
        assert_eq!(state.output.stop_reason, StopReason::Error);
        assert_eq!(state.output.error_message.as_deref(), Some("Quota exceeded"));

        // Even before `finish_into` sets `done = true`, a subsequent `finishReason` cannot overwrite `Error`.
        let mut safety_state = GoogleStreamState::new(&model);
        let mut safety_events = Vec::new();
        safety_state.process_chunk_into(
            &serde_json::json!({
                "candidates": [{ "finishReason": "SAFETY" }]
            })
            .to_string(),
            &mut safety_events,
        );
        safety_state.process_chunk_into(
            &serde_json::json!({
                "candidates": [{ "finishReason": "STOP" }]
            })
            .to_string(),
            &mut safety_events,
        );
        let final_msg = safety_state.finish_into(&mut safety_events);
        assert_eq!(final_msg.stop_reason, StopReason::Error);
    }

    #[test]
    fn google_thinking_off_omits_budget_zero_for_always_on_pro_models() {
        let opts_off = StreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        };

        for id in ["gemini-2.5-pro", "gemini-3-pro"] {
            let mut pro = test_gemini_model();
            pro.id = id.to_string();
            pro.reasoning = true;
            let req = build_google_request(&pro, &Context::default(), &opts_off);
            assert!(
                req.get("generationConfig")
                    .and_then(|gc| gc.get("thinkingConfig"))
                    .is_none(),
                "{id} cannot disable thinking and must omit thinkingConfig on ThinkingLevel::Off: {req}"
            );
        }

        // Flash can disable thinking and still sends thinkingBudget: 0.
        let flash = test_gemini_model();
        let req_flash = build_google_request(&flash, &Context::default(), &opts_off);
        assert_eq!(
            req_flash["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn google_vertex_requires_project_env_instead_of_defaulting_to_default() {
        let _lock = GOOGLE_ENV_MUTEX.lock().unwrap();
        let _g1 = EnvGuard::set("GOOGLE_CLOUD_PROJECT", None);
        let _g2 = EnvGuard::set("GCLOUD_PROJECT", None);

        let model = Model {
            id: "gemini-2.5-flash".to_string(),
            api: "google-vertex".to_string(),
            provider: "google-vertex".to_string(),
            base_url: String::new(),
            ..Default::default()
        };
        let err = try_resolve_google_endpoint(&model)
            .expect_err("missing GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT must fail instead of using 'default'");
        assert!(
            err.to_string().contains("GOOGLE_CLOUD_PROJECT"),
            "unexpected error: {err}"
        );
    }
}
