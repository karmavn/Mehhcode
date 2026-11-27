//! Amazon Bedrock ConverseStream API client, AWS SigV4 signer, EventStream binary framing, and event processing.

use std::collections::HashMap;
use std::time::SystemTime;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::oauth::redact_error_body_with_limit;
use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry_with_secrets, RetryPolicy,
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

struct SigV4Request<'a> {
    method: &'a str,
    url: &'a str,
    region: &'a str,
    service: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
    session_token: Option<&'a str>,
    payload_hash: &'a str,
    amz_date: &'a str,
}

fn sign_aws_sigv4_with_hash(req: SigV4Request<'_>) -> Vec<(String, String)> {
    let (host, path, canonical_query) = parse_url_parts(req.url);
    let date_stamp = if req.amz_date.is_ascii() {
        req.amz_date.get(..8).unwrap_or(req.amz_date)
    } else {
        ""
    };

    let mut signed_headers_map = vec![
        ("host", host.clone()),
        ("x-amz-content-sha256", req.payload_hash.to_string()),
        ("x-amz-date", req.amz_date.to_string()),
    ];
    if let Some(token) = req.session_token {
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
        req.method.to_uppercase(),
        canonical_path,
        canonical_query,
        canonical_headers,
        signed_headers_names,
        req.payload_hash,
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());

    let credential_scope = format!("{date_stamp}/{}/{}/aws4_request", req.region, req.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        req.amz_date,
        credential_scope,
        canonical_request_hash,
    );

    let k_secret = format!("AWS4{}", req.secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, req.region.as_bytes());
    let k_service = hmac_sha256(&k_region, req.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    // SigV4 signs by hex-encoding the raw HMAC digest. Hashing it a second
    // time with SHA-256 produces an invalid signature (403 InvalidSignature).
    let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers_names}, Signature={signature}",
        req.access_key,
    );

    let mut out = vec![
        ("authorization".to_string(), auth_header),
        ("host".to_string(), host),
        ("x-amz-date".to_string(), req.amz_date.to_string()),
        ("x-amz-content-sha256".to_string(), req.payload_hash.to_string()),
    ];
    if let Some(token) = req.session_token {
        out.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    out
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
    let payload_hash = sha256_hex(payload);
    sign_aws_sigv4_with_hash(SigV4Request {
        method,
        url,
        region,
        service,
        access_key,
        secret_key,
        session_token,
        payload_hash: &payload_hash,
        amz_date,
    })
}

/// A parsed EventStream frame.
#[derive(Debug, Clone, PartialEq)]
pub struct EventStreamMessage {
    pub headers: HashMap<String, String>,
    pub payload: Vec<u8>,
}

fn read_be_bytes<const N: usize>(bytes: &[u8], label: &str) -> Result<[u8; N], anyhow::Error> {
    bytes
        .get(..N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("{label}"))
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
                let val = i16::from_be_bytes(read_be_bytes::<2>(bytes, "Header short underflow")?);
                headers.insert(name, val.to_string());
                bytes = &bytes[2..];
            }
            4 => {
                let val = i32::from_be_bytes(read_be_bytes::<4>(bytes, "Header int underflow")?);
                headers.insert(name, val.to_string());
                bytes = &bytes[4..];
            }
            5 => {
                let val = i64::from_be_bytes(read_be_bytes::<8>(bytes, "Header long underflow")?);
                headers.insert(name, val.to_string());
                bytes = &bytes[8..];
            }
            6 => {
                let len =
                    u16::from_be_bytes(read_be_bytes::<2>(bytes, "Header bytes length underflow")?)
                        as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    anyhow::bail!("Header byte array underflow");
                }
                bytes = &bytes[len..];
            }
            7 => {
                let len = u16::from_be_bytes(read_be_bytes::<2>(
                    bytes,
                    "Header string length underflow",
                )?) as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    anyhow::bail!("Header string value underflow");
                }
                let val = std::str::from_utf8(&bytes[..len])?.to_string();
                headers.insert(name, val);
                bytes = &bytes[len..];
            }
            8 => {
                let val =
                    i64::from_be_bytes(read_be_bytes::<8>(bytes, "Header timestamp underflow")?);
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
        Self {
            buffer: Vec::with_capacity(4096),
        }
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
                let total_len = u32::from_be_bytes(read_be_bytes::<4>(
                    &frame[0..4],
                    "EventStream prelude underflow",
                )?) as usize;
                let headers_len = u32::from_be_bytes(read_be_bytes::<4>(
                    &frame[4..8],
                    "EventStream prelude underflow",
                )?) as usize;
                let prelude_crc = u32::from_be_bytes(read_be_bytes::<4>(
                    &frame[8..12],
                    "EventStream prelude underflow",
                )?);

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

                let message_crc = u32::from_be_bytes(read_be_bytes::<4>(
                    &frame[total_len - 4..total_len],
                    "EventStream message CRC underflow",
                )?);
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
    crate::transform_messages::sanitize_and_cap_tool_call_id(id, 64)
}

fn push_bedrock_message(
    messages: &mut Vec<serde_json::Value>,
    role: &str,
    parts: Vec<serde_json::Value>,
) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(existing) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                existing.extend(parts);
                return;
            }
        }
    }
    messages.push(serde_json::json!({
        "role": role,
        "content": parts,
    }));
}

/// Builds the JSON request payload for Amazon Bedrock ConverseStream API.
pub fn build_bedrock_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> serde_json::Value {
    use crate::transform_messages::{split_error_tool_result_images, transform_messages};
    use crate::unicode::sanitize_surrogates_cow;

    let mut system_texts = Vec::new();
    if let Some(sys) = &context.system_prompt {
        if !sys.is_empty() {
            system_texts.push(sanitize_surrogates_cow(sys));
        }
    }
    for msg in &context.messages {
        if let Message::System(sys) = msg {
            if !sys.content.is_empty() {
                system_texts.push(sanitize_surrogates_cow(&sys.content));
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
                    UserContent::Text(text) => vec![serde_json::json!({ "text": sanitize_surrogates_cow(text) })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => serde_json::json!({ "text": sanitize_surrogates_cow(&tc.text) }),
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
                push_bedrock_message(&mut messages, "user", content_parts);
                i += 1;
            }
            Message::Assistant(a) => {
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            if !tc.text.is_empty() {
                                content_parts.push(serde_json::json!({ "text": sanitize_surrogates_cow(&tc.text) }));
                            }
                        }
                        AssistantContentBlock::Thinking(th) => {
                            if th.redacted == Some(true) {
                                if let Some(sig) = &th.thinking_signature {
                                    content_parts.push(serde_json::json!({
                                        "reasoningContent": { "redactedContent": sig }
                                    }));
                                }
                            } else {
                                let mut reasoning_text = serde_json::Map::new();
                                reasoning_text.insert("text".to_string(), serde_json::json!(sanitize_surrogates_cow(&th.thinking)));
                                if let Some(sig) = &th.thinking_signature {
                                    reasoning_text.insert("signature".to_string(), serde_json::json!(sig));
                                }
                                content_parts.push(serde_json::json!({
                                    "reasoningContent": { "reasoningText": reasoning_text }
                                }));
                            }
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
                push_bedrock_message(&mut messages, "assistant", content_parts);
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
                                    contents.push(serde_json::json!({ "text": sanitize_surrogates_cow(&tc.text) }));
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
                            error_images.push(serde_json::json!({ "text": sanitize_surrogates_cow(&s.caption) }));
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
                push_bedrock_message(&mut messages, "user", combined_content);
            }
        }
    }

    let resolved_cache_retention = resolve_cache_retention(options.cache_retention);
    let caching_enabled = resolved_cache_retention != CacheRetention::None
        && supports_prompt_caching(model);
    let cache_point = if resolved_cache_retention == CacheRetention::Long {
        serde_json::json!({ "type": "default", "ttl": "1h" })
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

    let thinking_enabled =
        is_claude && model.reasoning && options.reasoning.is_some_and(|r| r != ThinkingLevel::Off);
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);

    let mut inference_config = serde_json::Map::new();
    // Claude on Bedrock rejects temperature != 1 when extended thinking is on.
    if let Some(temp) = options.temperature {
        if !thinking_enabled {
            inference_config.insert("temperature".to_string(), serde_json::json!(temp));
        }
    }
    if options.max_tokens.is_some() || thinking_enabled {
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
            .clamp(MIN_ANSWER_TOKENS, 32768)
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
    secrets: Vec<String>,
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
        if self.done {
            return;
        }
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
                    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or(k.as_str());
                    self.mark_error(format!("{k}: {msg}"));
                    self.finish_into(events);
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
                    if let Some(redacted_data) = rc.get("redactedContent").and_then(|v| v.as_str()) {
                        if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get_mut(content_index) {
                            th.redacted = Some(true);
                            th.thinking_signature = Some(redacted_data.to_string());
                            if th.thinking.is_empty() {
                                th.thinking = "[Reasoning redacted]".to_string();
                            }
                        }
                    }
                } else if let Some(tu) = delta.get("toolUse") {
                    if let Some(input_delta) = tu.get("input").and_then(|v| v.as_str()) {
                        if let Some(BedrockOpenBlock::ToolCall { content_index, stream }) = self.open_blocks.get_mut(&block_idx) {
                            let c_idx = *content_index;
                            if let Some(args) = stream.append_if_changed(input_delta) {
                                if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(c_idx) {
                                    tc.arguments = args.clone();
                                }
                            }
                            events.push(AssistantMessageEvent::ToolCallDelta {
                                content_index: c_idx,
                                delta: input_delta.to_string(),
                                partial: self.output.clone(),
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
            if !matches!(
                self.output.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                if let Some(reason) = ms.get("stopReason").and_then(|v| v.as_str()) {
                    self.output.stop_reason = match reason {
                        "end_turn" | "stop_sequence" => StopReason::Stop,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::Length,
                        "content_filtered" => {
                            if self.output.error_message.is_none() {
                                self.output.error_message =
                                    Some("Response blocked by Bedrock content filter".to_string());
                            }
                            StopReason::Error
                        }
                        _ => StopReason::Stop,
                    };
                }
            }
        }

        // 5. metadata
        if let Some(meta) = json.get("metadata") {
            if let Some(usage) = meta.get("usage").filter(|u| u.is_object()) {
                let input = usage.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = usage.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = usage.get("totalTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_read = usage.get("cacheReadInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let cache_write = usage.get("cacheWriteInputTokens").and_then(|v| v.as_u64()).unwrap_or(0);

                // Bedrock `inputTokens` already excludes cached tokens.
                self.output.usage.input = input;
                self.output.usage.output = output;
                self.output.usage.cache_read = cache_read;
                self.output.usage.cache_write = cache_write;
                self.output.usage.total_tokens = if total > 0 {
                    total
                } else {
                    input
                        .saturating_add(output)
                        .saturating_add(cache_read)
                        .saturating_add(cache_write)
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

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_bedrock_region_from_base_url(base_url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base_url.trim()).ok()?;
    let host = parsed.host_str()?;
    let region = host
        .strip_prefix("bedrock-runtime.")?
        .strip_suffix(".amazonaws.com")?;
    if region.is_empty() || region.contains('.') {
        return None;
    }
    Some(region.to_string())
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

/// Dispatches a decoded Bedrock EventStream frame into `BedrockStreamState`.
/// Returns `true` when a fatal frame error/exception requires immediate stream termination.
pub fn process_eventstream_message_into(
    ev: &EventStreamMessage,
    state: &mut BedrockStreamState,
    batch: &mut Vec<AssistantMessageEvent>,
) -> bool {
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
        state.finish_into(batch);
        return true;
    }

    match serde_json::from_slice::<serde_json::Value>(&ev.payload) {
        Ok(json) => {
            let wrapped = match ev.headers.get(":event-type").map(|s| s.as_str()) {
                Some(event_type) if !event_type.is_empty() => match &json {
                    serde_json::Value::Object(map) if !map.contains_key(event_type) => {
                        serde_json::json!({ event_type: json })
                    }
                    _ => json,
                },
                _ => json,
            };
            state.process_event_into(&wrapped, batch);
            state.done
                && matches!(
                    state.output.stop_reason,
                    StopReason::Error | StopReason::Aborted
                )
        }
        Err(e) => {
            if msg_type == Some("event") {
                state.mark_error(format!("Invalid JSON in Bedrock event frame: {e}"));
                state.finish_into(batch);
                true
            } else {
                false
            }
        }
    }
}

fn validate_bedrock_endpoint(model_base_url: &str, region: &str) -> Result<String, String> {
    if region.is_empty()
        || !region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(format!("Invalid AWS Bedrock region: '{region}'"));
    }
    let base_url = if model_base_url.trim().is_empty() {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    } else {
        model_base_url.trim().trim_end_matches('/').to_string()
    };
    let parsed = reqwest::Url::parse(&base_url)
        .map_err(|e| format!("Invalid AWS Bedrock base URL '{base_url}': {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!(
            "Invalid AWS Bedrock base URL '{base_url}': must use http or https with a host"
        ));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn resolve_bedrock_region_and_base_url(model_base_url: &str) -> Result<(String, String), String> {
    let region = extract_bedrock_region_from_base_url(model_base_url)
        .or_else(|| non_empty_env("AWS_REGION"))
        .or_else(|| non_empty_env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".to_string());
    let base_url = validate_bedrock_endpoint(model_base_url, &region)?;
    Ok((region, base_url))
}

struct SigV4HeaderParams<'a> {
    url: &'a str,
    region: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
    session_token: Option<&'a str>,
    payload_hash: &'a str,
    amz_date: &'a str,
}

fn build_sigv4_headers_with_hash(
    p: SigV4HeaderParams<'_>,
) -> Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>, String> {
    let sig_headers = sign_aws_sigv4_with_hash(SigV4Request {
        method: "POST",
        url: p.url,
        region: p.region,
        service: "bedrock",
        access_key: p.access_key,
        secret_key: p.secret_key,
        session_token: p.session_token,
        payload_hash: p.payload_hash,
        amz_date: p.amz_date,
    });
    let mut out = Vec::with_capacity(sig_headers.len());
    for (name, val) in sig_headers {
        let h_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("Invalid SigV4 header name '{name}': {e}"))?;
        let h_val = reqwest::header::HeaderValue::from_str(&val)
            .map_err(|e| format!("Invalid SigV4 header value for '{name}': {e}"))?;
        out.push((h_name, h_val));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_sigv4_headers(
    url: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    body_bytes: &[u8],
    amz_date: &str,
) -> Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>, String> {
    let payload_hash = sha256_hex(body_bytes);
    build_sigv4_headers_with_hash(SigV4HeaderParams {
        url,
        region,
        access_key,
        secret_key,
        session_token,
        payload_hash: &payload_hash,
        amz_date,
    })
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
        let options = options.clone();
        let body_bytes = serde_json::to_vec(&build_bedrock_request(&model, context, &options))
            .unwrap_or_default();

        tokio::spawn(async move {

            let (region, base_url) = match resolve_bedrock_region_and_base_url(&model.base_url) {
                Ok(pair) => pair,
                Err(e) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(e);
                    let _ = sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };

            // Bedrock model IDs can contain ':' or '/' (e.g. "...-v1:0" or inference profile ARNs),
            // which must be percent-encoded in the path. The signer double-encodes '%'.
            let encoded_model_id = crate::oauth::percent_encode(&model.id);
            let url = format!("{base_url}/model/{encoded_model_id}/converse-stream");

            let bearer = non_empty_env("AWS_BEARER_TOKEN_BEDROCK");
            let sigv4_creds = if bearer.is_none() {
                let access_key = non_empty_env("AWS_ACCESS_KEY_ID").or_else(|| {
                    options
                        .api_key
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
                let secret_key = non_empty_env("AWS_SECRET_ACCESS_KEY");
                let session_token = non_empty_env("AWS_SESSION_TOKEN");
                match (access_key, secret_key) {
                    (Some(ak), Some(sk)) => Some((ak, sk, session_token)),
                    _ => {
                        let mut err_msg = AssistantMessage::empty_for_model(&model);
                        err_msg.stop_reason = StopReason::Error;
                        err_msg.error_message = Some(
                            "AWS Bedrock credentials missing: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (or AWS_BEARER_TOKEN_BEDROCK)"
                                .to_string(),
                        );
                        let _ = sender
                            .send(AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error: err_msg,
                            })
                            .await;
                        return;
                    }
                }
            } else {
                None
            };

            let mut secrets_owned: Vec<String> = Vec::new();
            if let Some(ref b) = bearer {
                secrets_owned.push(b.clone());
            }
            if let Some((ref ak, ref sk, ref st)) = sigv4_creds {
                secrets_owned.push(ak.clone());
                secrets_owned.push(sk.clone());
                if let Some(ref token) = st {
                    secrets_owned.push(token.clone());
                }
            }
            let secret_refs: Vec<&str> = secrets_owned.iter().map(String::as_str).collect();

            let mut base_headers = reqwest::header::HeaderMap::new();
            base_headers.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            base_headers.insert(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("application/vnd.amazon.eventstream"),
            );
            if let Err(e) = insert_custom_headers(&mut base_headers, model.headers.as_ref())
                .and_then(|_| insert_custom_headers(&mut base_headers, options.headers.as_ref()))
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
            if let Some(ref b) = bearer {
                match reqwest::header::HeaderValue::from_str(&format!("Bearer {b}")) {
                    Ok(val) => {
                        base_headers.insert(reqwest::header::AUTHORIZATION, val);
                    }
                    Err(e) => {
                        let mut err_msg = AssistantMessage::empty_for_model(&model);
                        err_msg.stop_reason = StopReason::Error;
                        err_msg.error_message = Some(redact_error_body_with_limit(
                            &format!("Invalid bearer token header: {e}"),
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

            let payload_hash = sha256_hex(&body_bytes);
            if let Some((ref access_key, ref secret_key, ref session_token)) = sigv4_creds {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let amz_date = format_iso8601_basic(now);
                if let Err(e) = build_sigv4_headers_with_hash(SigV4HeaderParams {
                    url: &url,
                    region: &region,
                    access_key,
                    secret_key,
                    session_token: session_token.as_deref(),
                    payload_hash: &payload_hash,
                    amz_date: &amz_date,
                }) {
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
            }

            let resp = match send_with_retry_with_secrets(
                || {
                    let mut headers = base_headers.clone();
                    if let Some((ref access_key, ref secret_key, ref session_token)) = sigv4_creds {
                        // Perform AWS SigV4 signing fresh on each retry attempt;
                        // insert after user headers so signed headers cannot be overwritten.
                        let now = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let amz_date = format_iso8601_basic(now);

                        if let Ok(sig_headers) = build_sigv4_headers_with_hash(SigV4HeaderParams {
                            url: &url,
                            region: &region,
                            access_key,
                            secret_key,
                            session_token: session_token.as_deref(),
                            payload_hash: &payload_hash,
                            amz_date: &amz_date,
                        }) {
                            for (h_name, h_val) in sig_headers {
                                headers.insert(h_name, h_val);
                            }
                        }
                    }
                    client
                        .post(&url)
                        .headers(headers)
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
            let mut parser = EventStreamParser::new();
            let mut state = BedrockStreamState::new(&model);
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
                                match parser.feed(&bytes) {
                                    Ok(events) => {
                                        let mut batch = Vec::new();
                                        for ev in events {
                                            if process_eventstream_message_into(&ev, &mut state, &mut batch) {
                                                sender.send_batch(batch).await;
                                                return;
                                            }
                                        }
                                        if !sender.send_batch(batch).await {
                                            return;
                                        }
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

        // 2. Long cache retention adds ttl: 1h
        let long_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let req_long = build_bedrock_request(&model, &context, &long_opts);
        let sys_long = req_long["system"].as_array().expect("system array");
        assert_eq!(
            sys_long[1]["cachePoint"],
            serde_json::json!({ "type": "default", "ttl": "1h" })
        );
        let msgs_long = req_long["messages"].as_array().expect("messages array");
        let user_content_long = msgs_long[0]["content"].as_array().expect("content array");
        assert_eq!(
            user_content_long[1]["cachePoint"],
            serde_json::json!({ "type": "default", "ttl": "1h" })
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

    #[test]
    fn bedrock_end_to_end_eventstream_dispatch_and_usage_and_content_filtered() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "https://bedrock-runtime.eu-west-1.amazonaws.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        };

        // Pipe encode_eventstream_frame -> EventStreamParser -> :event-type dispatch -> BedrockStreamState
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "contentBlockDelta")],
            br#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"cmVkYWN0ZWQ="}}}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "contentBlockStop")],
            br#"{"contentBlockIndex":0}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "contentBlockDelta")],
            br#"{"contentBlockIndex":1,"delta":{"text":"Hello"}}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "contentBlockStop")],
            br#"{"contentBlockIndex":1}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "messageStop")],
            br#"{"stopReason":"content_filtered"}"#,
        ));
        wire.extend_from_slice(&encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "metadata")],
            br#"{"usage":{"inputTokens":120,"outputTokens":30,"cacheReadInputTokens":40,"cacheWriteInputTokens":10}}"#,
        ));

        let mut parser = EventStreamParser::new();
        let frames = parser.feed(&wire).expect("parse eventstream");
        let mut state = BedrockStreamState::new(&model);
        let mut batch = Vec::new();
        for ev in &frames {
            let fatal = process_eventstream_message_into(ev, &mut state, &mut batch);
            assert!(!fatal, "content_filtered messageStop must not abort before trailing metadata frame");
        }
        let final_msg = state.finish_into(&mut batch);

        assert_eq!(final_msg.stop_reason, StopReason::Error);
        // Bedrock inputTokens is already exclusive of cacheReadInputTokens.
        assert_eq!(final_msg.usage.input, 120);
        assert_eq!(final_msg.usage.output, 30);
        assert_eq!(final_msg.usage.cache_read, 40);
        assert_eq!(final_msg.usage.cache_write, 10);
        assert_eq!(final_msg.usage.total_tokens, 200);

        match &final_msg.content[0] {
            AssistantContentBlock::Thinking(th) => {
                assert_eq!(th.redacted, Some(true));
                assert_eq!(th.thinking_signature.as_deref(), Some("cmVkYWN0ZWQ="));
            }
            other => panic!("Expected redacted thinking block, got {other:?}"),
        }
        match &final_msg.content[1] {
            AssistantContentBlock::Text(tc) => assert_eq!(tc.text, "Hello"),
            other => panic!("Expected text block, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_invalid_json_on_event_frame_is_stream_error() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            ..Default::default()
        };
        let frame = encode_eventstream_frame(
            &[(":message-type", "event"), (":event-type", "contentBlockDelta")],
            b"{not valid json",
        );
        let mut parser = EventStreamParser::new();
        let frames = parser.feed(&frame).expect("valid framing");
        let mut state = BedrockStreamState::new(&model);
        let mut batch = Vec::new();
        let fatal = process_eventstream_message_into(&frames[0], &mut state, &mut batch);
        assert!(fatal);
        assert!(matches!(
            batch.last(),
            Some(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                ..
            })
        ));
    }

    #[test]
    fn bedrock_replays_redacted_thinking_skips_empty_assistant_and_gates_thinking() {
        let mut model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Anthropic Claude 3.7 Sonnet".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            reasoning: true,
            max_tokens: 8192,
            ..Default::default()
        };

        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("First".to_string()),
                    timestamp: 1,
                }),
                // Empty assistant turn -> skipped, consecutive user turns merged.
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "".to_string(),
                        text_signature: None,
                    })],
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 2,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Second".to_string()),
                    timestamp: 3,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "[Reasoning redacted]".to_string(),
                        thinking_signature: Some("opaque_redacted_sig".to_string()),
                        redacted: Some(true),
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

        // When options.reasoning is None, Anthropic thinking must NOT be enabled.
        let req_none = build_bedrock_request(&model, &context, &StreamOptions::default());
        assert!(req_none.get("additionalModelRequestFields").is_none());

        // When thinking is enabled and options.max_tokens is None, inferenceConfig.maxTokens must be set.
        let opts_on = StreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let req_on = build_bedrock_request(&model, &context, &opts_on);
        assert_eq!(req_on["inferenceConfig"]["maxTokens"], 8192);
        assert!(req_on.get("additionalModelRequestFields").is_some());

        let msgs = req_on["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(
            msgs[1]["content"][0],
            serde_json::json!({
                "reasoningContent": { "redactedContent": "opaque_redacted_sig" }
            })
        );

        // Non-Claude reasoning model must NOT get Anthropic thinking fields.
        model.id = "deepseek.r1-v1:0".to_string();
        model.name = "DeepSeek R1".to_string();
        let req_non_claude = build_bedrock_request(&model, &context, &opts_on);
        assert!(req_non_claude.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn bedrock_multi_block_tool_call_delta_and_sigv4_non_ascii_and_region() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            ..Default::default()
        };
        let mut state = BedrockStreamState::new(&model);
        let mut events = Vec::new();
        state.process_event_into(
            &serde_json::json!({
                "contentBlockDelta": {
                    "contentBlockIndex": 0,
                    "delta": { "text": "Working" }
                }
            }),
            &mut events,
        );
        state.process_event_into(
            &serde_json::json!({
                "contentBlockStop": { "contentBlockIndex": 0 }
            }),
            &mut events,
        );
        state.process_event_into(
            &serde_json::json!({
                "contentBlockStart": {
                    "contentBlockIndex": 1,
                    "start": { "toolUse": { "toolUseId": "tu_1", "name": "ping" } }
                }
            }),
            &mut events,
        );
        state.process_event_into(
            &serde_json::json!({
                "contentBlockDelta": {
                    "contentBlockIndex": 1,
                    "delta": { "toolUse": { "input": "{\"ok\":true}" } }
                }
            }),
            &mut events,
        );
        let delta = events
            .iter()
            .find_map(|ev| match ev {
                AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    partial,
                    ..
                } => Some((*content_index, partial)),
                _ => None,
            })
            .expect("ToolCallDelta emitted");
        assert_eq!(delta.0, 1);
        assert_eq!(delta.1.content.len(), 2);

        // Non-ASCII amz_date must not panic.
        let _ = sign_aws_sigv4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse-stream",
            "us-east-1",
            "bedrock",
            "AKIA",
            "secret",
            None,
            b"{}",
            "2025\u{1F980}0101T000000Z",
        );

        assert_eq!(
            extract_bedrock_region_from_base_url("https://bedrock-runtime.ap-northeast-1.amazonaws.com/"),
            Some("ap-northeast-1".to_string())
        );
        assert_eq!(
            extract_bedrock_region_from_base_url("https://custom-proxy.example.com"),
            None
        );
    }

    #[test]
    fn in_band_json_exception_terminates_stream_and_blocks_subsequent_message_stop() {
        let model = Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            ..Default::default()
        };
        let mut state = BedrockStreamState::new(&model);
        let mut events = Vec::new();
        let exc_frame = EventStreamMessage {
            headers: [(":message-type".to_string(), "event".to_string())]
                .into_iter()
                .collect(),
            payload: br#"{"throttlingException":{"message":"Too many requests"}}"#.to_vec(),
        };
        let fatal = process_eventstream_message_into(&exc_frame, &mut state, &mut events);
        assert!(fatal, "in-band JSON exception must terminate the stream");
        assert!(state.done);
        assert_eq!(state.output.stop_reason, StopReason::Error);
        assert!(events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Error { reason: StopReason::Error, .. })));

        // A trailing messageStop frame must not overwrite StopReason::Error or emit Done.
        let stop_frame = EventStreamMessage {
            headers: [
                (":message-type".to_string(), "event".to_string()),
                (":event-type".to_string(), "messageStop".to_string()),
            ]
            .into_iter()
            .collect(),
            payload: br#"{"stopReason":"end_turn"}"#.to_vec(),
        };
        let prev_len = events.len();
        let _ = process_eventstream_message_into(&stop_frame, &mut state, &mut events);
        assert_eq!(events.len(), prev_len, "no events may be emitted after terminal Error");
        assert_eq!(state.output.stop_reason, StopReason::Error);
    }

    #[test]
    fn validate_bedrock_endpoint_and_sigv4_headers_fail_closed() {
        assert!(validate_bedrock_endpoint("", "us-east-1").is_ok());
        assert!(validate_bedrock_endpoint("", "us-east-1.attacker.com").is_err());
        assert!(validate_bedrock_endpoint("", "").is_err());
        assert!(validate_bedrock_endpoint("file:///etc/passwd", "us-east-1").is_err());
        assert!(validate_bedrock_endpoint("https://custom.example.com", "us-east-1").is_ok());
        assert_eq!(
            validate_bedrock_endpoint(
                "HTTPS://bedrock-runtime.us-east-1.amazonaws.com/",
                "us-east-1"
            )
            .expect("uppercase scheme normalized"),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );

        let err = build_sigv4_headers(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/test/converse-stream",
            "us-east-1",
            "AKIA\r\ninjected",
            "secret",
            None,
            b"{}",
            "20250101T000000Z",
        )
        .unwrap_err();
        assert!(err.contains("Invalid SigV4 header"), "{err}");
    }

    #[test]
    fn builtin_bedrock_models_honor_aws_region_env_and_fallback_to_us_east_1() {
        static BEDROCK_ENV_MUTEX: std::sync::LazyLock<std::sync::Mutex<()>> =
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

        let _lock = BEDROCK_ENV_MUTEX.lock().unwrap();
        let _g1 = EnvGuard::set("AWS_REGION", Some("eu-west-1"));
        let _g2 = EnvGuard::set("AWS_DEFAULT_REGION", Some("ap-south-1"));

        for id in [
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
        ] {
            let model = crate::catalog::resolve_model("amazon-bedrock", Some(id))
                .expect("built-in bedrock model");
            assert!(
                model.base_url.trim().is_empty(),
                "built-in bedrock model '{id}' must not hardcode base_url: {}",
                model.base_url
            );
            let (region, base_url) = resolve_bedrock_region_and_base_url(&model.base_url).unwrap();
            assert_eq!(region, "eu-west-1");
            assert_eq!(
                base_url,
                "https://bedrock-runtime.eu-west-1.amazonaws.com"
            );
        }

        // Unset both env vars -> falls back to us-east-1.
        unsafe {
            std::env::remove_var("AWS_REGION");
            std::env::remove_var("AWS_DEFAULT_REGION");
        }
        let (region, base_url) = resolve_bedrock_region_and_base_url("").unwrap();
        assert_eq!(region, "us-east-1");
        assert_eq!(
            base_url,
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );

        // Explicit custom model.base_url still wins over AWS_REGION.
        unsafe {
            std::env::set_var("AWS_REGION", "eu-west-1");
        }
        let custom_url = "https://bedrock-runtime.ap-northeast-1.amazonaws.com";
        let (custom_region, resolved_custom_url) =
            resolve_bedrock_region_and_base_url(custom_url).unwrap();
        assert_eq!(custom_region, "ap-northeast-1");
        assert_eq!(resolved_custom_url, custom_url);
    }
}
