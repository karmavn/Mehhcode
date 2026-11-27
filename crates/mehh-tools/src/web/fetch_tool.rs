//! Built-in `web_fetch` AgentTool: local-first HTTP fetch + HTML-to-Markdown + automatic keyless fallback.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::http_safety::{
    check_url_host_policy, is_eligible_for_remote_extract, is_private_or_local_ip,
    normalize_and_validate_http_url, DnsPolicy,
};
#[cfg(test)]
use mehh_ai::http_safety::url_contains_potential_secret;
use reqwest::Url;
use serde_json::{json, Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::html_markdown::{
    html_to_markdown, html_to_text, looks_like_empty_spa_shell, strip_markdown_to_text,
};
use super::mcp_rpc::{read_bounded_body, send_cancellable_request};
use super::output::format_fetch_content_async;
use super::router::ProgressFn;
use super::types::{ProviderError, WebProviderChoice};
use super::{LazyWebRuntime, WebRuntime, WebRuntimeHandle};

pub const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 10;
const DEFAULT_BROWSER_UA: &str =
    "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; MehhCode/0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchFormat {
    Markdown,
    Text,
    Html,
}

impl FetchFormat {
    fn from_param(raw: Option<&str>) -> Result<Self, String> {
        match raw.unwrap_or("markdown").trim().to_ascii_lowercase().as_str() {
            "markdown" => Ok(Self::Markdown),
            "text" => Ok(Self::Text),
            "html" => Ok(Self::Html),
            other => Err(format!(
                "Unsupported format '{other}': expected 'markdown', 'text', or 'html'"
            )),
        }
    }

    fn accept_header(self) -> &'static str {
        match self {
            Self::Markdown => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            Self::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            Self::Html => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
            }
        }
    }

    fn spill_extension(self, content_type: &str) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Text => {
                if content_type.contains("json") {
                    "json"
                } else {
                    "txt"
                }
            }
        }
    }
}

enum Stage1Outcome {
    Ready {
        content_type: String,
        body: String,
    },
    EmptySpaShell {
        content_type: String,
        rendered: String,
        remote_extract_allowed: bool,
    },
    BlockedHttp {
        status: u16,
        detail: String,
        remote_extract_allowed: bool,
    },
    TerminalError(String),
}

enum Stage2Error {
    Cancelled,
    PolicyBlocked(String),
    Failed(String),
}

/// Built-in web fetch tool using local-first HTTP fetch and keyless extract fallback.
#[derive(Clone)]
pub struct WebFetchTool {
    runtime: WebRuntimeHandle,
}

impl WebFetchTool {
    pub fn new(runtime: Result<Arc<WebRuntime>, String>) -> Self {
        Self {
            runtime: WebRuntimeHandle::Eager(runtime),
        }
    }

    pub fn from_runtime(runtime: Arc<WebRuntime>) -> Self {
        Self::new(Ok(runtime))
    }

    pub fn lazy(runtime: Arc<LazyWebRuntime>) -> Self {
        Self {
            runtime: WebRuntimeHandle::Lazy(runtime),
        }
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn label(&self) -> &str {
        "Web Fetch"
    }

    fn description(&self) -> &str {
        "Fetch and read live content from an HTTP or HTTPS URL as markdown (default), plain text, or HTML. Use this whenever the user provides a URL or you need full page content from a web_search result. This tool is read-only. Very large responses are truncated inline with the full text saved to a temporary file readable via read."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTP or HTTPS URL to fetch"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text", "html"],
                    "description": "Output format (default: markdown)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 30, max: 120)"
                }
            },
            "required": ["url"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Web fetch aborted".to_string());
        }
        let runtime = self.runtime.resolve()?;
        if !runtime.config.enabled {
            return Err("Web fetch is disabled in settings".to_string());
        }

        let raw_url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required 'url' parameter".to_string())?;
        let format_raw = match params.get("format") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => {
                return Err(
                    "Invalid format parameter: expected 'markdown', 'text', or 'html'".to_string(),
                );
            }
        };
        let format = FetchFormat::from_param(format_raw)?;
        let timeout_secs = match params.get("timeout") {
            None | Some(Value::Null) => runtime.config.effective_timeout_seconds(),
            Some(v) => match v.as_u64() {
                Some(t) if (1..=120).contains(&t) => t,
                Some(t) => {
                    return Err(format!(
                        "Invalid timeout {t}: expected integer seconds between 1 and 120"
                    ));
                }
                None => {
                    return Err(
                        "Invalid timeout: expected integer seconds between 1 and 120".to_string(),
                    );
                }
            },
        };

        let url = normalize_and_validate_http_url(raw_url)?;
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        let (source, content_type, content) = match run_stage1_local(
            runtime,
            &url,
            format,
            deadline,
            &cancel,
        )
        .await
        {
            Stage1Outcome::Ready { content_type, body } => ("local".to_string(), content_type, body),
            Stage1Outcome::TerminalError(msg) => return Err(msg),
            Stage1Outcome::EmptySpaShell {
                content_type,
                rendered,
                remote_extract_allowed,
            } => {
                if remote_extract_allowed && can_use_remote_fallback(runtime, &url, format) {
                    match run_stage2_extract(
                        runtime,
                        &url,
                        format,
                        deadline,
                        &cancel,
                        on_update.as_ref(),
                    )
                    .await
                    {
                        Ok((provider_id, extracted)) => {
                            (provider_id, "text/markdown".to_string(), extracted)
                        }
                        Err(Stage2Error::Cancelled) => return Err("Web fetch aborted".to_string()),
                        Err(Stage2Error::PolicyBlocked(err)) => return Err(err),
                        Err(Stage2Error::Failed(_)) => ("local".to_string(), content_type, rendered),
                    }
                } else {
                    ("local".to_string(), content_type, rendered)
                }
            }
            Stage1Outcome::BlockedHttp {
                status,
                detail,
                remote_extract_allowed,
            } => {
                if remote_extract_allowed && can_use_remote_fallback(runtime, &url, format) {
                    match run_stage2_extract(
                        runtime,
                        &url,
                        format,
                        deadline,
                        &cancel,
                        on_update.as_ref(),
                    )
                    .await
                    {
                        Ok((provider_id, extracted)) => {
                            (provider_id, "text/markdown".to_string(), extracted)
                        }
                        Err(Stage2Error::Cancelled) => return Err("Web fetch aborted".to_string()),
                        Err(Stage2Error::PolicyBlocked(err)) => return Err(err),
                        Err(Stage2Error::Failed(fallback_err)) => {
                            return Err(format!(
                                "Unable to fetch {url}: {detail}; remote fallback failed ({fallback_err})"
                            ));
                        }
                    }
                } else {
                    return Err(format!(
                        "Unable to fetch {url}: local request returned HTTP {status} ({detail})"
                    ));
                }
            }
        };

        let ext = format.spill_extension(&content_type).to_string();
        let formatted = format_fetch_content_async(content, ext).await?;
        Ok(AgentToolResult {
            content: vec![mehh_ai::ToolResultContentBlock::Text(mehh_ai::TextContent {
                text: formatted.text,
                text_signature: None,
            })],
            details: Some(json!({
                "source": source,
                "contentType": content_type,
                "truncated": formatted.truncated,
                "spilledPath": formatted.spilled_path.map(|p| p.to_string_lossy().replace('\\', "/")),
            })),
            ..Default::default()
        })
    }
}

fn is_hop_remote_extract_eligible(runtime: &WebRuntime, url: &Url) -> bool {
    #[cfg(test)]
    if runtime.config.allow_remote_extract_for_loopback_tests {
        return !url_contains_potential_secret(url);
    }
    let _ = runtime;
    is_eligible_for_remote_extract(url).is_ok()
}

fn can_use_remote_fallback(runtime: &WebRuntime, url: &Url, format: FetchFormat) -> bool {
    if !runtime.config.fetch_fallback
        || runtime.config.provider == WebProviderChoice::Disabled
        || format == FetchFormat::Html
    {
        return false;
    }
    is_hop_remote_extract_eligible(runtime, url)
}

async fn check_local_hop_policy(runtime: &WebRuntime, target: &Url) -> Result<(), String> {
    check_url_host_policy(runtime.config.local_dns_policy(), target)?;
    if let Some(ref checker) = runtime.config.url_policy_checker {
        checker.check_url(target.as_str()).await?;
    }
    Ok(())
}

async fn execute_with_manual_redirects(
    runtime: &WebRuntime,
    start_url: &Url,
    format: FetchFormat,
    user_agent: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(Url, reqwest::Response, bool), Stage1Outcome> {
    let mut current = start_url.clone();
    if let Err(err) = check_local_hop_policy(runtime, &current).await {
        return Err(Stage1Outcome::TerminalError(err));
    }
    let mut remote_extract_allowed = is_hop_remote_extract_eligible(runtime, &current);

    for hop in 0..=MAX_REDIRECTS {
        let req = runtime
            .local_fetch_client
            .get(current.clone())
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::ACCEPT, format.accept_header())
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

        let resp = match send_cancellable_request(req, deadline, cancel).await {
            Ok(r) => r,
            Err(ProviderError::Cancelled) => {
                return Err(Stage1Outcome::TerminalError("Web fetch aborted".to_string()));
            }
            Err(err) => {
                return Err(Stage1Outcome::TerminalError(format!(
                    "Unable to fetch {current}: {err}"
                )));
            }
        };

        if let Some(addr) = resp.remote_addr() {
            let allow_loopback_test = {
                #[cfg(test)]
                {
                    runtime.config.allow_remote_extract_for_loopback_tests
                }
                #[cfg(not(test))]
                {
                    false
                }
            };
            if !allow_loopback_test && is_private_or_local_ip(addr.ip()) {
                remote_extract_allowed = false;
            }
        }

        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let Some(loc) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                return Err(Stage1Outcome::TerminalError(format!(
                    "HTTP {status} redirect from {current} is missing a valid Location header"
                )));
            };
            if hop >= MAX_REDIRECTS {
                return Err(Stage1Outcome::TerminalError(format!(
                    "Too many redirects (more than {MAX_REDIRECTS})"
                )));
            }
            let joined = current
                .join(&loc)
                .map_err(|e| Stage1Outcome::TerminalError(format!("Invalid redirect URL: {e}")))?;
            let next = normalize_and_validate_http_url(joined.as_str())
                .map_err(Stage1Outcome::TerminalError)?;
            if let Err(err) = check_local_hop_policy(runtime, &next).await {
                return Err(Stage1Outcome::TerminalError(err));
            }
            remote_extract_allowed &= is_hop_remote_extract_eligible(runtime, &next);
            current = next;
            continue;
        }

        return Ok((current, resp, remote_extract_allowed));
    }

    Err(Stage1Outcome::TerminalError(format!(
        "Too many redirects (more than {MAX_REDIRECTS})"
    )))
}

async fn run_stage1_local(
    runtime: &WebRuntime,
    url: &Url,
    format: FetchFormat,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Stage1Outcome {
    let (mut final_url, mut response, mut remote_extract_allowed) =
        match execute_with_manual_redirects(
            runtime,
            url,
            format,
            DEFAULT_BROWSER_UA,
            deadline,
            cancel,
        )
        .await
        {
            Ok(ok) => ok,
            Err(outcome) => return outcome,
        };

    let mut status = response.status().as_u16();
    let is_cf_challenge = status == 403
        && response
            .headers()
            .get("cf-mitigated")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("challenge"));

    if is_cf_challenge {
        match execute_with_manual_redirects(
            runtime,
            &final_url,
            format,
            "mehh-code",
            deadline,
            cancel,
        )
        .await
        {
            Ok((retry_url, retry_resp, retry_remote_allowed)) => {
                final_url = retry_url;
                response = retry_resp;
                remote_extract_allowed &= retry_remote_allowed;
                status = response.status().as_u16();
            }
            Err(outcome) => return outcome,
        }
    }

    if status >= 400 {
        let reason = response
            .status()
            .canonical_reason()
            .unwrap_or("Error")
            .to_string();
        if matches!(status, 403 | 429 | 503) {
            return Stage1Outcome::BlockedHttp {
                status,
                detail: format!("HTTP {status} {reason}"),
                remote_extract_allowed,
            };
        }
        return Stage1Outcome::TerminalError(format!(
            "HTTP {status} {reason} fetching {final_url}"
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if is_disallowed_image_mime(&mime) {
        return Stage1Outcome::TerminalError(format!(
            "Unsupported fetched image content type: {mime}"
        ));
    }
    if !mime.is_empty() && !is_textual_mime(&mime) {
        return Stage1Outcome::TerminalError(format!(
            "Unsupported fetched file content type: {mime}"
        ));
    }

    let bytes = match read_bounded_body(response, MAX_FETCH_BYTES, deadline, cancel).await {
        Ok(b) => b,
        Err(ProviderError::Cancelled) => {
            return Stage1Outcome::TerminalError("Web fetch aborted".to_string());
        }
        Err(err) => return Stage1Outcome::TerminalError(format!("Unable to fetch {final_url}: {err}")),
    };

    if looks_like_binary_bytes(&bytes) {
        return Stage1Outcome::TerminalError(
            "Unsupported binary response payload".to_string(),
        );
    }

    let raw_text = String::from_utf8_lossy(&bytes).into_owned();
    let is_html = mime.contains("text/html")
        || mime.contains("application/xhtml+xml")
        || (mime.is_empty() && looks_like_html_document(&raw_text));

    if !is_html || format == FetchFormat::Html {
        return Stage1Outcome::Ready {
            content_type,
            body: raw_text,
        };
    }

    let base_url = final_url.clone();
    let conversion = tokio::task::spawn_blocking(move || {
        let converted = match format {
            FetchFormat::Markdown => html_to_markdown(&raw_text, Some(&base_url)),
            FetchFormat::Text => html_to_text(&raw_text),
            FetchFormat::Html => unreachable!(),
        };
        let is_empty_spa = looks_like_empty_spa_shell(&raw_text, &converted);
        (converted, is_empty_spa)
    })
    .await;

    let (converted, is_empty_spa) = match conversion {
        Ok(pair) => pair,
        Err(e) => {
            return Stage1Outcome::TerminalError(format!("HTML conversion task failed: {e}"));
        }
    };

    if is_empty_spa {
        return Stage1Outcome::EmptySpaShell {
            content_type,
            rendered: converted,
            remote_extract_allowed,
        };
    }

    Stage1Outcome::Ready {
        content_type,
        body: converted,
    }
}

async fn run_stage2_extract(
    runtime: &WebRuntime,
    url: &Url,
    format: FetchFormat,
    deadline: Instant,
    cancel: &CancellationToken,
    on_update: Option<&AgentToolUpdateCallback>,
) -> Result<(String, String), Stage2Error> {
    if cancel.is_cancelled() {
        return Err(Stage2Error::Cancelled);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Stage2Error::Failed(
            "Request timed out before remote fallback".to_string(),
        ));
    }
    let progress_cb: Option<Box<ProgressFn>> = on_update.cloned().map(|cb| {
        let f: Box<ProgressFn> = Box::new(move |msg: &str| {
            cb(AgentToolResult::text(msg.to_string()));
        });
        f
    });

    let extracted = runtime
        .router
        .extract_fallback(
            runtime.config.provider,
            url.as_str(),
            remaining,
            cancel,
            progress_cb.as_deref(),
        )
        .await
        .map_err(|e| match e {
            ProviderError::Cancelled => Stage2Error::Cancelled,
            other => Stage2Error::Failed(other.to_string()),
        })?;

    if let Some(ref final_u) = extracted.page.final_url {
        let parsed_final = normalize_and_validate_http_url(final_u)
            .map_err(|e| Stage2Error::PolicyBlocked(format!("Invalid post-redirect URL from extractor: {e}")))?;
        check_url_host_policy(DnsPolicy::PublicOnly, &parsed_final)
            .map_err(Stage2Error::PolicyBlocked)?;
        if let Some(ref checker) = runtime.config.url_policy_checker {
            checker
                .check_url(parsed_final.as_str())
                .await
                .map_err(Stage2Error::PolicyBlocked)?;
        }
    }

    let body = match format {
        FetchFormat::Text => strip_markdown_to_text(&extracted.page.content),
        FetchFormat::Markdown | FetchFormat::Html => extracted.page.content,
    };
    Ok((extracted.provider.as_str().to_string(), body))
}

fn is_disallowed_image_mime(mime: &str) -> bool {
    mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet"
}

fn is_textual_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "image/svg+xml"
                | "image/vnd.fastbidsheet"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn looks_like_binary_bytes(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    bytes.starts_with(b"%PDF-")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"PK\x03\x04")
}

fn looks_like_html_document(text: &str) -> bool {
    let prefix = text.trim_start();
    let end = crate::core::output::floor_char_boundary(prefix, 256);
    let lower = prefix[..end].to_ascii_lowercase();
    lower.starts_with("<!doctype html") || lower.starts_with("<html")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{ProviderId, UrlPolicyChecker, WebConfig};
    use mehh_ai::ToolResultContentBlock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct DenyUrlChecker(String);

    #[async_trait]
    impl UrlPolicyChecker for DenyUrlChecker {
        async fn check_url(&self, url: &str) -> Result<(), String> {
            if url.contains(&self.0) {
                Err(format!("Denied by policy: {url}"))
            } else {
                Ok(())
            }
        }
    }

    async fn spawn_one_shot_server(
        responses: Vec<Vec<u8>>,
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reqs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reqs_clone = reqs.clone();
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                reqs_clone
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = sock.write_all(&resp).await;
            }
        });
        (addr, reqs)
    }

    fn http_resp(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn test_local_fetch_markdown_text_html() {
        let html = b"<h1>Hello</h1><p>World <strong>rust</strong></p>";
        let md_body = b"# Raw Markdown";
        let (addr, reqs) = spawn_one_shot_server(vec![
            http_resp("200 OK", "Content-Type: text/html; charset=utf-8\r\n", html),
            http_resp("200 OK", "Content-Type: text/html\r\n", html),
            http_resp("200 OK", "Content-Type: text/html\r\n", html),
            http_resp("200 OK", "Content-Type: text/markdown\r\n", md_body),
        ])
        .await;

        let cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: false,
            ..Default::default()
        };
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));

        let mut p1 = Map::new();
        p1.insert("url".into(), json!(format!("http://{addr}/page")));
        let r1 = tool
            .execute("f1", p1, CancellationToken::new(), None)
            .await
            .unwrap();
        let t1 = match &r1.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!(),
        };
        assert!(t1.contains("# Hello\n\nWorld **rust**"));

        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{addr}/page")));
        p2.insert("format".into(), json!("text"));
        let r2 = tool
            .execute("f2", p2, CancellationToken::new(), None)
            .await
            .unwrap();
        let t2 = match &r2.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!(),
        };
        assert!(t2.contains("Hello") && !t2.contains("**"));

        let mut p3 = Map::new();
        p3.insert("url".into(), json!(format!("http://{addr}/page")));
        p3.insert("format".into(), json!("html"));
        let r3 = tool
            .execute("f3", p3, CancellationToken::new(), None)
            .await
            .unwrap();
        let t3 = match &r3.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!(),
        };
        assert!(t3.contains("<h1>Hello</h1>"));

        let mut p4 = Map::new();
        p4.insert("url".into(), json!(format!("http://{addr}/md")));
        let r4 = tool
            .execute("f4", p4, CancellationToken::new(), None)
            .await
            .unwrap();
        let t4 = match &r4.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!(),
        };
        assert_eq!(t4, "# Raw Markdown");
        assert!(reqs.lock().unwrap()[0].to_ascii_lowercase().contains("text/markdown;q=1.0"));
    }

    #[tokio::test]
    async fn test_cloudflare_challenge_retries_with_honest_ua() {
        let (addr, reqs) = spawn_one_shot_server(vec![
            http_resp("403 Forbidden", "cf-mitigated: challenge\r\n", b"challenge"),
            http_resp("200 OK", "Content-Type: text/plain\r\n", b"passed challenge"),
        ])
        .await;

        let cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: false,
            ..Default::default()
        };
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));
        let mut params = Map::new();
        params.insert("url".into(), json!(format!("http://{addr}/cf")));
        let res = tool
            .execute("cf", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!(),
        };
        assert_eq!(txt, "passed challenge");
        assert!(reqs.lock().unwrap()[1].contains("mehh-code\r\n"));
    }

    #[tokio::test]
    async fn test_redirect_limit_and_ssrf_and_policy_blocks() {
        // 1. 11 redirects exceed MAX_REDIRECTS (10)
        let mut redirs = Vec::new();
        for i in 0..11 {
            redirs.push(http_resp(
                "302 Found",
                &format!("Location: /hop{}\r\n", i + 1),
                b"",
            ));
        }
        let (addr1, _) = spawn_one_shot_server(redirs).await;
        let tool1 = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(WebConfig {
            allow_private_urls: true,
            ..Default::default()
        })));
        let mut p1 = Map::new();
        p1.insert("url".into(), json!(format!("http://{addr1}/start")));
        let e1 = tool1
            .execute("r1", p1, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e1.contains("Too many redirects"));

        // 2. PublicOnly blocks localhost/private upfront and blocks redirect to metadata IP
        //    without invoking Stage 2 remote fallback.
        let (addr2, _) = spawn_one_shot_server(vec![
            http_resp(
                "302 Found",
                "Location: http://169.254.169.254/latest/meta-data\r\n",
                b"",
            ),
            http_resp(
                "302 Found",
                "Location: //169.254.169.254/latest/meta-data\r\n",
                b"",
            ),
        ])
        .await;
        let (spy_addr, spy_reqs) = spawn_one_shot_server(vec![]).await;
        let mut meta_cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            provider: WebProviderChoice::Fixed(ProviderId::Keenable),
            allow_remote_extract_for_loopback_tests: true,
            ..Default::default()
        };
        meta_cfg.endpoints.keenable_base = format!("http://{spy_addr}");
        let tool_meta = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(meta_cfg)));
        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{addr2}/start")));
        let e2 = tool_meta
            .execute("r2", p2, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e2.contains("Blocked private or restricted IP"));

        let mut p2b = Map::new();
        p2b.insert("url".into(), json!(format!("http://{addr2}/proto-rel")));
        let e2b = tool_meta
            .execute("r2b", p2b, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e2b.contains("Blocked private or restricted IP"));
        assert!(spy_reqs.lock().unwrap().is_empty());

        // PublicOnly also rejects protocol-relative redirect resolved to 127.0.0.1.
        let public_rt = WebRuntime::for_tests(WebConfig {
            allow_private_urls: false,
            ..Default::default()
        });
        let joined_loopback = Url::parse("https://example.com/start")
            .unwrap()
            .join("//127.0.0.1:9/admin")
            .unwrap();
        assert!(check_local_hop_policy(&public_rt, &joined_loopback).await.is_err());

        // 3. Policy checker blocks redirect hop without invoking Stage 2
        let (addr3, _) = spawn_one_shot_server(vec![http_resp(
            "302 Found",
            "Location: /blocked-secret-admin\r\n",
            b"",
        )])
        .await;
        let mut pol_cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            provider: WebProviderChoice::Fixed(ProviderId::Keenable),
            allow_remote_extract_for_loopback_tests: true,
            ..Default::default()
        }
        .with_url_policy_checker(Arc::new(DenyUrlChecker("blocked-secret-admin".into())));
        pol_cfg.endpoints.keenable_base = format!("http://{spy_addr}");
        let tool_pol = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(pol_cfg)));
        let mut p3 = Map::new();
        p3.insert("url".into(), json!(format!("http://{addr3}/start")));
        let e3 = tool_pol
            .execute("r3", p3, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e3.contains("Denied by policy"));
        assert!(spy_reqs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_stage2_fallback_on_403_and_spa_shell() {
        let (local_addr, _) = spawn_one_shot_server(vec![
            http_resp("403 Forbidden", "Content-Type: text/html\r\n", b"WAF blocked"),
            http_resp(
                "200 OK",
                "Content-Type: text/html\r\n",
                b"<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>",
            ),
            http_resp("404 Not Found", "Content-Type: text/plain\r\n", b"missing"),
            http_resp("403 Forbidden", "Content-Type: text/html\r\n", b"WAF"),
            http_resp("403 Forbidden", "Content-Type: text/html\r\n", b"WAF"),
            http_resp("403 Forbidden", "Content-Type: text/html\r\n", b"WAF"),
        ])
        .await;

        let (ext_addr, ext_reqs) = spawn_one_shot_server(vec![
            http_resp(
                "200 OK",
                "Content-Type: application/json\r\n",
                json!({ "url": "https://docs.rs/final", "title": "Docs", "content": "# Extracted" })
                    .to_string()
                    .as_bytes(),
            ),
            http_resp(
                "200 OK",
                "Content-Type: application/json\r\n",
                json!({ "url": "https://docs.rs/spa", "title": "SPA", "content": "# SPA Content" })
                    .to_string()
                    .as_bytes(),
            ),
            http_resp(
                "200 OK",
                "Content-Type: application/json\r\n",
                json!({ "url": "http://127.0.0.1:9/ssrf", "title": "Bad", "content": "leak" })
                    .to_string()
                    .as_bytes(),
            ),
        ])
        .await;

        let mut cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            provider: WebProviderChoice::Fixed(ProviderId::Keenable),
            allow_remote_extract_for_loopback_tests: true,
            ..Default::default()
        };
        cfg.endpoints.keenable_base = format!("http://{ext_addr}");
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));

        // 1. 403 triggers fallback
        let mut p1 = Map::new();
        p1.insert("url".into(), json!(format!("http://{local_addr}/403")));
        let r1 = tool
            .execute("s1", p1, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(r1.details.as_ref().unwrap()["source"], "keenable");

        // 2. Empty SPA HTML triggers fallback
        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{local_addr}/spa")));
        let r2 = tool
            .execute("s2", p2, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(r2.details.as_ref().unwrap()["source"], "keenable");

        // 3. 404 does NOT trigger fallback
        let mut p3 = Map::new();
        p3.insert("url".into(), json!(format!("http://{local_addr}/404")));
        assert!(tool
            .execute("s3", p3, CancellationToken::new(), None)
            .await
            .is_err());

        // 4. format == "html" does NOT trigger fallback
        let mut p4 = Map::new();
        p4.insert("url".into(), json!(format!("http://{local_addr}/html403")));
        p4.insert("format".into(), json!("html"));
        assert!(tool
            .execute("s4", p4, CancellationToken::new(), None)
            .await
            .is_err());

        // 5. Secret-bearing URL does NOT trigger fallback
        let mut p5 = Map::new();
        p5.insert(
            "url".into(),
            json!(format!("http://{local_addr}/secret?token=secret123")),
        );
        assert!(tool
            .execute("s5", p5, CancellationToken::new(), None)
            .await
            .is_err());

        // 6. Stage 2 final_url pointing to 127.0.0.1 is rejected by PublicOnly policy
        let mut p6 = Map::new();
        p6.insert("url".into(), json!(format!("http://{local_addr}/ssrf-final")));
        assert!(tool
            .execute("s6", p6, CancellationToken::new(), None)
            .await
            .is_err());

        assert_eq!(ext_reqs.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_mime_gate_and_5mib_cap() {
        let oversized = vec![b'a'; MAX_FETCH_BYTES + 1024];
        let (addr, _) = spawn_one_shot_server(vec![
            http_resp("200 OK", "Content-Type: image/png\r\n", b"png"),
            http_resp("200 OK", "Content-Type: application/pdf\r\n", b"%PDF-1.4"),
            http_resp("200 OK", "", b"%PDF-1.7 binary"),
            http_resp("200 OK", "Content-Type: text/plain\r\n", &oversized),
        ])
        .await;

        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(WebConfig {
            allow_private_urls: true,
            fetch_fallback: false,
            ..Default::default()
        })));

        for (i, path) in ["img", "pdf", "sniff", "huge"].iter().enumerate() {
            let mut p = Map::new();
            p.insert("url".into(), json!(format!("http://{addr}/{path}")));
            assert!(
                tool.execute(&format!("m{i}"), p, CancellationToken::new(), None)
                    .await
                    .is_err(),
                "expected rejection for {path}"
            );
        }
    }

    #[tokio::test]
    async fn test_stage2_blocks_post_redirect_secret_and_propagates_cancel_on_spa() {
        let (local_addr, _) = spawn_one_shot_server(vec![
            http_resp("302 Found", "Location: /auth?token=SECRET_XYZ\r\n", b""),
            http_resp("403 Forbidden", "Content-Type: text/html\r\n", b"WAF"),
            http_resp(
                "200 OK",
                "Content-Type: text/html\r\n",
                b"<html><body><div id=\"root\"></div><script></script></body></html>",
            ),
        ])
        .await;

        let ext_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ext_addr = ext_listener.local_addr().unwrap();
        let ext_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ext_hits_bg = ext_hits.clone();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let mut accepted_tx = Some(accepted_tx);
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = ext_listener.accept().await {
                ext_hits_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(tx) = accepted_tx.take() {
                    let _ = tx.send(());
                }
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let mut cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            provider: WebProviderChoice::Fixed(ProviderId::Keenable),
            allow_remote_extract_for_loopback_tests: true,
            ..Default::default()
        };
        cfg.endpoints.keenable_base = format!("http://{ext_addr}");
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));

        // Redirect onto ?token=SECRET_XYZ must NOT invoke Stage 2 remote fallback.
        let mut p1 = Map::new();
        p1.insert("url".into(), json!(format!("http://{local_addr}/clean")));
        assert!(tool
            .execute("sec_redir", p1, CancellationToken::new(), None)
            .await
            .is_err());
        assert_eq!(ext_hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Cancellation during Stage 2 on EmptySpaShell must return Err, not Ok.
        let cancel = CancellationToken::new();
        let c_bg = cancel.clone();
        tokio::spawn(async move {
            let _ = accepted_rx.await;
            c_bg.cancel();
        });
        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{local_addr}/spa")));
        let err = tool
            .execute("spa_cancel", p2, cancel, None)
            .await
            .expect_err("cancelled Stage 2 on SPA must fail instead of returning empty shell");
        assert_eq!(ext_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(err.to_ascii_lowercase().contains("abort"), "got: {err}");
    }

    #[tokio::test]
    async fn test_redirect_missing_location_and_invalid_param_types() {
        let (addr, _) = spawn_one_shot_server(vec![http_resp(
            "302 Found",
            "Content-Type: text/plain\r\n",
            b"redirecting without location",
        )])
        .await;
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(WebConfig {
            allow_private_urls: true,
            fetch_fallback: false,
            ..Default::default()
        })));

        let mut p1 = Map::new();
        p1.insert("url".into(), json!(format!("http://{addr}/noloc")));
        assert!(tool
            .execute("noloc", p1, CancellationToken::new(), None)
            .await
            .is_err());

        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{addr}/x")));
        p2.insert("timeout".into(), json!(-5));
        assert!(tool
            .execute("bad_t", p2, CancellationToken::new(), None)
            .await
            .is_err());

        let mut p3 = Map::new();
        p3.insert("url".into(), json!(format!("http://{addr}/x")));
        p3.insert("format".into(), json!(123));
        assert!(tool
            .execute("bad_f", p3, CancellationToken::new(), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_stage2_policy_blocked_on_spa_returns_error() {
        let (local_addr, _) = spawn_one_shot_server(vec![http_resp(
            "200 OK",
            "Content-Type: text/html\r\n",
            b"<html><body><div id=\"root\"></div><script></script></body></html>",
        )])
        .await;
        let (ext_addr, _) = spawn_one_shot_server(vec![http_resp(
            "200 OK",
            "Content-Type: application/json\r\n",
            json!({ "url": "http://127.0.0.1:9/ssrf", "title": "Bad", "content": "leak" })
                .to_string()
                .as_bytes(),
        )])
        .await;

        let mut cfg = WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            provider: WebProviderChoice::Fixed(ProviderId::Keenable),
            allow_remote_extract_for_loopback_tests: true,
            ..Default::default()
        };
        cfg.endpoints.keenable_base = format!("http://{ext_addr}");
        let tool = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));

        let mut params = Map::new();
        params.insert("url".into(), json!(format!("http://{local_addr}/spa")));
        let err = tool
            .execute("spa_policy", params, CancellationToken::new(), None)
            .await
            .expect_err("Stage 2 policy block on SPA shell must fail closed");
        let lower = err.to_ascii_lowercase();
        assert!(
            lower.contains("policy") || lower.contains("private") || lower.contains("loopback") || lower.contains("restricted"),
            "got: {err}"
        );
    }
}
