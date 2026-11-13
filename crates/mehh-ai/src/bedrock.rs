//! Amazon Bedrock ConverseStream API client, AWS SigV4 signer, EventStream binary framing, and event processing.

use std::collections::HashMap;
use std::time::SystemTime;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::stream::{create_event_stream, AssistantEventSender, AssistantEventStream, LlmProvider};
use crate::types::*;

type HmacSha256 = Hmac<Sha256>;

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in hash {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
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

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
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
    let signature = sha256_hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

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

/// Binary frame parser for AWS `application/vnd.amazon.eventstream` protocol.
pub struct EventStreamParser {
    buffer: Vec<u8>,
}

impl EventStreamParser {
    /// Creates a new empty EventStream parser.
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feeds raw bytes into the parser and extracts all complete valid frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<EventStreamMessage>, anyhow::Error> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();

        while self.buffer.len() >= 12 {
            let total_len = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
            let headers_len = u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
            let prelude_crc = u32::from_be_bytes(self.buffer[8..12].try_into().unwrap());

            if total_len < 16 || headers_len > total_len - 16 {
                anyhow::bail!("Invalid EventStream frame: total_len={total_len}, headers_len={headers_len}");
            }

            let calculated_prelude_crc = crc32fast::hash(&self.buffer[0..8]);
            if calculated_prelude_crc != prelude_crc {
                anyhow::bail!(
                    "EventStream prelude CRC mismatch: expected {prelude_crc}, got {calculated_prelude_crc}"
                );
            }

            if self.buffer.len() < total_len {
                break;
            }

            let message_crc =
                u32::from_be_bytes(self.buffer[total_len - 4..total_len].try_into().unwrap());
            let calculated_message_crc = crc32fast::hash(&self.buffer[0..total_len - 4]);
            if calculated_message_crc != message_crc {
                anyhow::bail!(
                    "EventStream message CRC mismatch: expected {message_crc}, got {calculated_message_crc}"
                );
            }

            let headers_bytes = &self.buffer[12..12 + headers_len];
            let headers = decode_eventstream_headers(headers_bytes)?;
            let payload_bytes = self.buffer[12 + headers_len..total_len - 4].to_vec();

            messages.push(EventStreamMessage {
                headers,
                payload: payload_bytes,
            });

            self.buffer.drain(0..total_len);
        }

        Ok(messages)
    }
}

impl Default for EventStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the JSON request payload for Amazon Bedrock ConverseStream API.
pub fn build_bedrock_request(
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

    let mut messages: Vec<serde_json::Value> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::System(_) => {}
            Message::User(u) => {
                let content_parts: Vec<serde_json::Value> = match &u.content {
                    UserContent::Text(text) => vec![serde_json::json!({ "text": text })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(tc) => serde_json::json!({ "text": tc.text }),
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
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content_parts,
                }));
            }
            Message::Assistant(a) => {
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(tc) => {
                            if !tc.text.is_empty() {
                                content_parts.push(serde_json::json!({ "text": tc.text }));
                            }
                        }
                        AssistantContentBlock::Thinking(th) => {
                            let mut reasoning_obj = serde_json::Map::new();
                            reasoning_obj.insert("text".to_string(), serde_json::json!(th.thinking));
                            if let Some(sig) = &th.thinking_signature {
                                reasoning_obj.insert("signature".to_string(), serde_json::json!(sig));
                            }
                            content_parts.push(serde_json::json!({
                                "reasoningContent": reasoning_obj
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

                let tool_result = serde_json::json!({
                    "toolResult": {
                        "toolUseId": tr.tool_call_id,
                        "content": [{ "text": text }],
                        "status": if tr.is_error { "error" } else { "success" }
                    }
                });

                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [tool_result]
                }));
            }
        }
    }

    let mut req_map = serde_json::Map::new();
    req_map.insert("messages".to_string(), serde_json::Value::Array(messages));

    if !system_texts.is_empty() {
        req_map.insert(
            "system".to_string(),
            serde_json::json!([{ "text": system_texts.join("\n\n") }]),
        );
    }

    let mut inference_config = serde_json::Map::new();
    if let Some(temp) = options.temperature {
        inference_config.insert("temperature".to_string(), serde_json::json!(temp));
    }
    if let Some(max_tokens) = options.max_tokens {
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

    if model.reasoning && options.reasoning != Some(ThinkingLevel::Off) {
        let budget = match options.reasoning {
            Some(ThinkingLevel::Minimal) => 1024,
            Some(ThinkingLevel::Low) => 2048,
            Some(ThinkingLevel::Medium) => 8192,
            Some(ThinkingLevel::High) | Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => {
                32768
            }
            _ => 2048,
        };
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
    ToolCall { content_index: usize, raw_args: String },
}

/// Streaming state machine processing Bedrock EventStream frames into AssistantMessageEvents.
pub struct BedrockStreamState {
    model: Model,
    output: AssistantMessage,
    started: bool,
    done: bool,
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
            open_blocks: HashMap::new(),
        }
    }

    /// Processes a single decoded JSON payload from an EventStream frame.
    pub fn process_event(&mut self, json: &serde_json::Value, sender: &AssistantEventSender) {
        if !self.started {
            self.started = true;
            sender.push(AssistantMessageEvent::Start {
                partial: self.output.clone(),
            });
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
                            raw_args: String::new(),
                        },
                    );

                    sender.push(AssistantMessageEvent::ToolCallStart {
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
                            sender.push(AssistantMessageEvent::TextStart {
                                content_index: idx,
                                partial: self.output.clone(),
                            });
                            idx
                        }
                    };

                    if let Some(AssistantContentBlock::Text(tc)) = self.output.content.get_mut(content_index) {
                        tc.text.push_str(text);
                    }
                    sender.push(AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: text.to_string(),
                        partial: self.output.clone(),
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
                            sender.push(AssistantMessageEvent::ThinkingStart {
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
                        sender.push(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: th_text.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                    if let Some(sig) = rc.get("signature").and_then(|v| v.as_str()) {
                        if let Some(AssistantContentBlock::Thinking(th)) = self.output.content.get_mut(content_index) {
                            th.thinking_signature = Some(sig.to_string());
                        }
                    }
                } else if let Some(tu) = delta.get("toolUse") {
                    if let Some(input_delta) = tu.get("input").and_then(|v| v.as_str()) {
                        if let Some(BedrockOpenBlock::ToolCall { content_index, raw_args }) = self.open_blocks.get_mut(&block_idx) {
                            let c_idx = *content_index;
                            raw_args.push_str(input_delta);
                            sender.push(AssistantMessageEvent::ToolCallDelta {
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
                        sender.push(AssistantMessageEvent::TextEnd {
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
                        sender.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    BedrockOpenBlock::ToolCall { content_index, raw_args } => {
                        let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw_args)
                            .unwrap_or_default();
                        let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                            tc.arguments = parsed;
                            tc.clone()
                        } else {
                            return;
                        };
                        sender.push(AssistantMessageEvent::ToolCallEnd {
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
                self.output.usage.total_tokens = if total > 0 { total } else { input + output };
                self.output.usage.calculate_cost(&self.model.cost);
            }
        }
    }

    /// Closes all active blocks and emits the terminal completion event.
    pub fn finish(&mut self, sender: &AssistantEventSender) -> AssistantMessage {
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
                        sender.push(AssistantMessageEvent::TextEnd {
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
                        sender.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.output.clone(),
                        });
                    }
                    BedrockOpenBlock::ToolCall { content_index, raw_args } => {
                        let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw_args)
                            .unwrap_or_default();
                        let tool_call = if let Some(AssistantContentBlock::ToolCall(tc)) = self.output.content.get_mut(content_index) {
                            tc.arguments = parsed;
                            tc.clone()
                        } else {
                            continue;
                        };
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
            && self.output.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)))
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
            client: reqwest::Client::new(),
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

            let url = format!("{base_url}/model/{}/converse-stream", model.id);

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");
            req = req.header("accept", "application/vnd.amazon.eventstream");

            // Check if bearer token is provided
            if let Ok(bearer) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
                req = req.header("authorization", format!("Bearer {bearer}"));
            } else {
                // Perform AWS SigV4 signing
                let access_key = std::env::var("AWS_ACCESS_KEY_ID")
                    .or_else(|_| options.api_key.clone().ok_or(std::env::VarError::NotPresent))
                    .unwrap_or_default();
                let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
                let session_token = std::env::var("AWS_SESSION_TOKEN").ok();

                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // Simple UTC timestamp format YYYYMMDDTHHMMSSZ
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
            req = req.body(body_bytes);

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
            let mut parser = EventStreamParser::new();
            let mut state = BedrockStreamState::new(&model);

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
                                match parser.feed(&bytes) {
                                    Ok(events) => {
                                        for ev in events {
                                            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&ev.payload) {
                                                state.process_event(&json, &sender);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let mut err_msg = state.finish(&sender);
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

            state.finish(&sender);
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
}
