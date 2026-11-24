//! Bounded JSON-RPC 2.0 `tools/call` and SSE response client for keyless MCP providers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::types::ProviderError;

pub const MAX_PROVIDER_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_PROVIDER_ERROR_SNIPPET: usize = 300;
pub const WEB_USER_AGENT: &str = "mehh-code/0.1";
pub const MIN_RETRY_AFTER_SECS: u64 = 5;
pub const MAX_RETRY_AFTER_SECS: u64 = 300;

/// Parsed payload extracted from an MCP `tools/call` response.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub text: String,
    pub structured_content: Option<Value>,
}

/// Reads a `reqwest::Response` body up to `max_bytes`, respecting `deadline` and `cancel`.
pub async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, ProviderError> {
    if let Some(len) = response.content_length() {
        if len > max_bytes as u64 {
            return Err(ProviderError::VendorUnavailable {
                status: None,
                detail: format!("Response exceeded {max_bytes} byte limit"),
            });
        }
    }

    let mut buf = Vec::new();
    loop {
        let next_chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ProviderError::VendorUnavailable {
                    status: None,
                    detail: "Request timed out while reading response body".to_string(),
                });
            }
            res = response.chunk() => res,
        };

        match next_chunk {
            Ok(Some(chunk)) => {
                if buf.len().saturating_add(chunk.len()) > max_bytes {
                    return Err(ProviderError::VendorUnavailable {
                        status: None,
                        detail: format!("Response exceeded {max_bytes} byte limit"),
                    });
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                return Err(ProviderError::VendorUnavailable {
                    status: None,
                    detail: format!("Failed reading response body: {err}"),
                });
            }
        }
    }

    Ok(buf)
}

/// Executes an HTTP request builder under cancellation and total deadline.
pub async fn send_cancellable_request(
    req: reqwest::RequestBuilder,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }
    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => {
            return Err(ProviderError::VendorUnavailable {
                status: None,
                detail: "Request timed out".to_string(),
            });
        }
        res = req.send() => res,
    };

    resp.map_err(|err| ProviderError::VendorUnavailable {
        status: err.status().map(|s| s.as_u16()),
        detail: format!("HTTP request failed: {err}"),
    })
}

/// Clamps `Retry-After` seconds or HTTP-date into `[5s, 300s]`.
pub fn parse_retry_after(header: Option<&str>, now: SystemTime) -> Option<Duration> {
    let raw = header?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(
            secs.clamp(MIN_RETRY_AFTER_SECS, MAX_RETRY_AFTER_SECS),
        ));
    }
    let target_unix = parse_imf_fixdate(raw)?;
    let now_unix = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let delta = target_unix.saturating_sub(now_unix);
    Some(Duration::from_secs(
        delta.clamp(MIN_RETRY_AFTER_SECS, MAX_RETRY_AFTER_SECS),
    ))
}

fn parse_imf_fixdate(raw: &str) -> Option<u64> {
    // e.g. "Wed, 21 Oct 2015 07:28:00 GMT"
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" || !parts[0].ends_with(',') {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month: u32 = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let hour: u64 = hms[0].parse().ok()?;
    let minute: u64 = hms[1].parse().ok()?;
    let second: u64 = hms[2].parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Converts Gregorian calendar date to days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn snippet_for_error(raw: &str) -> String {
    let trimmed = raw.trim();
    let end = crate::core::output::floor_char_boundary(trimmed, MAX_PROVIDER_ERROR_SNIPPET);
    trimmed[..end].to_string()
}

pub fn is_rate_limit_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "too many requests",
        "quota exceeded",
        "slow down",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || has_word_429(&lower)
}

fn has_word_429(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    lower.match_indices("429").any(|(idx, _)| {
        let before_ok =
            idx == 0 || !(bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_');
        let after = idx + 3;
        let after_ok =
            after >= bytes.len() || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
        before_ok && after_ok
    })
}

pub fn classify_http_status(status: u16, retry_after: Option<&str>, body_snippet: &str) -> ProviderError {
    let detail = if body_snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body_snippet}")
    };
    match status {
        429 => ProviderError::RateLimited {
            retry_after: parse_retry_after(retry_after, SystemTime::now()),
            detail,
        },
        400 => ProviderError::BadRequest(detail),
        _ => ProviderError::VendorUnavailable {
            status: Some(status),
            detail,
        },
    }
}

/// Splits strictly on SSE line endings (`\r\n`, `\r`, `\n`), preserving U+2028 / U+2029 inside JSON.
fn split_sse_lines(body: &str) -> impl Iterator<Item = &str> {
    body.split('\n')
        .flat_map(|line| line.split('\r'))
}

fn decode_mcp_payload(payload: &str) -> Option<Result<McpToolResult, ProviderError>> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let data: Value = serde_json::from_str(trimmed).ok()?;
    if let Some(err) = data.get("error").filter(|v| !v.is_null()) {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        if is_rate_limit_message(&msg) {
            return Some(Err(ProviderError::RateLimited {
                retry_after: None,
                detail: snippet_for_error(&msg),
            }));
        }
        return Some(Err(ProviderError::VendorUnavailable {
            status: None,
            detail: format!("MCP JSON-RPC error: {}", snippet_for_error(&msg)),
        }));
    }

    let result = data.get("result")?;
    let texts: Vec<&str> = result
        .get("content")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
        .collect();

    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        let msg = if texts.is_empty() {
            "MCP tool call failed".to_string()
        } else {
            texts.join(" ")
        };
        if is_rate_limit_message(&msg) {
            return Some(Err(ProviderError::RateLimited {
                retry_after: None,
                detail: snippet_for_error(&msg),
            }));
        }
        return Some(Err(ProviderError::VendorUnavailable {
            status: None,
            detail: snippet_for_error(&msg),
        }));
    }

    let structured = result.get("structuredContent").filter(|v| !v.is_null()).cloned();
    let text = texts
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();

    if text.is_empty() && structured.is_none() {
        return Some(Err(ProviderError::VendorUnavailable {
            status: None,
            detail: "MCP response contained no text or structured content".to_string(),
        }));
    }

    Some(Ok(McpToolResult {
        text,
        structured_content: structured,
    }))
}

/// Parses either a direct JSON-RPC response body or an SSE `data: {...}` stream.
pub fn parse_mcp_response_body(body: &str) -> Result<McpToolResult, ProviderError> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        if let Some(parsed) = decode_mcp_payload(trimmed) {
            return parsed;
        }
    }

    for line in split_sse_lines(body) {
        if let Some(rest) = line.strip_prefix("data:") {
            let candidate = rest.trim_start();
            if let Some(parsed) = decode_mcp_payload(candidate) {
                return parsed;
            }
        }
    }

    Err(ProviderError::VendorUnavailable {
        status: None,
        detail: "Unrecognized MCP response shape".to_string(),
    })
}

/// POSTs a JSON-RPC 2.0 `tools/call` payload without requiring `initialize` handshake.
pub async fn call_mcp_tool(
    client: &reqwest::Client,
    url: &str,
    tool_name: &str,
    arguments: Value,
    extra_headers: &[(&str, &str)],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<McpToolResult, ProviderError> {
    let deadline = Instant::now() + timeout;
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });

    let mut req = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .json(&payload);

    for &(k, v) in extra_headers {
        req = req.header(k, v);
    }

    let response = send_cancellable_request(req, deadline, cancel).await?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let bytes = read_bounded_body(response, MAX_PROVIDER_RESPONSE_BYTES, deadline, cancel).await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    if status >= 400 {
        return Err(classify_http_status(
            status,
            retry_after.as_deref(),
            &snippet_for_error(&text),
        ));
    }

    parse_mcp_response_body(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_parse_mcp_plain_json_and_structured_content() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{}"}],"structuredContent":{"results":[{"url":"https://docs.rs"}]}}}"#;
        let parsed = parse_mcp_response_body(raw).unwrap();
        assert_eq!(parsed.text, "{}");
        assert!(parsed.structured_content.is_some());
    }

    #[test]
    fn test_parse_mcp_sse_preserves_unicode_line_separators() {
        let sse = concat!(
            "event: message\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"line1\u{2028}line2\u{2029}end\"}]}}\r\n\r\n"
        );
        let parsed = parse_mcp_response_body(sse).unwrap();
        assert_eq!(parsed.text, "line1\u{2028}line2\u{2029}end");
    }

    #[test]
    fn test_parse_mcp_is_error_rate_limit_classification() {
        let rl = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"Rate limit exceeded (429)"}]}}"#;
        assert!(matches!(
            parse_mcp_response_body(rl),
            Err(ProviderError::RateLimited { .. })
        ));

        let non_rl = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"Internal tool failure"}]}}"#;
        assert!(matches!(
            parse_mcp_response_body(non_rl),
            Err(ProviderError::VendorUnavailable { .. })
        ));

        let crawl_429 = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"Error fetching: CRAWL_HTTP_429"}]}}"#;
        assert!(matches!(
            parse_mcp_response_body(crawl_429),
            Err(ProviderError::VendorUnavailable { .. })
        ));
    }

    #[test]
    fn test_parse_retry_after_clamps_bounds() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after(Some("2"), now),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            parse_retry_after(Some("600"), now),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_retry_after(Some("45"), now),
            Some(Duration::from_secs(45))
        );
        // 1700000000 = Tue, 14 Nov 2023 22:13:20 GMT -> +90s = 22:14:50 GMT
        assert_eq!(
            parse_retry_after(Some("Tue, 14 Nov 2023 22:14:50 GMT"), now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(parse_retry_after(Some("not-a-date"), now), None);
    }

    #[tokio::test]
    async fn test_bounded_body_exceeds_512k_fails_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = sock.read(&mut req_buf).await;
            let body = "a".repeat(MAX_PROVIDER_RESPONSE_BYTES + 1024);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let client = reqwest::Client::new();
        let res = call_mcp_tool(
            &client,
            &format!("http://{addr}/mcp"),
            "test_tool",
            json!({}),
            &[],
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            res,
            Err(ProviderError::VendorUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn test_cancellation_aborts_in_flight_mcp_call() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = accepted_rx.await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_clone.cancel();
        });

        let client = reqwest::Client::new();
        let res = call_mcp_tool(
            &client,
            &format!("http://{addr}/mcp"),
            "test_tool",
            json!({}),
            &[],
            Duration::from_secs(5),
            &cancel,
        )
        .await;
        assert_eq!(res.unwrap_err(), ProviderError::Cancelled);
    }
}
