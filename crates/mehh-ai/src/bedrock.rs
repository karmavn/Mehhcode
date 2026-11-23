//! Amazon Bedrock ConverseStream API client, AWS SigV4 signer, EventStream binary framing, and event processing.

use std::collections::HashMap;
use std::time::SystemTime;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry, RetryPolicy,
    DEFAULT_STREAM_CHUNK_TIMEOUT,
};
use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::*;
use crate::anthropic::MIN_ANSWER_TOKENS;

type HmacSha256 = Hmac<Sha256>;

fn hex_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn parse_url_parts(url: &str) -> (String, String, String) {
    let without_scheme = if let Some(stripped) = url.strip_prefix("https://") {
        stripped
    } else if let Some(stripped) = url.strip_prefix("http://") {
        stripped
    } else {
        url
    };

    let (host_port, path_query) = match without_scheme.split_once('/') {
        Some((h, pq)) => (h, format!("/{pq}")),
        None => (without_scheme, "/".to_string()),
    };

    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (path_query, "".to_string()),
    };

    (host_port.to_string(), path, query)
}

/// Pure-Rust AWS SigV4 request signer.
/// Generates required headers including Authorization, Host, x-amz-date, and x-amz-content-sha256.
#[allow(clippy::too_many_arguments)]
pub fn sign_aws_sigv4(
    method: &str,
    url: &str,
    region: &str,
    service: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    payload: &[u8],
    amz_date: &str,
) -> Vec<(String, String)> {
    let (host, path, canonical_query) = parse_url_parts(url);
    let date_stamp = if amz_date.len() >= 8 {
        &amz_date[..8]
    } else {
        amz_date
    };

    let payload_hash = sha256_hex(payload);

    let mut signed_headers_map = vec![
        ("host", host.clone()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", amz_date.to_string()),
    ];
    if let Some(token) = session_token {
        signed_headers_map.push(("x-amz-security-token", token.to_string()));
    }
    signed_headers_map.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = signed_headers_map
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();

    let signed_headers_names: String = signed_headers_map
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    // SigV4 canonical URIs are double-encoded for non-S3 services: a `%3A`
    // in the request path must appear as `%253A` in the string to sign.
    let canonical_path = path.replace('%', "%25");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_path,
        canonical_query,
        canonical_headers,
        signed_headers_names,
        payload_hash,
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());

    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        canonical_request_hash,
    );

    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    // SigV4 signs by hex-encoding the raw HMAC digest. Hashing it a second
    // time with SHA-256 produces an invalid signature (403 InvalidSignature).
    let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{credential_scope}, SignedHeaders={signed_headers_names}, Signature={signature}"
    );

    let mut out = vec![
        ("authorization".to_string(), auth_header),
        ("host".to_string(), host),
        ("x-amz-date".to_string(), amz_date.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash),
    ];
    if let Some(token) = session_token {
        out.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    out
}

/// A parsed EventStream frame.
#[derive(Debug, Clone, PartialEq)]
pub struct EventStreamMessage {
    pub headers: HashMap<String, String>,
    pub payload: Vec<u8>,
}

fn decode_eventstream_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>, anyhow::Error> {
    let mut headers = HashMap::new();
    while !bytes.is_empty() {
        if bytes.len() < 2 {
            anyhow::bail!("Invalid header prelude: too short");
        }
        let name_len = bytes[0] as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len + 1 {
            anyhow::bail!("Invalid header name length");
        }
        let name = std::str::from_utf8(&bytes[..name_len])?.to_string();
        bytes = &bytes[name_len..];

        let val_type = bytes[0];
        bytes = &bytes[1..];

        match val_type {
            0 => {
                headers.insert(name, "true".to_string());
            }
            1 => {
                headers.insert(name, "false".to_string());
            }
            2 => {
                if bytes.is_empty() {
                    anyhow::bail!("Header byte underflow");
                }
                headers.insert(name, bytes[0].to_string());
                bytes = &bytes[1..];
            }
            3 => {
                if bytes.len() < 2 {
                    anyhow::bail!("Header short underflow");
                }
                let val = i16::from_be_bytes(bytes[..2].try_into().unwrap());
                headers.insert(name, val.to_string());
                bytes = &bytes[2..];
            }
            4 => {
                if bytes.len() < 4 {
                    anyhow::bail!("Header int underflow");
                }
                let val = i32::from_be_bytes(bytes[..4].try_into().unwrap());
                headers.insert(name, val.to_string());
                bytes = &bytes[4..];
            }
            5 => {
                if bytes.len() < 8 {
                    anyhow::bail!("Header long underflow");
                }
                let val = i64::from_be_bytes(bytes[..8].try_into().unwrap());
                headers.insert(name, val.to_string());
                bytes = &bytes[8..];
            }
            6 => {
                if bytes.len() < 2 {
                    anyhow::bail!("Header bytes length underflow");
                }
                let len = u16::from_be_bytes(bytes[..2].try_into().unwrap()) as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    anyhow::bail!("Header byte array underflow");
                }
                bytes = &bytes[len..];
            }
            7 => {
                if bytes.len() < 2 {
                    anyhow::bail!("Header string length underflow");
                }
                let len = u16::from_be_bytes(bytes[..2].try_into().unwrap()) as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    anyhow::bail!("Header string value underflow");
                }
                let val = std::str::from_utf8(&bytes[..len])?.to_string();
                headers.insert(name, val);
                bytes = &bytes[len..];
            }
            8 => {
                if bytes.len() < 8 {
                    anyhow::bail!("Header timestamp underflow");
                }
                let val = i64::from_be_bytes(bytes[..8].try_into().unwrap());
                headers.insert(name, val.to_string());
                bytes = &bytes[8..];
            }
            9 => {
                if bytes.len() < 16 {
                    anyhow::bail!("Header uuid underflow");
                }
                bytes = &bytes[16..];
            }
            other => anyhow::bail!("Unsupported header value type {other}"),
        }
    }
    Ok(headers)
}

/// Helper encoding an EventStream message into a binary frame with CRC32 checksums.
pub fn encode_eventstream_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut headers_buf = Vec::new();
    for (name, val) in headers {
        headers_buf.push(name.len() as u8);
        headers_buf.extend_from_slice(name.as_bytes());
        headers_buf.push(7u8); // type 7 = string
        headers_buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
        headers_buf.extend_from_slice(val.as_bytes());
    }

    let headers_len = headers_buf.len() as u32;
    let total_len = (12 + headers_len as usize + payload.len() + 4) as u32;

    let mut buf = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = crc32fast::hash(&buf[0..8]);
    buf.extend_from_slice(&prelude_crc.to_be_bytes());

    buf.extend_from_slice(&headers_buf);
    buf.extend_from_slice(payload);

    let message_crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&message_crc.to_be_bytes());

    buf
}

/// Maximum buffered bytes allowed in EventStream parser before failing closed.
pub const MAX_EVENTSTREAM_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// Binary frame parser for AWS `application/vnd.amazon.eventstream` protocol.
pub struct EventStreamParser {
    buffer: Vec<u8>,
}

impl EventStreamParser {
    /// Creates a new empty EventStream parser.
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Returns true when trailing incomplete bytes remain in the frame buffer.
    pub fn has_pending_bytes(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Feeds raw bytes into the parser and extracts all complete valid frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<EventStreamMessage>, anyhow::Error> {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_EVENTSTREAM_BUFFER_BYTES {
            anyhow::bail!(
                "EventStream frame buffer exceeded {} bytes",
                MAX_EVENTSTREAM_BUFFER_BYTES
            );
        }
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        let mut offset = 0usize;

        let result = (|| {
            while self.buffer.len() - offset >= 12 {
                let frame = &self.buffer[offset..];
                let total_len = u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize;
                let headers_len = u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize;
                let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().unwrap());

                if !(16..=MAX_EVENTSTREAM_BUFFER_BYTES).contains(&total_len)
                    || headers_len > total_len - 16
                {
                    anyhow::bail!(
                        "Invalid EventStream frame: total_len={total_len}, headers_len={headers_len}"
                    );
                }

                let calculated_prelude_crc = crc32fast::hash(&frame[0..8]);
                if calculated_prelude_crc != prelude_crc {
                    anyhow::bail!(
                        "EventStream prelude CRC mismatch: expected {prelude_crc}, got {calculated_prelude_crc}"
                    );
                }

                if frame.len() < total_len {
                    break;
                }

                let message_crc =
                    u32::from_be_bytes(frame[total_len - 4..total_len].try_into().unwrap());
                let calculated_message_crc = crc32fast::hash(&frame[0..total_len - 4]);
                if calculated_message_crc != message_crc {
                    anyhow::bail!(
                        "EventStream message CRC mismatch: expected {message_crc}, got {calculated_message_crc}"
                    );
                }

                let headers_bytes = &frame[12..12 + headers_len];
                let headers = decode_eventstream_headers(headers_bytes)?;
                let payload_bytes = frame[12 + headers_len..total_len - 4].to_vec();

                messages.push(EventStreamMessage {
                    headers,
                    payload: payload_bytes,
                });

                offset += total_len;
            }
            Ok(messages)
        })();

        if offset > 0 {
            self.buffer.drain(0..offset);
        }

        result
    }
}

impl Default for EventStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn supports_prompt_caching(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    let candidates = [id.as_str(), name.as_str()];

    let has_claude_ref = candidates.iter().any(|s| s.contains("claude"));
    if !has_claude_ref {
        if std::env::var("AWS_BEDROCK_FORCE_CACHE").as_deref() == Ok("1") {
            return true;
        }
        return false;
    }

    if candidates.iter().any(|s| {
        s.contains("fable-5") || s.contains("opus-5") || s.contains("sonnet-5") || s.contains("haiku-5")
    }) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("-4-")) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("claude-3-7-sonnet")) {
        return true;
    }
    if candidates.iter().any(|s| s.contains("claude-3-5-haiku")) {
        return true;
    }

    false
}

fn normalize_bedrock_tool_call_id(id: &str) -> String {
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

/// Builds the JSON request payload for Amazon Bedrock ConverseStream API.
pub fn build_bedrock_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::{split_error_tool_result_images, transform_messages};
    use crate::unicode::sanitize_surrogates;

    let mut system_texts = Vec::new();
    if let Some(sys) = &context.system_prompt {
        if !sys.is_empty() {
            system_texts.push(sanitize_surrogates(sys));
        }
    }
    for msg in &context.messages {
        if let Message::System(sys) = msg {
            if !sys.content.is_empty() {
                system_texts.push(sanitize_surrogates(&sys.content));
            }
        }
    }

    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(|id: &str, _m: &Model, _a: &AssistantMessage| normalize_bedrock_tool_call_id(id)),
    );

    let mut messages: Vec<serde_json::Value> = Vec::new();
    let is_claude = is_anthropic_claude_model(model);

    let mut i = 0;
    while i < transformed_messages.len() {
        let msg = &transformed_messages[i];
        match msg {
            Message::System(_) => {
                i += 1;
            }
            Message::User(u) => {
                let content_parts: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => vec![serde_json::json!({ "text": sanitize_surrogates(text) })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => serde_json::json!({ "text": sanitize_surrogates(&tc.text) }),
                            UserContentBlock::Image(img) => {
                                let format = match img.mime_type.as_str() {
                                    "image/png" => "png",
                                    "image/jpeg" => "jpeg",
                                    "image/gif" => "gif",
                                    "image/webp" => "webp",
                                    _ => "png",
                                };
                                serde_json::json!({
                                    "image": {
                                        "format": format,
                                        "source": { "bytes": img.data }
                                    }
                                })
                            }
                        })
                        .collect(),
                };
                // A steering user can follow a ToolResult, which is also emitted
                // as role=user. Converse requires alternating roles, so merge
                // the parts into the existing user message instead.
                if let Some(last) = messages.last_mut() {
                    if last.get("role").and_then(|r| r.as_str()) == Some("user") {
                        if let Some(existing) =
                            last.get_mut("content").and_then(|c| c.as_array_mut())
                        {
                            existing.extend(content_parts);
                            i += 1;
                            continue;
                        }
                    }
                }
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content_parts,
                }));
                i += 1;
            }
            Message::Assistant(a) => {
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            if !tc.text.is_empty() {
                                content_parts.push(serde_json::json!({ "text": sanitize_surrogates(&tc.text) }));
                            }
                        }
                        AssistantContentBlock::Thinking(th) => {
                            let mut reasoning_text = serde_json::Map::new();
                            reasoning_text.insert("text".to_string(), serde_json::json!(sanitize_surrogates(&th.thinking)));
                            if let Some(sig) = &th.thinking_signature {
                                reasoning_text.insert("signature".to_string(), serde_json::json!(sig));
                            }
                            content_parts.push(serde_json::json!({
                                "reasoningContent": { "reasoningText": reasoning_text }
                            }));
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            content_parts.push(serde_json::json!({
                                "toolUse": {
                                    "toolUseId": tc.id,
                                    "name": tc.name,
                                    "input": tc.arguments,
                                }
                            }));
                        }
                    }
                }
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_parts,
                }));
                i += 1;
            }
            Message::ToolResult(_) => {
                let mut tool_results: Vec<serde_json::Value> = Vec::new();
                let mut error_images: Vec<serde_json::Value> = Vec::new();
                let mut j = i;
                while j < transformed_messages.len() {
                    if let Message::ToolResult(tr) = &transformed_messages[j] {
                        let split = if is_claude {
                            split_error_tool_result_images(tr)
                        } else {
                            None
                        };
                        let content_blocks = if let Some(ref s) = split {
                            &s.content
                        } else {
                            &tr.content
                        };
                        let mut contents = Vec::new();
                        for b in content_blocks {
                            match b {
                                ToolResultContentBlock::Text(tc) => {
                                    contents.push(serde_json::json!({ "text": sanitize_surrogates(&tc.text) }));
                                }
                                ToolResultContentBlock::Image(img) => {
                                    let format = match img.mime_type.as_str() {
                                        "image/png" => "png",
                                        "image/jpeg" => "jpeg",
                                        "image/gif" => "gif",
                                        "image/webp" => "webp",
                                        _ => "png",
                                    };
                                    contents.push(serde_json::json!({
                                        "image": {
                                            "format": format,
                                            "source": { "bytes": img.data }
                                        }
                                    }));
                                }
                            }
                        }
                        if contents.is_empty() {
                            contents.push(serde_json::json!({ "text": "<empty>" }));
                        }
                        tool_results.push(serde_json::json!({
                            "toolResult": {
                                "toolUseId": tr.tool_call_id,
                                "content": contents,
                                "status": if tr.is_error { "error" } else { "success" }
                            }
                        }));
                        if let Some(s) = split {
                            error_images.push(serde_json::json!({ "text": sanitize_surrogates(&s.caption) }));
                            for img in s.images {
                                let format = match img.mime_type.as_str() {
                                    "image/png" => "png",
                                    "image/jpeg" => "jpeg",
                                    "image/gif" => "gif",
                                    "image/webp" => "webp",
                                    _ => "png",
                                };
                                error_images.push(serde_json::json!({
                                    "image": {
                                        "format": format,
                                        "source": { "bytes": img.data }
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

                let mut combined_content = tool_results;
                combined_content.extend(error_images);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": combined_content,
                }));
            }
        }
    }

    let resolved_cache_retention = resolve_cache_retention(options.cache_retention);
    let caching_enabled = resolved_cache_retention != CacheRetention::None
        && supports_prompt_caching(model);
    let cache_point = if resolved_cache_retention == CacheRetention::Long {
        serde_json::json!({ "type": "default", "ttl": "ONE_HOUR" })
    } else {
        serde_json::json!({ "type": "default" })
    };

    if caching_enabled {
        if let Some(last_msg) = messages.last_mut() {
            if last_msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(content_arr) = last_msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                    content_arr.push(serde_json::json!({ "cachePoint": cache_point }));
                }
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));

    if !system_texts.is_empty() {
        let mut sys_blocks = vec![serde_json::json!({ "text": system_texts.join("\n\n") })];
        if caching_enabled {
            sys_blocks.push(serde_json::json!({ "cachePoint": cache_point }));
        }
        req_map.insert(
            "system".to_string(),
            serde_json::Value::Array(sys_blocks),
        );
    }

    let thinking_enabled = model.reasoning && options.reasoning != Some(ThinkingLevel::Off);
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);

    let mut inference_config = serde_json::Map::new();
    // Claude on Bedrock rejects temperature != 1 when extended thinking is on.
    if let Some(temp) = options.temperature {
        if !thinking_enabled || !is_claude {
            inference_config.insert("temperature".to_string(), serde_json::json!(temp));
        }
    }
    if options.max_tokens.is_some() {
        inference_config.insert("maxTokens".to_string(), serde_json::json!(max_tokens));
    }
    if !inference_config.is_empty() {
        req_map.insert(
            "inferenceConfig".to_string(),
            serde_json::Value::Object(inference_config),
        );
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let tool_specs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "toolSpec": {
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": {
                                "json": t.parameters
                            }
                        }
                    })
                })
                .collect();
            req_map.insert(
                "toolConfig".to_string(),
                serde_json::json!({ "tools": tool_specs }),
            );
        }
    }

    // The API requires budget_tokens >= MIN_ANSWER_TOKENS and
    // budget_tokens < maxTokens. If the ceiling cannot fit both, omit
    // the thinking block entirely instead of emitting a sub-floor
    // budget that makes every request fail with HTTP 400.
    if thinking_enabled && max_tokens > MIN_ANSWER_TOKENS {
        let budget = match options.reasoning {
            Some(ThinkingLevel::Minimal) => MIN_ANSWER_TOKENS,
            Some(ThinkingLevel::Low) => 2048,
            Some(ThinkingLevel::Medium) => 8192,
            Some(ThinkingLevel::High) | Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => {
                32768
            }
            _ => 2048,
        };
        // Bedrock caps budget_tokens at 32768 and requires it below maxTokens.
        let budget = budget
            .min(32768)
            .max(MIN_ANSWER_TOKENS)
            .min(max_tokens - 1);
        req_map.insert(
            "additionalModelRequestFields".to_string(),
            serde_json::json!({
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": budget
                }
            }),
        );
    }

    serde_json::Value::Object(req_map)
}

enum BedrockOpenBlock {
    Text { content_index: usize },
    Thinking { content_index: usize },
    ToolCall {
        content_index: usize,
        stream: Box<ToolCallArgumentsStream>,
    },
}

/// Streaming state machine processing Bedrock EventStream frames into AssistantMessageEvents.
pub struct BedrockStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
    pub received_terminal: bool,
    open_blocks: HashMap<usize, BedrockOpenBlock>,
}

impl BedrockStreamState {
    /// Creates a new Bedrock stream state.
    pub fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            output: AssistantMessage::empty_for_model(model),
            started: false,
            done: false,
            received_terminal: false,
            open_blocks: HashMap::new(),
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

    /// Processes a single decoded JSON payload from an EventStream frame and sends events.
    pub fn process_event(&mut self, json: &serde_json::Value, sender: &AssistantEventSender) {
        let mut batch = Vec::new();
        self.process_event_into(json, &mut batch);
        for ev in batch {
            sender.push(ev);
        }
    }

    /// Processes a single decoded JSON payload from an EventStream frame, appending to `events`.
    pub fn process_event_into(
        &mut self,
        json: &serde_json::Value,
        events: &mut Vec<AssistantMessageEvent>,
    ) {
        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
        }

        // 0. In-band exceptions or errors
        if let Some(obj) = json.as_object() {
            for (k, v) in obj {
                if (k.ends_with("Exception") || k.ends_with("exception") || k == "error") && v.is_object() {
                    self.received_terminal = true;
                    self.output.stop_reason = StopReason::Error;
                    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or(k.as_str());
                    self.output.error_message = Some(format!("{k}: {msg}"));
                    return;
                }
            }
        }

        // 1. contentBlockStart
        if let Some(cbs) = json.get("contentBlockStart") {
            let block_idx = cbs.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(start) = cbs.get("start") {
                if let Some(tool_use) = start.get("toolUse") {
                    let tool_id = tool_use.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = tool_use.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

                    let tool_call = ToolCall {
                        id: tool_id,
                        name,
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    };
                    self.output.content.push(AssistantContentBlock::ToolCall(tool_call));
                    let content_index = self.output.content.len() - 1;

                    self.open_blocks.insert(
                        block_idx,
                        BedrockOpenBlock::ToolCall {
                            content_index,
                            stream: Box::new(ToolCallArgumentsStream::new()),
                        },
                    );

                    events.push(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: self.output.clone(),
                    });
                }
            }
        }

        // 2. contentBlockDelta
        if let Some(cbd) = json.get("contentBlockDelta") {
            let block_idx = cbd.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = cbd.get("delta") {
                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                    let content_index = match self.open_blocks.get(&block_idx) {
                        Some(BedrockOpenBlock::Text { content_index }) => *content_index,
                        _ => {
                            self.output.content.push(AssistantContentBlock::Text(TextContent {
                                text: String::new(),
                                text_signature: None,
                            }));
                            let idx = self.output.content.len() - 1;
                            self.open_blocks.insert(block_idx, BedrockOpenBlock::Text { content_index: idx });
                            events.push(AssistantMessageEvent::TextStart {
                                content_index: idx,
                                partial: self.output.clone(),
                            });
                            idx
                        }
                    };

                    if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get_mut(content_index) {
                        tc.text.push_str(text);
                    }
                    events.push(AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: text.to_string(),
                        partial: self.output.metadata_clone(),
                    });
                } else if let Some(rc) = delta.get("reasoningContent") {
                    let content_index = match self.open_blocks.get(&block_idx) {
                        Some(BedrockOpenBlock::Thinking { content_index }) => *content_index,
                        _ => {
                            self.output.content.push(AssistantContentBlock::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: None,
                            }));
                            let idx = self.output.content.len() - 1;
                            self.open_blocks.insert(block_idx, BedrockOpenBlock::Thinking { content_index: idx });
                            events.push(AssistantMessageEvent::ThinkingStart {
                                content_index: idx,
                                partial: self.output.clone(),
                            });
                            idx
                        }
                    };

                    if let Some(th_text) = rc.get("text").and_then(|v| v.as_str()) {
                        if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get_mut(content_index) {
                            th.thinking.push_str(th_text);
                        }
                        events.push(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: th_text.to_string(),
                            partial: self.output.metadata_clone(),
                        });
                    }
                    if let Some(sig) = rc.get("signature").and_then(|v| v.as_str()) {
                        if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get_mut(content_index) {
                            th.thinking_signature = Some(sig.to_string());
                        }
                    }
                } else if let Some(tu) = delta.get("toolUse") {
                    if let Some(input_delta) = tu.get("input").and_then(|v| v.as_str()) {
                        if let Some(BedrockOpenBlock::ToolCall { content_index, stream }) = self.open_blocks.get_mut(&block_idx) {
                            let c_idx = *content_index;
                            let args = stream.append(input_delta);
                            if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(c_idx) {
                                tc.arguments = args;
                            }
                            events.push(AssistantMessageEvent::ToolCallDelta {
                                content_index: c_idx,
                                delta: input_delta.to_string(),
                                partial: self.output.metadata_clone_with_block(c_idx),
                            });
                        }
                    }
                }
            }
        }

        // 3. contentBlockStop
        if let Some(cbs) = json.get("contentBlockStop") {
            let block_idx = cbs.get("contentBlockIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(open) = self.open_blocks.remove(&block_idx) {
                match open {
                    BedrockOpenBlock::Text { content_index } => {
                        let content = if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get(content_index) {
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
                    BedrockOpenBlock::Thinking { content_index } => {
                        let content = if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get(content_index) {
                            th.thinking.clone()
                        } else {
                            String::new()
                        };
                        events.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    BedrockOpenBlock::ToolCall { content_index, stream } => {
                        let (args, err) = stream.finish();
                        let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                            tc.arguments = args;
                            tc.arguments_error = err;
                            tc.clone()
                        } else {
                            return;
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

        // 4. messageStop
        if let Some(ms) = json.get("messageStop") {
            self.received_terminal = true;
            if let Some(reason) = ms.get("stopReason").and_then(|v| v.as_str()) {
                self.output.stop_reason = match reason {
                    "end_turn" | "stop_sequence" => StopReason::Stop,
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::Length,
                    "content_filtered" => StopReason::Error,
                    _ => StopReason::Stop,
                };
            }
        }

        // 5. metadata
        if let Some(meta) = json.get("metadata") {
            if let Some(usage) = meta.get("usage") {
                let input = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = usage.get("totalTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_read = usage.get("cacheReadInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_write = usage.get("cacheWriteInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);

                self.output.usage.input = input.saturating_sub(cache_read);
                self.output.usage.output = output;
                self.output.usage.cache_read = cache_read;
                self.output.usage.cache_write = cache_write;
                self.output.usage.total_tokens = if total > 0 {
                    total
                } else {
                    input.saturating_add(output)
                };
                self.output.usage.calculate_cost(&self.model.cost);
            }
        }
    }

    /// Closes all active blocks and emits the terminal completion event.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
        let mut batch = Vec::new();
        let out = self.finish_into(&mut batch);
        for ev in batch {
            sender.push(ev);
        }
        out
    }

    /// Closes all active blocks and appends the terminal completion event to `events`.
    pub fn finish_into(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AssistantMessage {
        let mut keys: Vec<usize> = self.open_blocks.keys().copied().collect();
        keys.sort();
        for k in keys {
            if let Some(open) = self.open_blocks.remove(&k) {
                match open {
                    BedrockOpenBlock::Text { content_index } => {
                        let content = if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get(content_index) {
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
                    BedrockOpenBlock::Thinking { content_index } => {
                        let content = if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get(content_index) {
                            th.thinking.clone()
                        } else {
                            String::new()
                        };
                        events.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    BedrockOpenBlock::ToolCall { content_index, stream } => {
                        let (args, err) = stream.finish();
                        let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                            tc.arguments = args;
                            tc.arguments_error = err;
                            tc.clone()
                        } else {
                            continue;
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
            && self.output.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)))
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

/// Provider for Amazon Bedrock ConverseStream API.
pub struct BedrockProvider {
    client: reqwest::Client,
}

impl BedrockProvider {
    /// Creates a new Bedrock provider with default HTTP client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new Bedrock provider with the given HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for BedrockProvider {
    fn default() -> Self {
        Self {
            client: default_http_client(),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for BedrockProvider {
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
            let body_json = build_bedrock_request(&model, &context, &options);
            let body_bytes = serde_json::to_vec(&body_json).unwrap_or_default();

            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|_| "us-east-1".to_string());

            let base_url = if model.base_url.is_empty() {
                format!("https://bedrock-runtime.{region}.amazonaws.com")
            } else {
                model.base_url.trim_end_matches('/').to_string()
            };

            // Bedrock model IDs commonly contain ':' (e.g. "...-v1:0"), which
            // must be percent-encoded in the path. The signer double-encodes it.
            let encoded_model_id = model.id.replace(':', "%3A");
            let url = format!("{base_url}/model/{encoded_model_id}/converse-stream");

            let resp = match send_with_retry(
                || {
                    let mut req = client.post(&url);
                    req = req.header("content-type", "application/json");
                    req = req.header("accept", "application/vnd.amazon.eventstream");

                    // Check if bearer token is provided
                    if let Ok(bearer) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
                        req = req.header("authorization", format!("Bearer {bearer}"));
                    } else {
                        // Perform AWS SigV4 signing fresh on each retry attempt
                        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
                            .or_else(|_| {
                                options.api_key.clone().ok_or(std::env::VarError::NotPresent)
                            })
                            .unwrap_or_default();
                        let secret_key =
                            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
                        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();

                        let now = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let amz_date = format_iso8601_basic(now);

                        let sig_headers = sign_aws_sigv4(
                            "POST",
                            &url,
                            &region,
                            "bedrock",
                            &access_key,
                            &secret_key,
                            session_token.as_deref(),
                            &body_bytes,
                            &amz_date,
                        );

                        for (name, val) in sig_headers {
                            req = req.header(name, val);
                        }
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
                    req.body(body_bytes.clone())
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
            let mut parser = EventStreamParser::new();
            let mut state = BedrockStreamState::new(&model);

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
                                match parser.feed(&bytes) {
                                    Ok(events) => {
                                        let mut batch = Vec::new();
                                        for ev in events {
                                            let msg_type = ev.headers.get(":message-type").map(|s| s.as_str());
                                            if msg_type == Some("exception") || msg_type == Some("error") {
                                                let exc = ev
                                                    .headers
                                                    .get(":exception-type")
                                                    .or_else(|| ev.headers.get(":error-code"))
                                                    .cloned()
                                                    .unwrap_or_else(|| "Bedrock exception".to_string());
                                                let detail = if let Some(msg) = ev.headers.get(":error-message") {
                                                    msg.clone()
                                                } else {
                                                    String::from_utf8_lossy(&ev.payload).to_string()
                                                };
                                                state.mark_error(format!("{exc}: {detail}"));
                                                state.finish_into(&mut batch);
                                                sender.send_batch(batch).await;
                                                return;
                                            }
                                            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&ev.payload) {
                                                state.process_event_into(&json, &mut batch);
                                                if state.output.stop_reason == StopReason::Error
                                                    || state.output.stop_reason == StopReason::Aborted
                                                {
                                                    state.finish_into(&mut batch);
                                                    sender.send_batch(batch).await;
                                                    return;
                                                }
                                            }
                                        }
                                        sender.send_batch(batch).await;
                                    }
                                    Err(e) => {
                                        state.mark_error(e.to_string());
                                        let mut batch = Vec::new();
                                        state.finish_into(&mut batch);
                                        sender.send_batch(batch).await;
                                        return;
                                    }
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

            let mut batch = Vec::new();
            if parser.has_pending_bytes() || !state.received_terminal {
                state.mark_error("Stream ended prematurely before Bedrock messageStop");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

fn format_iso8601_basic(secs: u64) -> String {
    // Basic day/time calculation from seconds
    let days = secs / 86400;
    let rem = secs % 86400;
    let hours = rem / 3600;
    let mins = (rem % 3600) / 60;
    let s = rem % 60;

    // Approximate date from 1970-01-01
    let mut year = 1970;
    let mut d = days;
    loop {
        let leap = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 1 } else { 0 };
        let days_in_year = 365 + leap;
        if d >= days_in_year {
            d -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }
    let leap = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 1 } else { 0 };
    let month_days = [31, 28 + leap, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    for &md in &month_days {
        if d >= md {
            d -= md;
            month += 1;
        } else {
            break;
        }
    }
    let day = d + 1;

    format!("{year:04}{month:02}{day:02}T{hours:02}{mins:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_aws_sigv4() {
        let headers = sign_aws_sigv4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-7-sonnet-20250219-v1:0/converse-stream",
            "us-east-1",
            "bedrock",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            Some("session-token-xyz"),
            b"{\"messages\":[]}",
            "20250219T120000Z",
        );

        let map: HashMap<String, String> = headers.into_iter().collect();
        assert_eq!(map.get("host").unwrap(), "bedrock-runtime.us-east-1.amazonaws.com");
        assert_eq!(map.get("x-amz-date").unwrap(), "20250219T120000Z");
        assert_eq!(map.get("x-amz-security-token").unwrap(), "session-token-xyz");
        assert!(map.contains_key("x-amz-content-sha256"));

        let auth = map.get("authorization").unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20250219/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
    }

    #[test]
    fn test_eventstream_frame_roundtrip() {
        let headers = vec![
            (":event-type", "contentBlockDelta"),
            (":content-type", "application/json"),
        ];
        let payload = br#"{"contentBlockIndex":0,"delta":{"text":"Hello world"}}"#;

        let frame = encode_eventstream_frame(&headers, payload);
        let mut parser = EventStreamParser::new();
        let messages = parser.feed(&frame).expect("parse frame");

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.headers.get(":event-type").unwrap(), "contentBlockDelta");
        assert_eq!(msg.headers.get(":content-type").unwrap(), "application/json");
        assert_eq!(msg.payload, payload);
    }

    #[test]
    fn test_bedrock_stream_state_mapping() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let mut state = BedrockStreamState::new(&model);

        // 1. messageStart
        state.process_event(&serde_json::json!({
            "messageStart": { "role": "assistant" }
        }), &sender);

        // 2. contentBlockDelta (thinking)
        state.process_event(&serde_json::json!({
            "contentBlockDelta": {
                "contentBlockIndex": 0,
                "delta": {
                    "reasoningContent": {
                        "text": "Thinking steps",
                        "signature": "sig123"
                    }
                }
            }
        }), &sender);

        // 3. contentBlockStop
        state.process_event(&serde_json::json!({
            "contentBlockStop": {
                "contentBlockIndex": 0
            }
        }), &sender);

        // 4. contentBlockDelta (text)
        state.process_event(&serde_json::json!({
            "contentBlockDelta": {
                "contentBlockIndex": 1,
                "delta": {
                    "text": "Final response text"
                }
            }
        }), &sender);

        // 5. contentBlockStop
        state.process_event(&serde_json::json!({
            "contentBlockStop": {
                "contentBlockIndex": 1
            }
        }), &sender);

        // 6. messageStop & metadata
        state.process_event(&serde_json::json!({
            "messageStop": { "stopReason": "end_turn" }
        }), &sender);
        state.process_event(&serde_json::json!({
            "metadata": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 50,
                    "totalTokens": 150
                }
            }
        }), &sender);

        let final_msg = state.finish(&sender);
        assert_eq!(final_msg.stop_reason, StopReason::Stop);
        assert_eq!(final_msg.usage.input, 100);
        assert_eq!(final_msg.usage.output, 50);
        assert_eq!(final_msg.content.len(), 2);

        match &final_msg.content[0] {
            AssistantContentBlock::Thinking(th) => {
                assert_eq!(th.thinking, "Thinking steps");
                assert_eq!(th.thinking_signature.as_deref(), Some("sig123"));
            }
            _ => panic!("Expected thinking block at index 0"),
        }

        match &final_msg.content[1] {
            AssistantContentBlock::Text(tc) => {
                assert_eq!(tc.text, "Final response text");
            }
            _ => panic!("Expected text block at index 1"),
        }
    }

    #[test]
    fn build_request_splits_error_images_for_claude_and_normalizes_ids() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        };

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Run two tools".to_string()),
                    timestamp: 100,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_tool1|item1".to_string(),
                            name: "failing_tool".to_string(),
                            arguments: serde_json::Map::new(),
                            arguments_error: None,
                            thought_signature: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_tool2|item2".to_string(),
                            name: "orphaned_tool".to_string(),
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
                    tool_call_id: "call_tool1|item1".to_string(),
                    tool_name: "failing_tool".to_string(),
                    content: vec![
                        ToolResultContentBlock::Text(TextContent {
                            text: "Failure with screenshot".to_string(),
                            text_signature: None,
                        }),
                        ToolResultContentBlock::Image(ImageContent {
                            mime_type: "image/png".to_string(),
                            data: "bedrock_base64".to_string(),
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

        let uncached_opts = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let req = build_bedrock_request(&model, &context, &uncached_opts);
        let messages = req["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);

        let assistant_blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks.len(), 2);
        assert_eq!(assistant_blocks[0]["toolUse"]["toolUseId"], "call_tool1_item1");
        assert_eq!(assistant_blocks[1]["toolUse"]["toolUseId"], "call_tool2_item2");

        let turn_content = messages[2]["content"].as_array().unwrap();
        assert_eq!(turn_content.len(), 4, "Expected 2 toolResult blocks + 1 caption + 1 image");
        assert_eq!(turn_content[0]["toolResult"]["toolUseId"], "call_tool1_item1");
        assert_eq!(turn_content[0]["toolResult"]["status"], "error");
        let tool1_content = turn_content[0]["toolResult"]["content"].as_array().unwrap();
        assert_eq!(tool1_content[0]["text"], "Failure with screenshot");
        assert_eq!(tool1_content[1]["text"], "(image attached after the tool results)");

        assert_eq!(turn_content[1]["toolResult"]["toolUseId"], "call_tool2_item2");
        assert_eq!(turn_content[1]["toolResult"]["content"][0]["text"], "No result provided");

        assert!(turn_content[2]["text"].as_str().unwrap().contains("Images from the error result"));
        assert_eq!(turn_content[3]["image"]["source"]["bytes"], "bedrock_base64");
    }

    #[test]
    fn test_bedrock_prompt_cache_point_added_for_supported_models() {
        let mut model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Anthropic Claude 3.7 Sonnet".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200000,
            max_tokens: 8192,
            ..Default::default()
        };

        let context = Context {
            system_prompt: Some("System instructions".to_string()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("User query".to_string()),
                    timestamp: 1,
                }),
            ],
            tools: None,
        };

        // 0. Default StreamOptions (options.cache_retention == None) defaults to Short retention
        let default_opts = StreamOptions::default();
        let req_default = build_bedrock_request(&model, &context, &default_opts);
        let sys_default = req_default["system"].as_array().expect("system array");
        assert_eq!(sys_default.len(), 2);
        assert_eq!(
            sys_default[1]["cachePoint"],
            serde_json::json!({ "type": "default" })
        );
        let msgs_default = req_default["messages"].as_array().expect("messages array");
        let user_content_default = msgs_default[0]["content"].as_array().expect("content array");
        assert_eq!(user_content_default.len(), 2);
        assert_eq!(
            user_content_default[1]["cachePoint"],
            serde_json::json!({ "type": "default" })
        );

        // 1. When cache_retention is Short on supported model:
        let short_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Short),
            ..Default::default()
        };
        let req_short = build_bedrock_request(&model, &context, &short_opts);

        // system contains cachePoint
        let sys_short = req_short["system"].as_array().expect("system array");
        assert_eq!(sys_short.len(), 2);
        assert_eq!(sys_short[0]["text"], "System instructions");
        assert_eq!(
            sys_short[1]["cachePoint"],
            serde_json::json!({ "type": "default" })
        );

        // final user message content contains cachePoint
        let msgs_short = req_short["messages"].as_array().expect("messages array");
        let user_msg_short = &msgs_short[0];
        let user_content_short = user_msg_short["content"].as_array().expect("content array");
        assert_eq!(user_content_short.len(), 2);
        assert_eq!(user_content_short[0]["text"], "User query");
        assert_eq!(
            user_content_short[1]["cachePoint"],
            serde_json::json!({ "type": "default" })
        );

        // 2. Long cache retention adds ttl: ONE_HOUR
        let long_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let req_long = build_bedrock_request(&model, &context, &long_opts);
        let sys_long = req_long["system"].as_array().expect("system array");
        assert_eq!(
            sys_long[1]["cachePoint"],
            serde_json::json!({ "type": "default", "ttl": "ONE_HOUR" })
        );
        let msgs_long = req_long["messages"].as_array().expect("messages array");
        let user_content_long = msgs_long[0]["content"].as_array().expect("content array");
        assert_eq!(
            user_content_long[1]["cachePoint"],
            serde_json::json!({ "type": "default", "ttl": "ONE_HOUR" })
        );

        // 3. When cache_retention == Some(CacheRetention::None): cachePoint is omitted
        let none_opts = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let req_none = build_bedrock_request(&model, &context, &none_opts);
        let sys_none = req_none["system"].as_array().expect("system array");
        assert_eq!(sys_none.len(), 1);
        assert!(sys_none[0].get("cachePoint").is_none());

        let msgs_none = req_none["messages"].as_array().expect("messages array");
        let user_content_none = msgs_none[0]["content"].as_array().expect("content array");
        assert_eq!(user_content_none.len(), 1);
        assert!(user_content_none[0].get("cachePoint").is_none());

        // 4. Unsupported model (e.g. amazon nova / titan) does not get cachePoint
        model.id = "amazon.titan-text-express-v1".to_string();
        model.name = "Titan Text Express".to_string();
        let req_unsupported = build_bedrock_request(&model, &context, &short_opts);
        let sys_unsup = req_unsupported["system"].as_array().expect("system array");
        assert_eq!(sys_unsup.len(), 1);
        let msgs_unsup = req_unsupported["messages"].as_array().expect("messages array");
        let user_content_unsup = msgs_unsup[0]["content"].as_array().expect("content array");
        assert_eq!(user_content_unsup.len(), 1);
    }

    #[test]
    fn bedrock_sse_updates_partial_tool_arguments_incrementally() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Anthropic Claude 3.7 Sonnet".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200000,
            max_tokens: 8192,
            ..Default::default()
        };

        let mut state = BedrockStreamState::new(&model);
        let mut events = Vec::new();

        // 1. Tool call start
        state.process_event_into(
            &serde_json::json!({
                "contentBlockStart": {
                    "contentBlockIndex": 0,
                    "start": {
                        "toolUse": {
                            "toolUseId": "toolu_01",
                            "name": "search"
                        }
                    }
                }
            }),
            &mut events,
        );

        events.clear();

        // 2. Partial input delta
        state.process_event_into(
            &serde_json::json!({
                "contentBlockDelta": {
                    "contentBlockIndex": 0,
                    "delta": {
                        "toolUse": {
                            "input": r#"{"query":"hel"#
                        }
                    }
                }
            }),
            &mut events,
        );

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

        // 3. Second input delta
        state.process_event_into(
            &serde_json::json!({
                "contentBlockDelta": {
                    "contentBlockIndex": 0,
                    "delta": {
                        "toolUse": {
                            "input": r#"lo"}"#
                        }
                    }
                }
            }),
            &mut events,
        );

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
    fn test_bedrock_in_band_exception() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let mut state = BedrockStreamState::new(&model);
        state.process_event(
            &serde_json::json!({
                "modelStreamErrorException": {
                    "message": "The model encountered an issue during generation"
                }
            }),
            &sender,
        );
        let final_msg = state.finish(&sender);
        assert_eq!(final_msg.stop_reason, StopReason::Error);
        assert!(final_msg
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("The model encountered an issue during generation"));
    }

    #[test]
    fn bedrock_merges_user_message_following_tool_result() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        };
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("run tool".to_string()),
                    timestamp: 1,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
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
                    tool_call_id: "call_1".to_string(),
                    tool_name: "bash".to_string(),
                    content: vec![ToolResultContentBlock::Text(TextContent {
                        text: "tool output".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 3,
                    duration_ms: None,
                    usage: None,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("steering follow-up".to_string()),
                    timestamp: 4,
                }),
            ],
            tools: None,
        };
        let opts = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };

        let req = build_bedrock_request(&model, &context, &opts);
        let messages = req["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 3, "consecutive user roles must be merged: {req}");
        for pair in messages.windows(2) {
            assert_ne!(pair[0]["role"], pair[1]["role"], "roles must alternate: {req}");
        }
        let merged = messages[2]["content"].as_array().expect("merged content");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["toolResult"]["content"][0]["text"], "tool output");
        assert_eq!(merged[1]["text"], "steering follow-up");
    }

    #[test]
    fn build_request_omits_thinking_when_max_tokens_cannot_fit_budget() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: String::new(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };

        // A ceiling below the API floor cannot fit a valid budget, so the
        // thinking block must be omitted instead of failing every request.
        let opts = StreamOptions {
            max_tokens: Some(500),
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let req = build_bedrock_request(&model, &context, &opts);
        assert!(
            req.get("additionalModelRequestFields").is_none(),
            "sub-floor budget must not be emitted: {req}"
        );

        // Exactly at the floor is still too small: budget must be < max_tokens.
        let opts = StreamOptions {
            max_tokens: Some(MIN_ANSWER_TOKENS),
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let req = build_bedrock_request(&model, &context, &opts);
        assert!(
            req.get("additionalModelRequestFields").is_none(),
            "budget cannot fit below max_tokens: {req}"
        );

        // Just above the floor, the budget is clamped into [floor, max_tokens - 1].
        let opts = StreamOptions {
            max_tokens: Some(1500),
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let req = build_bedrock_request(&model, &context, &opts);
        let budget = req["additionalModelRequestFields"]["thinking"]["budget_tokens"]
            .as_u64()
            .expect("budget_tokens");
        assert!(budget >= MIN_ANSWER_TOKENS, "budget below floor: {req}");
        assert!(budget < 1500, "budget must be below max_tokens: {req}");
    }

    #[test]
    fn bedrock_usage_saturates_overflowing_totals() {
        let model = Model {
            id: "bedrock-test".to_string(),
            cost: ModelCost::default(),
            ..Default::default()
        };
        let (sender, _stream) = create_event_stream();
        let mut state = BedrockStreamState::new(&model);
        // totalTokens omitted so the fallback input + output sum is exercised.
        state.process_event(
            &serde_json::json!({
                "metadata": {
                    "usage": {
                        "inputTokens": u64::MAX,
                        "outputTokens": u64::MAX
                    }
                }
            }),
            &sender,
        );
        let result = state.finish(&sender);
        assert_eq!(result.usage.total_tokens, u64::MAX);
    }
}
