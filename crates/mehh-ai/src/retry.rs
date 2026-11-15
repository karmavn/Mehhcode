//! Exponential backoff retry policy, timeouts, and error response capping for LLM HTTP providers.

use std::time::Duration;

use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;

/// Default timeout while waiting for a provider's HTTP response headers.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Default timeout while waiting for the next streaming chunk from a provider.
pub const DEFAULT_STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum bytes read from an HTTP error response body before truncation.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Maximum characters included in a formatted error message from an HTTP error body.
const MAX_ERROR_BODY_CHARS: usize = 4_096;

/// Policy controlling retries for transient provider HTTP and transport failures.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Computes exponential backoff duration for the zero-based retry attempt.
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let mult = 1u64.checked_shl(attempt.min(10)).unwrap_or(1024);
        let delay = self.initial_backoff.saturating_mul(mult as u32);
        delay.min(self.max_backoff)
    }
}

/// Terminal failure returned by `send_with_retry`.
#[derive(Debug)]
pub enum RetryFailure {
    Cancelled,
    Transport(String),
    HttpStatus { status: StatusCode, body: String },
}

/// Builds a default HTTP client configured with connection timeout.
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Returns true if an HTTP status code represents a transient error worth retrying.
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Parses `retry-after-ms` or integer `retry-after` headers, clamped to 60 seconds.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(val) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()) {
        if let Ok(ms) = val.trim().parse::<u64>() {
            return Some(Duration::from_millis(ms).min(Duration::from_secs(60)));
        }
    }
    if let Some(val) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(secs) = val.trim().parse::<u64>() {
            return Some(Duration::from_secs(secs).min(Duration::from_secs(60)));
        }
    }
    None
}

/// Safely truncates an error body to at most `max_chars` characters on UTF-8 boundaries.
pub fn truncate_error_body(body: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    match body.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...[truncated]", &body[..idx]),
        None => body.to_string(),
    }
}

/// Reads at most `MAX_ERROR_BODY_BYTES` from a non-2xx response and truncates text cleanly.
pub async fn read_error_body_capped(resp: reqwest::Response) -> String {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    let read_fut = async {
        while let Some(chunk_res) = stream.next().await {
            let Ok(chunk) = chunk_res else {
                break;
            };
            let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
            if remaining == 0 {
                break;
            }
            let take = chunk.len().min(remaining);
            buf.extend_from_slice(&chunk[..take]);
            if buf.len() >= MAX_ERROR_BODY_BYTES {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), read_fut).await;
    let text = String::from_utf8_lossy(&buf);
    truncate_error_body(&text, MAX_ERROR_BODY_CHARS)
}

/// Executes an HTTP request built by `build_request` with retry on transient errors.
pub async fn send_with_retry<F>(
    build_request: F,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, RetryFailure>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return Err(RetryFailure::Cancelled);
        }

        let send_fut = tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, build_request().send());
        let outcome = tokio::select! {
            _ = cancel.cancelled() => return Err(RetryFailure::Cancelled),
            res = send_fut => res,
        };

        match outcome {
            Ok(Ok(resp)) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }

                if is_retryable_status(status) && attempt < policy.max_retries {
                    let delay = parse_retry_after(resp.headers())
                        .unwrap_or_else(|| policy.backoff_for_attempt(attempt));
                    drop(resp);
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(RetryFailure::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }

                let body = read_error_body_capped(resp).await;
                return Err(RetryFailure::HttpStatus { status, body });
            }
            Ok(Err(err)) => {
                if attempt < policy.max_retries
                    && (err.is_timeout() || err.is_connect() || err.is_request())
                {
                    let delay = policy.backoff_for_attempt(attempt);
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(RetryFailure::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
                return Err(RetryFailure::Transport(err.to_string()));
            }
            Err(_) => {
                if attempt < policy.max_retries {
                    let delay = policy.backoff_for_attempt(attempt);
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(RetryFailure::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
                return Err(RetryFailure::Transport(format!(
                    "Request timed out after {} seconds",
                    DEFAULT_REQUEST_TIMEOUT.as_secs()
                )));
            }
        }
    }
}

/// Converts a terminal `RetryFailure` into an `AssistantMessageEvent::Error`.
pub fn retry_failure_event(
    failure: RetryFailure,
    model: &crate::types::Model,
) -> crate::types::AssistantMessageEvent {
    let mut err_msg = crate::types::AssistantMessage::empty_for_model(model);
    let (reason, text) = match failure {
        RetryFailure::Cancelled => (
            crate::types::StopReason::Aborted,
            "Request was cancelled".to_string(),
        ),
        RetryFailure::HttpStatus { status, body } => (
            crate::types::StopReason::Error,
            format!("HTTP {status}: {body}"),
        ),
        RetryFailure::Transport(e) => (crate::types::StopReason::Error, e),
    };
    err_msg.stop_reason = reason;
    err_msg.error_message = Some(text);
    crate::types::AssistantMessageEvent::Error {
        reason,
        error: err_msg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_is_retryable_status() {
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
        assert!(is_retryable_status(StatusCode::from_u16(529).unwrap()));

        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn test_parse_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(5)));

        headers.insert("retry-after-ms", "250".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_millis(250)));

        headers.insert("retry-after-ms", "999999".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_truncate_error_body_multibyte_safe() {
        let body = "错误🎉".repeat(3000);
        let truncated = truncate_error_body(&body, 100);
        assert!(truncated.ends_with("...[truncated]"));
    }

    #[tokio::test]
    async fn test_send_with_retry_recovers_from_429() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_srv = Arc::clone(&attempts);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let n = attempts_srv.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                if n == 0 {
                    let resp = "HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 10\r\nContent-Length: 4\r\n\r\nbusy";
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let client = default_http_client();
        let url = format!("http://{addr}/test");
        let policy = RetryPolicy {
            max_retries: 2,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(50),
        };
        let cancel = CancellationToken::new();

        let resp = send_with_retry(|| client.get(&url), &policy, &cancel)
            .await
            .expect("retry should succeed on second attempt");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_send_with_retry_does_not_retry_400() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_srv = Arc::clone(&attempts);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                attempts_srv.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let resp = "HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\nbad payload";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let client = default_http_client();
        let url = format!("http://{addr}/test");
        let policy = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(50),
        };
        let cancel = CancellationToken::new();

        let err = send_with_retry(|| client.get(&url), &policy, &cancel)
            .await
            .unwrap_err();
        match err {
            RetryFailure::HttpStatus { status, body } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(body, "bad payload");
            }
            other => panic!("Unexpected error variant: {:?}", other),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
