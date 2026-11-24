//! Built-in `web_fetch` AgentTool: local-first HTTP fetch + HTML-to-Markdown + automatic keyless fallback.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::http_safety::{
    check_url_host_policy, is_eligible_for_remote_extract, normalize_and_validate_http_url,
    DnsPolicy,
};
use reqwest::Url;
use serde_json::{json, Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::html_markdown::{
    html_to_markdown, html_to_text, looks_like_empty_spa_shell, strip_markdown_to_text,
};
use super::mcp_rpc::{read_bounded_body, send_cancellable_request};
use super::output::format_fetch_content;
use super::router::ProgressFn;
use super::types::{ProviderError, WebProviderChoice};
use super::WebRuntime;

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
    },
    BlockedHttp {
        status: u16,
        detail: String,
    },
    TerminalError(String),
}

/// Built-in web fetch tool using local-first HTTP fetch and keyless extract fallback.
#[derive(Clone)]
pub struct WebFetchTool {
    runtime: Result<Arc<WebRuntime>, String>,
}

impl WebFetchTool {
    pub fn new(runtime: Result<Arc<WebRuntime>, String>) -> Self {
        Self { runtime }
    }

    pub fn from_runtime(runtime: Arc<WebRuntime>) -> Self {
        Self::new(Ok(runtime))
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
        "Fetch content from an HTTP or HTTPS URL and return it as markdown (default), plain text, or HTML. This tool is read-only. Very large responses are truncated inline with the full text saved to a temporary file readable via read."
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
        let runtime = self.runtime.as_ref().map_err(Clone::clone)?;
        if !runtime.config.enabled {
            return Err("Web fetch is disabled in settings".to_string());
        }

        let raw_url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required 'url' parameter".to_string())?;
        let format = FetchFormat::from_param(params.get("format").and_then(|v| v.as_str()))?;
        let timeout_secs = match params.get("timeout").and_then(|v| v.as_u64()) {
            Some(t) if (1..=120).contains(&t) => t,
            Some(t) => {
                return Err(format!(
                    "Invalid timeout {t}: expected integer seconds between 1 and 120"
                ));
            }
            None => runtime.config.effective_timeout_seconds(),
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
            } => {
                if can_use_remote_fallback(runtime, &url, format) {
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
                        Err(_) => ("local".to_string(), content_type, rendered),
                    }
                } else {
                    ("local".to_string(), content_type, rendered)
                }
            }
            Stage1Outcome::BlockedHttp { status, detail } => {
                if can_use_remote_fallback(runtime, &url, format) {
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
                        Err(fallback_err) => {
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

        let formatted = format_fetch_content(&content, format.spill_extension(&content_type))?;
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

fn can_use_remote_fallback(runtime: &WebRuntime, url: &Url, format: FetchFormat) -> bool {
    if !runtime.config.fetch_fallback
        || runtime.config.provider == WebProviderChoice::Disabled
        || format == FetchFormat::Html
    {
        return false;
    }
    #[cfg(test)]
    if runtime.config.allow_remote_extract_for_loopback_tests {
        return !mehh_ai::http_safety::url_contains_potential_secret(url);
    }
    is_eligible_for_remote_extract(url).is_ok()
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
) -> Result<(Url, reqwest::Response), Stage1Outcome> {
    let mut current = start_url.clone();
    if let Err(err) = check_local_hop_policy(runtime, &current).await {
        return Err(Stage1Outcome::TerminalError(err));
    }

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

        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if let Some(loc) = location {
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
                current = next;
                continue;
            }
        }

        return Ok((current, resp));
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
    let (mut final_url, mut response) = match execute_with_manual_redirects(
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
            Ok((retry_url, retry_resp)) => {
                final_url = retry_url;
                response = retry_resp;
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

    let converted = match format {
        FetchFormat::Markdown => html_to_markdown(&raw_text, Some(&final_url)),
        FetchFormat::Text => html_to_text(&raw_text),
        FetchFormat::Html => unreachable!(),
    };

    if looks_like_empty_spa_shell(&raw_text, &converted) {
        return Stage1Outcome::EmptySpaShell {
            content_type,
            rendered: converted,
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
) -> Result<(String, String), String> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(1));
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
        .map_err(|e| e.to_string())?;

    if let Some(ref final_u) = extracted.page.final_url {
        let parsed_final = normalize_and_validate_http_url(final_u)
            .map_err(|e| format!("Invalid post-redirect URL from extractor: {e}"))?;
        check_url_host_policy(DnsPolicy::PublicOnly, &parsed_final)?;
        if let Some(ref checker) = runtime.config.url_policy_checker {
            checker.check_url(parsed_final.as_str()).await?;
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
    let sample = &bytes[..bytes.len().min(512)];
    if sample.contains(&0) {
        return true;
    }
    sample.starts_with(b"%PDF-")
        || sample.starts_with(b"\x89PNG\r\n\x1a\n")
        || sample.starts_with(b"\xff\xd8\xff")
        || sample.starts_with(b"PK\x03\x04")
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
        let (addr2, _) = spawn_one_shot_server(vec![http_resp(
            "302 Found",
            "Location: http://169.254.169.254/latest/meta-data\r\n",
            b"",
        )])
        .await;
        let tool_meta = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(WebConfig {
            allow_private_urls: true,
            fetch_fallback: true,
            ..Default::default()
        })));
        let mut p2 = Map::new();
        p2.insert("url".into(), json!(format!("http://{addr2}/start")));
        let e2 = tool_meta
            .execute("r2", p2, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e2.contains("Blocked private or restricted IP"));

        // 3. Policy checker blocks redirect hop
        let (addr3, _) = spawn_one_shot_server(vec![http_resp(
            "302 Found",
            "Location: /blocked-secret-admin\r\n",
            b"",
        )])
        .await;
        let tool_pol = WebFetchTool::from_runtime(Arc::new(WebRuntime::for_tests(
            WebConfig {
                allow_private_urls: true,
                fetch_fallback: true,
                ..Default::default()
            }
            .with_url_policy_checker(Arc::new(DenyUrlChecker("blocked-secret-admin".into()))),
        )));
        let mut p3 = Map::new();
        p3.insert("url".into(), json!(format!("http://{addr3}/start")));
        let e3 = tool_pol
            .execute("r3", p3, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(e3.contains("Denied by policy"));
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
}
