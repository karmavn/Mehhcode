//! Wire-protocol adapters for the 6 keyless search providers and 4 keyless extract providers.

use std::sync::OnceLock;
use std::time::Duration;

use mehh_ai::http_safety::DnsPolicy;
use regex::Regex;
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::mcp_rpc::{
    call_mcp_tool, classify_http_status, read_bounded_body, read_error_body_snippet,
    send_cancellable_request, snippet_for_error, MAX_PROVIDER_ERROR_BODY_BYTES,
    MAX_PROVIDER_RESPONSE_BYTES, WEB_USER_AGENT,
};
use super::native::{search_mehh, NativeEndpoints, NativeSearchContext};
use super::types::{ExtractedPage, ProviderError, ProviderId, SearchHit};

/// Configurable provider endpoints (production constants by default, overrideable in tests).
#[derive(Debug, Clone)]
pub struct WebEndpoints {
    pub native: NativeEndpoints,
    pub exa_mcp: String,
    pub parallel_mcp: String,
    pub firecrawl_mcp: String,
    pub firecrawl_scrape: String,
    pub tavily_search: String,
    pub tinyfish_mcp: String,
    pub keenable_base: String,
}

impl WebEndpoints {
    pub fn production() -> Self {
        Self {
            native: NativeEndpoints::production(),
            exa_mcp: "https://mcp.exa.ai/mcp".to_string(),
            parallel_mcp: "https://search.parallel.ai/mcp".to_string(),
            firecrawl_mcp: "https://mcp.firecrawl.dev/v2/mcp".to_string(),
            firecrawl_scrape: "https://api.firecrawl.dev/v2/scrape".to_string(),
            tavily_search: "https://api.tavily.com/search".to_string(),
            tinyfish_mcp: "https://agent.tinyfish.ai/mcp".to_string(),
            keenable_base: "https://api.keenable.ai".to_string(),
        }
    }
}

impl Default for WebEndpoints {
    fn default() -> Self {
        Self::production()
    }
}

/// Executes a search request against one built-in provider.
pub async fn execute_search(
    client: &reqwest::Client,
    endpoints: &WebEndpoints,
    session_id: &str,
    provider: ProviderId,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    match provider {
        ProviderId::Mehh => {
            let ctx = NativeSearchContext {
                client,
                enrich_client: client,
                enrich_dns_policy: DnsPolicy::PublicOnly,
                url_policy_checker: None,
                endpoints: &endpoints.native,
                state: None,
            };
            search_mehh(&ctx, query, timeout, cancel).await
        }
        ProviderId::Exa => search_exa(client, &endpoints.exa_mcp, query, timeout, cancel).await,
        ProviderId::Parallel => {
            search_parallel(
                client,
                &endpoints.parallel_mcp,
                session_id,
                query,
                timeout,
                cancel,
            )
            .await
        }
        ProviderId::Firecrawl => {
            search_firecrawl(client, &endpoints.firecrawl_mcp, query, timeout, cancel).await
        }
        ProviderId::Tavily => {
            search_tavily(client, &endpoints.tavily_search, query, timeout, cancel).await
        }
        ProviderId::TinyFish => {
            search_tinyfish(client, &endpoints.tinyfish_mcp, query, timeout, cancel).await
        }
        ProviderId::Keenable => {
            search_keenable(client, &endpoints.keenable_base, query, timeout, cancel).await
        }
    }
}

/// Executes a page extraction request against an extract-capable keyless provider.
pub async fn execute_extract(
    client: &reqwest::Client,
    endpoints: &WebEndpoints,
    session_id: &str,
    provider: ProviderId,
    url: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExtractedPage, ProviderError> {
    match provider {
        ProviderId::Exa => extract_exa(client, &endpoints.exa_mcp, url, timeout, cancel).await,
        ProviderId::Parallel => {
            extract_parallel(
                client,
                &endpoints.parallel_mcp,
                session_id,
                url,
                timeout,
                cancel,
            )
            .await
        }
        ProviderId::Firecrawl => {
            extract_firecrawl(client, &endpoints.firecrawl_scrape, url, timeout, cancel).await
        }
        ProviderId::Keenable => {
            extract_keenable(client, &endpoints.keenable_base, url, timeout, cancel).await
        }
        ProviderId::Mehh | ProviderId::Tavily | ProviderId::TinyFish => {
            Err(ProviderError::BadRequest(format!(
                "Provider '{}' does not support page extraction",
                provider.as_str()
            )))
        }
    }
}

async fn send_rest_json(
    req: reqwest::RequestBuilder,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(u16, Option<String>, String), ProviderError> {
    let deadline = Instant::now() + timeout;
    let resp = send_cancellable_request(req, deadline, cancel).await?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if status >= 400 {
        let body =
            read_error_body_snippet(resp, MAX_PROVIDER_ERROR_BODY_BYTES, deadline, cancel).await?;
        return Ok((status, retry_after, body));
    }
    let bytes = read_bounded_body(resp, MAX_PROVIDER_RESPONSE_BYTES, deadline, cancel).await?;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, retry_after, body))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_json_value(raw: &str, context: &str) -> Result<Value, ProviderError> {
    serde_json::from_str(raw).map_err(|err| ProviderError::VendorUnavailable {
        status: None,
        detail: format!("Invalid JSON from {context}: {err}"),
    })
}

// --- Exa --------------------------------------------------------------------

async fn search_exa(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let res = call_mcp_tool(
        client,
        endpoint,
        "web_search_exa",
        json!({ "query": query, "numResults": 8 }),
        &[],
        timeout,
        cancel,
    )
    .await?;
    Ok(parse_exa_blocks(&res.text))
}

fn parse_exa_blocks(text: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.trim() == "---" {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    for lines in blocks {
        let mut url: Option<String> = None;
        let mut title: Option<String> = None;
        let mut published: Option<String> = None;
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_body = false;

        for line in lines {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Title:") {
                in_body = false;
                let val = rest.trim();
                if !val.is_empty() && val != "N/A" {
                    title = Some(val.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix("URL:") {
                in_body = false;
                let val = rest.trim();
                if !val.is_empty() {
                    url = Some(val.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix("Published:") {
                in_body = false;
                let val = rest.trim();
                if !val.is_empty() && val != "N/A" {
                    published = Some(val.to_string());
                }
            } else if trimmed.starts_with("Author:") {
                in_body = false;
            } else if let Some(rest) = trimmed
                .strip_prefix("Highlights:")
                .or_else(|| trimmed.strip_prefix("Text:"))
            {
                in_body = true;
                let first = rest.trim();
                if !first.is_empty() {
                    body_lines.push(first);
                }
            } else if in_body {
                body_lines.push(trimmed);
            }
        }

        if let Some(u) = url {
            let content_text = body_lines.join("\n").trim().to_string();
            hits.push(SearchHit {
                url: u,
                title,
                published,
                content: (!content_text.is_empty()).then_some(content_text),
            });
        }
    }

    hits
}

fn exa_origin_status(text: &str) -> Option<u16> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\bCRAWL_(?:HTTP_(\d{3})|(NOT_FOUND))\s*$")
            .expect("valid Exa crawl status regex")
    });
    let caps = re.captures(text.trim())?;
    if let Some(m) = caps.get(1) {
        m.as_str().parse::<u16>().ok().filter(|s| *s >= 400)
    } else if caps.get(2).is_some() {
        Some(404)
    } else {
        None
    }
}

async fn extract_exa(
    client: &reqwest::Client,
    endpoint: &str,
    url: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExtractedPage, ProviderError> {
    let res = match call_mcp_tool(
        client,
        endpoint,
        "web_fetch_exa",
        json!({ "urls": [url] }),
        &[],
        timeout,
        cancel,
    )
    .await
    {
        Ok(ok) => ok,
        Err(ProviderError::VendorUnavailable { status, detail }) => {
            if let Some(origin_status) = exa_origin_status(&detail) {
                return Err(ProviderError::OriginBlocked {
                    status: origin_status,
                    detail,
                });
            }
            return Err(ProviderError::VendorUnavailable { status, detail });
        }
        Err(ProviderError::RateLimited {
            retry_after,
            detail,
        }) => {
            if let Some(origin_status) = exa_origin_status(&detail) {
                return Err(ProviderError::OriginBlocked {
                    status: origin_status,
                    detail,
                });
            }
            return Err(ProviderError::RateLimited {
                retry_after,
                detail,
            });
        }
        Err(err) => return Err(err),
    };

    let content = res.text.trim().to_string();
    if content.is_empty() {
        return Err(ProviderError::VendorUnavailable {
            status: None,
            detail: "Exa returned empty page content".to_string(),
        });
    }
    let title = content
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("# ")
                .or_else(|| line.strip_prefix("Title:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    Ok(ExtractedPage {
        url: url.to_string(),
        final_url: None,
        title,
        content,
    })
}

// --- Parallel ---------------------------------------------------------------

async fn search_parallel(
    client: &reqwest::Client,
    endpoint: &str,
    session_id: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let res = call_mcp_tool(
        client,
        endpoint,
        "web_search",
        json!({
            "objective": query,
            "search_queries": [query],
            "session_id": session_id,
        }),
        &[],
        timeout,
        cancel,
    )
    .await?;

    let payload = match res.structured_content {
        Some(v) => v,
        None => parse_json_value(&res.text, "Parallel search")?,
    };
    let results = payload
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "Parallel search payload missing results array".to_string(),
        })?;

    let mut hits = Vec::new();
    for item in results {
        let Some(url) = non_empty(item.get("url").and_then(|v| v.as_str())) else {
            continue;
        };
        let title = non_empty(item.get("title").and_then(|v| v.as_str()));
        let published = non_empty(item.get("publish_date").and_then(|v| v.as_str()));
        let excerpts: Vec<&str> = item
            .get("excerpts")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let content = (!excerpts.is_empty()).then(|| excerpts.join("\n\n"));
        hits.push(SearchHit {
            url,
            title,
            published,
            content,
        });
    }
    Ok(hits)
}

async fn extract_parallel(
    client: &reqwest::Client,
    endpoint: &str,
    session_id: &str,
    url: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExtractedPage, ProviderError> {
    let res = call_mcp_tool(
        client,
        endpoint,
        "web_fetch",
        json!({
            "urls": [url],
            "objective": "Full page content",
            "session_id": session_id,
        }),
        &[],
        timeout,
        cancel,
    )
    .await?;

    let payload = match res.structured_content {
        Some(v) => v,
        None => parse_json_value(&res.text, "Parallel extract")?,
    };

    if let Some(first) = payload
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
    {
        let final_url = non_empty(first.get("url").and_then(|v| v.as_str()));
        let title = non_empty(first.get("title").and_then(|v| v.as_str()));
        let excerpts: Vec<&str> = first
            .get("excerpts")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
            .collect();
        let content = non_empty(first.get("full_content").and_then(|v| v.as_str()))
            .or_else(|| non_empty(first.get("content").and_then(|v| v.as_str())))
            .unwrap_or_else(|| excerpts.join("\n\n").trim().to_string());
        if !content.is_empty() {
            return Ok(ExtractedPage {
                url: url.to_string(),
                final_url,
                title,
                content,
            });
        }
    }

    if let Some(err_entry) = payload
        .get("errors")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
    {
        let origin_status = err_entry
            .get("http_status_code")
            .and_then(|v| v.as_u64())
            .and_then(|s| u16::try_from(s).ok())
            .filter(|s| *s >= 400);
        let detail = non_empty(err_entry.get("content").and_then(|v| v.as_str()))
            .or_else(|| non_empty(err_entry.get("error_type").and_then(|v| v.as_str())))
            .unwrap_or_else(|| "Parallel extract returned error entry".to_string());
        if let Some(status) = origin_status {
            return Err(ProviderError::OriginBlocked { status, detail });
        }
        return Err(ProviderError::VendorUnavailable {
            status: None,
            detail,
        });
    }

    Err(ProviderError::VendorUnavailable {
        status: None,
        detail: "Parallel extract returned no content".to_string(),
    })
}

// --- Firecrawl --------------------------------------------------------------

async fn search_firecrawl(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let res = call_mcp_tool(
        client,
        endpoint,
        "firecrawl_search",
        json!({ "query": query, "limit": 8 }),
        &[],
        timeout,
        cancel,
    )
    .await?;

    let payload = if !res.text.trim().is_empty() {
        parse_json_value(&res.text, "Firecrawl search")?
    } else if let Some(structured) = res.structured_content {
        structured
    } else {
        return Err(ProviderError::VendorUnavailable {
            status: None,
            detail: "Firecrawl search returned empty response".to_string(),
        });
    };

    let items = payload
        .get("data")
        .and_then(|d| d.get("web").or(Some(d)))
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "Firecrawl search response missing data.web array".to_string(),
        })?;

    let mut hits = Vec::new();
    for item in items {
        let Some(url) = non_empty(item.get("url").and_then(|v| v.as_str())) else {
            continue;
        };
        hits.push(SearchHit {
            url,
            title: non_empty(item.get("title").and_then(|v| v.as_str())),
            published: None,
            content: non_empty(item.get("description").and_then(|v| v.as_str())),
        });
    }
    Ok(hits)
}

async fn extract_firecrawl(
    client: &reqwest::Client,
    endpoint: &str,
    url: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExtractedPage, ProviderError> {
    let req = client
        .post(endpoint)
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&json!({
            "url": url,
            "formats": ["markdown"],
        }));

    let (status, retry_after, body) = send_rest_json(req, timeout, cancel).await?;
    if status >= 400 {
        return Err(classify_http_status(
            status,
            retry_after.as_deref(),
            &snippet_for_error(&body),
        ));
    }

    let parsed = parse_json_value(&body, "Firecrawl scrape")?;
    let payload = parsed.get("data").unwrap_or(&parsed);
    let metadata = payload.get("metadata");

    if let Some(meta) = metadata {
        let origin_status = meta
            .get("statusCode")
            .or_else(|| meta.get("status_code"))
            .and_then(|v| v.as_u64())
            .and_then(|s| u16::try_from(s).ok())
            .filter(|s| *s >= 400);
        if let Some(code) = origin_status {
            let reason = non_empty(meta.get("error").and_then(|v| v.as_str()))
                .unwrap_or_else(|| format!("Origin returned HTTP {code}"));
            return Err(ProviderError::OriginBlocked {
                status: code,
                detail: reason,
            });
        }
    }

    let final_url = metadata.and_then(|m| non_empty(m.get("url").and_then(|v| v.as_str())));
    let title = metadata.and_then(|m| non_empty(m.get("title").and_then(|v| v.as_str())));
    let content = non_empty(payload.get("markdown").and_then(|v| v.as_str()))
        .or_else(|| non_empty(payload.get("html").and_then(|v| v.as_str())))
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "Firecrawl scrape returned empty content".to_string(),
        })?;

    Ok(ExtractedPage {
        url: url.to_string(),
        final_url,
        title,
        content,
    })
}

// --- Tavily -----------------------------------------------------------------

async fn search_tavily(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let req = client
        .post(endpoint)
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-Tavily-Access-Mode", "keyless")
        .header("X-Client-Name", "mehh-code")
        .json(&json!({
            "query": query,
            "search_depth": "basic",
            "chunks_per_source": 3,
            "max_results": 8,
        }));

    let (status, retry_after, body) = send_rest_json(req, timeout, cancel).await?;
    if status >= 400 {
        return Err(classify_http_status(
            status,
            retry_after.as_deref(),
            &snippet_for_error(&body),
        ));
    }

    let parsed = parse_json_value(&body, "Tavily search")?;
    let results = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "Tavily response missing results array".to_string(),
        })?;

    let mut hits = Vec::new();
    for item in results {
        let Some(url) = non_empty(item.get("url").and_then(|v| v.as_str())) else {
            continue;
        };
        hits.push(SearchHit {
            url,
            title: non_empty(item.get("title").and_then(|v| v.as_str())),
            published: None,
            content: non_empty(item.get("content").and_then(|v| v.as_str())),
        });
    }
    Ok(hits)
}

// --- TinyFish ---------------------------------------------------------------

async fn search_tinyfish(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let res = call_mcp_tool(
        client,
        endpoint,
        "search",
        json!({ "query": query }),
        &[("X-TinyFish-Access-Mode", "keyless")],
        timeout,
        cancel,
    )
    .await?;

    let payload = if !res.text.trim().is_empty() {
        parse_json_value(&res.text, "TinyFish search")?
    } else if let Some(structured) = res.structured_content {
        structured
    } else {
        return Err(ProviderError::VendorUnavailable {
            status: None,
            detail: "TinyFish search returned empty response".to_string(),
        });
    };

    let results = payload
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "TinyFish response missing results array".to_string(),
        })?;

    let mut hits = Vec::new();
    for item in results {
        let Some(url) = non_empty(item.get("url").and_then(|v| v.as_str())) else {
            continue;
        };
        hits.push(SearchHit {
            url,
            title: non_empty(item.get("title").and_then(|v| v.as_str())),
            published: None,
            content: non_empty(item.get("snippet").and_then(|v| v.as_str())),
        });
    }
    Ok(hits)
}

// --- Keenable ---------------------------------------------------------------

async fn search_keenable(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    let endpoint = format!("{}/v1/search/public", base_url.trim_end_matches('/'));
    let req = client
        .post(&endpoint)
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-Keenable-Title", "mehh-code")
        .json(&json!({
            "query": query,
            "max_results": 8,
        }));

    let (status, retry_after, body) = send_rest_json(req, timeout, cancel).await?;
    if status >= 400 {
        return Err(classify_http_status(
            status,
            retry_after.as_deref(),
            &snippet_for_error(&body),
        ));
    }

    let parsed = parse_json_value(&body, "Keenable search")?;
    let results = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::VendorUnavailable {
            status: None,
            detail: "Keenable search response missing results array".to_string(),
        })?;

    let mut hits = Vec::new();
    for item in results {
        let Some(url) = non_empty(item.get("url").and_then(|v| v.as_str())) else {
            continue;
        };
        let content = non_empty(item.get("snippet").and_then(|v| v.as_str()))
            .or_else(|| non_empty(item.get("description").and_then(|v| v.as_str())));
        hits.push(SearchHit {
            url,
            title: non_empty(item.get("title").and_then(|v| v.as_str())),
            published: None,
            content,
        });
    }
    Ok(hits)
}

fn keenable_relayed_origin_error(body: &str) -> Option<String> {
    let data: Value = serde_json::from_str(body).ok()?;
    let msg = data.get("message").and_then(|v| v.as_str())?.trim();
    let lower = msg.to_ascii_lowercase();
    if lower.starts_with("the target server") || lower.starts_with("the page does not exist") {
        Some(msg.to_string())
    } else {
        None
    }
}

async fn extract_keenable(
    client: &reqwest::Client,
    base_url: &str,
    url: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ExtractedPage, ProviderError> {
    let endpoint = format!("{}/v1/fetch/public", base_url.trim_end_matches('/'));
    let req = client
        .get(&endpoint)
        .query(&[("url", url)])
        .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-Keenable-Title", "mehh-code");

    let (status, retry_after, body) = send_rest_json(req, timeout, cancel).await?;
    if status >= 400 {
        if let Some(origin_msg) = keenable_relayed_origin_error(&body) {
            return Err(ProviderError::OriginBlocked {
                status,
                detail: origin_msg,
            });
        }
        return Err(classify_http_status(
            status,
            retry_after.as_deref(),
            &snippet_for_error(&body),
        ));
    }

    let parsed = parse_json_value(&body, "Keenable extract")?;
    let content = non_empty(parsed.get("content").and_then(|v| v.as_str())).ok_or_else(|| {
        ProviderError::VendorUnavailable {
            status: None,
            detail: "Keenable extract returned empty content".to_string(),
        }
    })?;
    Ok(ExtractedPage {
        url: url.to_string(),
        final_url: non_empty(parsed.get("url").and_then(|v| v.as_str())),
        title: non_empty(parsed.get("title").and_then(|v| v.as_str())),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct RecordedReq {
        request_line: String,
        headers_lower: String,
        body: String,
    }

    async fn spawn_mock_sequence(
        responses: Vec<(u16, &'static str, String)>,
    ) -> (String, Arc<Mutex<Vec<RecordedReq>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec_clone = recorded.clone();

        tokio::spawn(async move {
            for (status, content_type, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let raw_req = read_http_request(&mut sock).await;
                let (head, req_body) = raw_req.split_once("\r\n\r\n").unwrap_or((&raw_req, ""));
                let request_line = head.lines().next().unwrap_or("").to_string();
                rec_clone.lock().unwrap().push(RecordedReq {
                    request_line,
                    headers_lower: head.to_ascii_lowercase(),
                    body: req_body.to_string(),
                });
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        (format!("http://{addr}"), recorded)
    }

    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut expected_total: Option<usize> = None;
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if expected_total.is_none() {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]);
                    let content_len = head
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected_total = Some(pos + 4 + content_len);
                }
            }
            if expected_total.is_some_and(|total| buf.len() >= total) {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn test_exa_search_and_extract() {
        let search_text = "Title: Reqwest Docs\nURL: https://docs.rs/reqwest\nPublished: 2026-01-01\nHighlights:\nHTTP client for Rust.\n\n---\n\nTitle: Empty\nURL: https://example.com\nPublished: N/A\nText:\nFallback body.";
        let sse_search = format!(
            "data: {}\r\n\r\n",
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "content": [{ "type": "text", "text": search_text }] }
            })
        );
        let crawl_429 = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "isError": true, "content": [{ "type": "text", "text": "Error fetching: CRAWL_HTTP_429" }] }
        })
        .to_string();
        let crawl_404 = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "isError": true, "content": [{ "type": "text", "text": "Error: CRAWL_NOT_FOUND" }] }
        })
        .to_string();

        let (base, recorded) = spawn_mock_sequence(vec![
            (200, "text/event-stream", sse_search),
            (200, "application/json", crawl_429),
            (200, "application/json", crawl_404),
        ])
        .await;

        let mut endpoints = WebEndpoints::production();
        endpoints.exa_mcp = format!("{base}/mcp");
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();

        let hits = execute_search(
            &client,
            &endpoints,
            "sess",
            ProviderId::Exa,
            "rust reqwest",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title.as_deref(), Some("Reqwest Docs"));
        assert_eq!(hits[0].published.as_deref(), Some("2026-01-01"));
        assert_eq!(hits[0].content.as_deref(), Some("HTTP client for Rust."));
        assert_eq!(hits[1].published, None);

        let err1 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Exa,
            "https://docs.rs/reqwest",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err1, ProviderError::OriginBlocked { status: 429, .. }));

        let err2 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Exa,
            "https://docs.rs/missing",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err2, ProviderError::OriginBlocked { status: 404, .. }));

        let req0 = &recorded.lock().unwrap()[0].body;
        assert!(req0.contains("web_search_exa"));
        assert!(req0.contains("\"numResults\":8"));
    }

    #[tokio::test]
    async fn test_parallel_search_and_extract() {
        let search_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "{}" }],
                "structuredContent": {
                    "search_id": "s1",
                    "session_id": "sess-123",
                    "results": [{
                        "url": "https://parallel.ai/blog",
                        "title": "Parallel",
                        "publish_date": "2026-05-01",
                        "excerpts": ["One", "Two"]
                    }]
                }
            }
        })
        .to_string();
        let extract_ok = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{
                    "type": "text",
                    "text": json!({
                        "results": [{
                            "url": "https://example.com/final",
                            "title": "Page Title",
                            "full_content": "Full body",
                            "excerpts": ["Ignored"]
                        }]
                    }).to_string()
                }]
            }
        })
        .to_string();
        let extract_err = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{
                    "type": "text",
                    "text": json!({
                        "results": [],
                        "errors": [{
                            "url": "https://example.com/blocked",
                            "http_status_code": 403,
                            "error_type": "http_error",
                            "content": "Forbidden"
                        }]
                    }).to_string()
                }]
            }
        })
        .to_string();

        let (base, recorded) = spawn_mock_sequence(vec![
            (200, "application/json", search_body),
            (200, "application/json", extract_ok),
            (200, "application/json", extract_err),
        ])
        .await;

        let mut endpoints = WebEndpoints::production();
        endpoints.parallel_mcp = format!("{base}/mcp");
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();

        let hits = execute_search(
            &client,
            &endpoints,
            "sess-123",
            ProviderId::Parallel,
            "hello",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content.as_deref(), Some("One\n\nTwo"));

        let page = execute_extract(
            &client,
            &endpoints,
            "sess-123",
            ProviderId::Parallel,
            "https://example.com",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(page.content, "Full body");
        assert_eq!(page.final_url.as_deref(), Some("https://example.com/final"));

        let err = execute_extract(
            &client,
            &endpoints,
            "sess-123",
            ProviderId::Parallel,
            "https://example.com/blocked",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::OriginBlocked { status: 403, .. }));

        assert!(recorded.lock().unwrap()[0].body.contains("sess-123"));
    }

    #[tokio::test]
    async fn test_firecrawl_search_mcp_and_extract_rest() {
        let search_mcp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{
                    "type": "text",
                    "text": json!({
                        "success": true,
                        "data": { "web": [{ "url": "https://fc.dev", "title": "Firecrawl", "description": "Fast crawl" }] }
                    }).to_string()
                }]
            }
        })
        .to_string();
        let scrape_ok = json!({
            "success": true,
            "data": {
                "markdown": "# Hello",
                "metadata": {
                    "title": "Hello Page",
                    "url": "https://redirected.example/target",
                    "sourceURL": "https://example.com/start",
                    "statusCode": 200
                }
            }
        })
        .to_string();
        let scrape_blocked = json!({
            "success": true,
            "data": {
                "markdown": "Forbidden",
                "metadata": {
                    "statusCode": 403,
                    "error": "Blocked by origin"
                }
            }
        })
        .to_string();

        let (base, recorded) = spawn_mock_sequence(vec![
            (200, "application/json", search_mcp),
            (200, "application/json", scrape_ok),
            (200, "application/json", scrape_blocked),
        ])
        .await;

        let mut endpoints = WebEndpoints::production();
        endpoints.firecrawl_mcp = format!("{base}/v2/mcp");
        endpoints.firecrawl_scrape = format!("{base}/v2/scrape");
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();

        let hits = execute_search(
            &client,
            &endpoints,
            "sess",
            ProviderId::Firecrawl,
            "firecrawl",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content.as_deref(), Some("Fast crawl"));

        let page = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Firecrawl,
            "https://example.com/start",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            page.final_url.as_deref(),
            Some("https://redirected.example/target")
        );
        assert_eq!(page.content, "# Hello");

        let err = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Firecrawl,
            "https://example.com/403",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProviderError::OriginBlocked { status: 403, .. }));

        assert!(!recorded.lock().unwrap()[1].headers_lower.contains("authorization:"));
    }

    #[tokio::test]
    async fn test_tavily_and_tinyfish_keyless_search() {
        let tavily_body = json!({
            "results": [{
                "title": "Tavily Hit",
                "url": "https://tavily.com",
                "content": "AI search"
            }]
        })
        .to_string();
        let tinyfish_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{
                    "type": "text",
                    "text": json!({
                        "results": [{
                            "url": "https://tinyfish.ai",
                            "title": "TinyFish",
                            "snippet": "Fast snippet"
                        }]
                    }).to_string()
                }]
            }
        })
        .to_string();

        let (base, recorded) = spawn_mock_sequence(vec![
            (200, "application/json", tavily_body),
            (200, "application/json", tinyfish_body),
        ])
        .await;

        let mut endpoints = WebEndpoints::production();
        endpoints.tavily_search = format!("{base}/search");
        endpoints.tinyfish_mcp = format!("{base}/mcp");
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();

        let t_hits = execute_search(
            &client,
            &endpoints,
            "sess",
            ProviderId::Tavily,
            "tavily",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(t_hits[0].title.as_deref(), Some("Tavily Hit"));

        let f_hits = execute_search(
            &client,
            &endpoints,
            "sess",
            ProviderId::TinyFish,
            "tinyfish",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(f_hits[0].content.as_deref(), Some("Fast snippet"));

        let reqs = recorded.lock().unwrap();
        assert!(reqs[0]
            .headers_lower
            .contains("x-tavily-access-mode: keyless"));
        assert!(reqs[0].headers_lower.contains("x-client-name: mehh-code"));
        assert!(reqs[1]
            .headers_lower
            .contains("x-tinyfish-access-mode: keyless"));
        assert!(reqs[1].body.contains("\"name\":\"search\""));
    }

    #[tokio::test]
    async fn test_keenable_search_and_extract() {
        let search_body = json!({
            "results": [{
                "url": "https://keenable.ai",
                "title": "Keenable",
                "snippet": "Independent index"
            }]
        })
        .to_string();
        let fetch_ok = json!({
            "url": "https://example.com/ok",
            "title": "OK Page",
            "content": "Clean markdown"
        })
        .to_string();
        let origin_403 = json!({"message": "The target server returned HTTP 403"}).to_string();
        let origin_404 = json!({"message": "The page does not exist"}).to_string();
        let vendor_429 = json!({"message": "Rate limit exceeded"}).to_string();
        let vendor_403 = json!({"message": "Anonymous quota exhausted"}).to_string();

        let (base, recorded) = spawn_mock_sequence(vec![
            (200, "application/json", search_body),
            (200, "application/json", fetch_ok),
            (403, "application/json", origin_403),
            (404, "application/json", origin_404),
            (429, "application/json", vendor_429),
            (403, "application/json", vendor_403),
        ])
        .await;

        let mut endpoints = WebEndpoints::production();
        endpoints.keenable_base = base;
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();

        let hits = execute_search(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "keenable",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(hits[0].content.as_deref(), Some("Independent index"));

        let page = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "https://example.com/ok",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(page.content, "Clean markdown");

        let e_origin_403 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "https://example.com/403",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            e_origin_403,
            ProviderError::OriginBlocked { status: 403, .. }
        ));

        let e_origin_404 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "https://example.com/404",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            e_origin_404,
            ProviderError::OriginBlocked { status: 404, .. }
        ));

        let e_vendor_429 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "https://example.com/rl",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(e_vendor_429, ProviderError::RateLimited { .. }));

        let e_vendor_403 = execute_extract(
            &client,
            &endpoints,
            "sess",
            ProviderId::Keenable,
            "https://example.com/quota",
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            e_vendor_403,
            ProviderError::VendorUnavailable { status: Some(403), .. }
        ));

        let reqs = recorded.lock().unwrap();
        assert!(reqs[0]
            .headers_lower
            .contains("x-keenable-title: mehh-code"));
        assert!(reqs[1]
            .request_line
            .starts_with("GET /v1/fetch/public?url="));
    }
}
