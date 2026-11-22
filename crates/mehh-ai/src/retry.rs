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

/// Number of days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parses an RFC 7231 IMF-fixdate (e.g. `Wed, 21 Oct 2015 07:28:00 GMT`) into epoch seconds.
fn parse_http_date(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 || !parts[0].ends_with(',') || parts[5] != "GMT" {
        return None;
    }
    let day = parts[1].parse::<i64>().ok()?;
    let month = match parts[2] {
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
    let year = parts[3].parse::<i64>().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month {
        return None;
    }
    let mut clock = parts[4].split(':');
    let hour = clock.next()?.parse::<u64>().ok()?;
    let minute = clock.next()?.parse::<u64>().ok()?;
    let second = clock.next()?.parse::<u64>().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day) as u64;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Parses `retry-after-ms`, integer `retry-after`, or an HTTP-date, clamped to 60 seconds.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
    if let Some(val) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()) {
        if let Ok(ms) = val.trim().parse::<u64>() {
            return Some(Duration::from_millis(ms).min(MAX_RETRY_DELAY));
        }
    }
    if let Some(val) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        let value = val.trim();
        if let Ok(secs) = value.parse::<u64>() {
            return Some(Duration::from_secs(secs).min(MAX_RETRY_DELAY));
        }
        if let Some(epoch) = parse_http_date(value) {
            // HTTP-date is absolute: the delay is the time remaining, not the
            // epoch itself (which always exceeds the clamp).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            return Some(
                Duration::from_secs(epoch.saturating_sub(now)).min(MAX_RETRY_DELAY),
            );
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
    fn parse_retry_after_accepts_http_date_and_rejects_malformed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        // A past HTTP-date means retry immediately, not sleep for the clamp.
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(0)));

        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);

        headers.insert("retry-after", "not-a-date".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_http_date_subtracts_current_time() {
        use std::time::{SystemTime, UNIX_EPOCH};
        fn format_http_date(epoch_secs: u64) -> String {
            const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            const MON: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                "Dec",
            ];
            let days = (epoch_secs / 86_400) as i64;
            let rem = epoch_secs % 86_400;
            let z = days + 719_468;
            let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
            let mut y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            if m <= 2 {
                y += 1;
            }
            let wd = (days + 4).rem_euclid(7) as usize;
            format!(
                "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
                WD[wd],
                d,
                MON[(m - 1) as usize],
                y,
                rem / 3_600,
                (rem % 3_600) / 60,
                rem % 60
            )
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(0)));

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        headers.insert(
            "retry-after",
            format_http_date(now_secs + 5).parse().unwrap(),
        );
        let delay = parse_retry_after(&headers).expect("HTTP-date retry-after should parse");
        assert!(delay <= Duration::from_secs(5), "delay {delay:?} exceeds 5s");
    }

    #[test]
    fn parse_http_date_returns_epoch_seconds() {
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
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
