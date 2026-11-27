//! Multi-source async adapters for `MehhNative` (DuckDuckGo, Bing, Wikipedia, GitHub, crates.io, npm, StackOverflow).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::web::html_markdown::{decode_entities, html_to_text};
use crate::web::mcp_rpc::{
    parse_retry_after, read_bounded_body, read_error_body_snippet, send_cancellable_request,
    MAX_PROVIDER_ERROR_BODY_BYTES, MAX_PROVIDER_RESPONSE_BYTES,
};
use crate::web::native::html_parse::{looks_like_captcha, parse_bing_html, parse_ddg_html};
use crate::web::native::rank::{RawHit, SourceId};
use crate::web::types::ProviderError;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const API_UA: &str = "MehhCode/0.1 (https://github.com/karmavn/mehhcode)";
const HTML_OFFLOAD_THRESHOLD_BYTES: usize = 16 * 1024;
const BLOCKED_SOURCE_COOLDOWN: Duration = Duration::from_secs(120);
const TRANSIENT_SOURCE_COOLDOWN: Duration = Duration::from_secs(30);

/// Configurable endpoints for native search sources (overrideable in offline tests).
#[derive(Debug, Clone)]
pub struct NativeEndpoints {
    pub ddg_html: String,
    pub bing_html: String,
    pub wikipedia_api: String,
    pub github_api: String,
    pub crates_api: String,
    pub npm_api: String,
    pub stackoverflow_api: String,
}

impl NativeEndpoints {
    pub fn production() -> Self {
        Self {
            ddg_html: "https://html.duckduckgo.com/html/".to_string(),
            bing_html: "https://www.bing.com/search".to_string(),
            wikipedia_api: "https://en.wikipedia.org/w/api.php".to_string(),
            github_api: "https://api.github.com/search/repositories".to_string(),
            crates_api: "https://crates.io/api/v1/crates".to_string(),
            npm_api: "https://registry.npmjs.org/-/v1/search".to_string(),
            stackoverflow_api: "https://api.stackexchange.com/2.3/search/advanced".to_string(),
        }
    }

    pub(crate) fn endpoint_for(&self, source: SourceId) -> &str {
        match source {
            SourceId::DuckDuckGo => &self.ddg_html,
            SourceId::Bing => &self.bing_html,
            SourceId::Wikipedia => &self.wikipedia_api,
            SourceId::GitHub => &self.github_api,
            SourceId::CratesIo => &self.crates_api,
            SourceId::Npm => &self.npm_api,
            SourceId::StackOverflow => &self.stackoverflow_api,
        }
    }
}

impl Default for NativeEndpoints {
    fn default() -> Self {
        Self::production()
    }
}

#[derive(Debug)]
pub(crate) enum SourceOutcome {
    Hits(Vec<RawHit>),
    Failed {
        reason: String,
        cooldown: Option<Duration>,
    },
    Cancelled,
}

#[cfg(test)]
pub(crate) async fn fetch_source(
    client: &reqwest::Client,
    endpoints: &NativeEndpoints,
    source: SourceId,
    query: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> SourceOutcome {
    fetch_source_url(
        client,
        endpoints.endpoint_for(source),
        source,
        query,
        deadline,
        cancel,
    )
    .await
}

pub(crate) async fn fetch_source_url(
    client: &reqwest::Client,
    endpoint: &str,
    source: SourceId,
    query: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> SourceOutcome {
    let req = match source {
        SourceId::DuckDuckGo => client
            .get(endpoint)
            .query(&[("q", query), ("kl", "wt-wt")])
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9"),
        SourceId::Bing => client
            .get(endpoint)
            .query(&[("q", query), ("setlang", "en"), ("count", "10")])
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9"),
        SourceId::Wikipedia => client
            .get(endpoint)
            .query(&[
                ("action", "query"),
                ("list", "search"),
                ("srsearch", query),
                ("format", "json"),
                ("utf8", "1"),
                ("srlimit", "5"),
            ])
            .header(reqwest::header::USER_AGENT, API_UA)
            .header(reqwest::header::ACCEPT, "application/json"),
        SourceId::GitHub => client
            .get(endpoint)
            .query(&[
                ("q", query),
                ("sort", "stars"),
                ("order", "desc"),
                ("per_page", "6"),
            ])
            .header(reqwest::header::USER_AGENT, API_UA)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"),
        SourceId::CratesIo => client
            .get(endpoint)
            .query(&[("q", query), ("per_page", "6"), ("sort", "relevance")])
            .header(reqwest::header::USER_AGENT, API_UA)
            .header(reqwest::header::ACCEPT, "application/json"),
        SourceId::Npm => client
            .get(endpoint)
            .query(&[("text", query), ("size", "6")])
            .header(reqwest::header::USER_AGENT, API_UA)
            .header(reqwest::header::ACCEPT, "application/json"),
        SourceId::StackOverflow => client
            .get(endpoint)
            .query(&[
                ("q", query),
                ("site", "stackoverflow"),
                ("sort", "relevance"),
                ("order", "desc"),
                ("pagesize", "6"),
            ])
            .header(reqwest::header::USER_AGENT, API_UA)
            .header(reqwest::header::ACCEPT, "application/json"),
    };

    let resp = match send_cancellable_request(req, deadline, cancel).await {
        Ok(r) => r,
        Err(ProviderError::Cancelled) => return SourceOutcome::Cancelled,
        Err(err) => {
            return SourceOutcome::Failed {
                reason: format!("{}: {err}", source.label()),
                cooldown: Some(TRANSIENT_SOURCE_COOLDOWN),
            };
        }
    };

    let status = resp.status().as_u16();
    if status >= 400 {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|hdr| parse_retry_after(Some(hdr), SystemTime::now()));
        let snippet =
            match read_error_body_snippet(resp, MAX_PROVIDER_ERROR_BODY_BYTES, deadline, cancel).await
            {
                Ok(s) => s,
                Err(ProviderError::Cancelled) => return SourceOutcome::Cancelled,
                Err(_) => String::new(),
            };
        let detail = if snippet.trim().is_empty() {
            format!("{} returned HTTP {status}", source.label())
        } else {
            format!("{} returned HTTP {status}: {}", source.label(), snippet.trim())
        };
        let cooldown = if status == 429 {
            Some(retry_after.unwrap_or(BLOCKED_SOURCE_COOLDOWN))
        } else if matches!(status, 401..=403) {
            Some(BLOCKED_SOURCE_COOLDOWN)
        } else {
            Some(TRANSIENT_SOURCE_COOLDOWN)
        };
        return SourceOutcome::Failed {
            reason: detail,
            cooldown,
        };
    }

    let bytes = match read_bounded_body(resp, MAX_PROVIDER_RESPONSE_BYTES, deadline, cancel).await {
        Ok(b) => b,
        Err(ProviderError::Cancelled) => return SourceOutcome::Cancelled,
        Err(err) => {
            return SourceOutcome::Failed {
                reason: format!("{}: {err}", source.label()),
                cooldown: Some(TRANSIENT_SOURCE_COOLDOWN),
            };
        }
    };

    if matches!(source, SourceId::DuckDuckGo | SourceId::Bing) {
        let len = bytes.len();
        let parse_html = move || {
            let text = String::from_utf8_lossy(&bytes);
            if looks_like_captcha(&text) {
                let msg = if source == SourceId::DuckDuckGo {
                    "duckduckgo served CAPTCHA"
                } else {
                    "bing served CAPTCHA"
                };
                return SourceOutcome::Failed {
                    reason: msg.to_string(),
                    cooldown: Some(BLOCKED_SOURCE_COOLDOWN),
                };
            }
            let hits = match source {
                SourceId::DuckDuckGo => parse_ddg_html(&text),
                SourceId::Bing => parse_bing_html(&text),
                _ => Vec::new(),
            };
            SourceOutcome::Hits(hits)
        };
        if len > HTML_OFFLOAD_THRESHOLD_BYTES {
            return match tokio::task::spawn_blocking(parse_html).await {
                Ok(outcome) => outcome,
                Err(err) => SourceOutcome::Failed {
                    reason: format!("{} HTML parse task failed: {err}", source.label()),
                    cooldown: None,
                },
            };
        }
        return parse_html();
    }

    let text = String::from_utf8_lossy(&bytes);
    match source {
        SourceId::Wikipedia => parse_wikipedia_json(&text),
        SourceId::GitHub => parse_github_json(&text),
        SourceId::CratesIo => parse_crates_json(&text),
        SourceId::Npm => parse_npm_json(&text),
        SourceId::StackOverflow => parse_stackoverflow_json(&text),
        SourceId::DuckDuckGo | SourceId::Bing => unreachable!(),
    }
}

fn non_empty_str(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_typed<'a, T: Deserialize<'a>>(raw: &'a str, source: SourceId) -> Result<T, SourceOutcome> {
    serde_json::from_str(raw).map_err(|err| SourceOutcome::Failed {
        reason: format!("{} invalid JSON: {err}", source.label()),
        cooldown: Some(TRANSIENT_SOURCE_COOLDOWN),
    })
}

#[derive(Deserialize)]
struct WikiResp<'a> {
    #[serde(default, borrow)]
    query: Option<WikiQuery<'a>>,
}

#[derive(Deserialize)]
struct WikiQuery<'a> {
    #[serde(default, borrow)]
    search: Vec<WikiItem<'a>>,
}

#[derive(Deserialize)]
struct WikiItem<'a> {
    #[serde(default, borrow)]
    title: Option<Cow<'a, str>>,
    #[serde(default)]
    pageid: Option<u64>,
    #[serde(default, borrow)]
    snippet: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
}

fn parse_wikipedia_json(raw: &str) -> SourceOutcome {
    let data: WikiResp<'_> = match parse_typed(raw, SourceId::Wikipedia) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let Some(query) = data.query else {
        return SourceOutcome::Hits(Vec::new());
    };

    let mut hits = Vec::with_capacity(query.search.len());
    for (idx, item) in query.search.iter().enumerate() {
        let Some(title) = non_empty_str(item.title.as_deref()) else {
            continue;
        };
        let url = if let Some(pageid) = item.pageid {
            format!("https://en.wikipedia.org/?curid={pageid}")
        } else {
            format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_"))
        };
        let snippet = item
            .snippet
            .as_deref()
            .map(html_to_text)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let published = non_empty_str(item.timestamp.as_deref());
        hits.push(RawHit {
            source: SourceId::Wikipedia,
            rank: idx + 1,
            url,
            title: Some(title),
            snippet,
            published,
        });
    }
    SourceOutcome::Hits(hits)
}

#[derive(Deserialize)]
struct GitHubResp<'a> {
    #[serde(default, borrow)]
    items: Vec<GitHubItem<'a>>,
}

#[derive(Deserialize)]
struct GitHubItem<'a> {
    #[serde(default, borrow)]
    html_url: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    full_name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    description: Option<Cow<'a, str>>,
    #[serde(default)]
    stargazers_count: Option<u64>,
    #[serde(default, borrow)]
    language: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    updated_at: Option<Cow<'a, str>>,
}

fn parse_github_json(raw: &str) -> SourceOutcome {
    let data: GitHubResp<'_> = match parse_typed(raw, SourceId::GitHub) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let mut hits = Vec::with_capacity(data.items.len());
    for (idx, item) in data.items.iter().enumerate() {
        let Some(url) = non_empty_str(item.html_url.as_deref()) else {
            continue;
        };
        let title = non_empty_str(item.full_name.as_deref());
        let desc = item.description.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let lang = item.language.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let mut snippet = String::with_capacity(desc.map_or(0, str::len) + 32);
        if let Some(d) = desc {
            snippet.push_str(d);
        }
        if let Some(s) = item.stargazers_count {
            if !snippet.is_empty() {
                snippet.push_str(" | ");
            }
            let _ = write!(snippet, "{s} stars");
        }
        if let Some(l) = lang {
            if !snippet.is_empty() {
                snippet.push_str(" | ");
            }
            snippet.push_str(l);
        }

        hits.push(RawHit {
            source: SourceId::GitHub,
            rank: idx + 1,
            url,
            title,
            snippet: (!snippet.is_empty()).then_some(snippet),
            published: non_empty_str(item.updated_at.as_deref()),
        });
    }
    SourceOutcome::Hits(hits)
}

#[derive(Deserialize)]
struct CratesResp<'a> {
    #[serde(default, borrow)]
    crates: Vec<CrateEntry<'a>>,
}

#[derive(Deserialize)]
struct CrateEntry<'a> {
    #[serde(default, borrow, rename = "crate")]
    inner: Option<CrateFields<'a>>,
    #[serde(flatten)]
    flat: CrateFields<'a>,
}

#[derive(Deserialize, Default)]
struct CrateFields<'a> {
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    description: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    newest_version: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    max_version: Option<Cow<'a, str>>,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default, borrow)]
    updated_at: Option<Cow<'a, str>>,
}

fn parse_crates_json(raw: &str) -> SourceOutcome {
    let data: CratesResp<'_> = match parse_typed(raw, SourceId::CratesIo) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let mut hits = Vec::with_capacity(data.crates.len());
    for (idx, entry) in data.crates.iter().enumerate() {
        let info = entry.inner.as_ref().unwrap_or(&entry.flat);
        let Some(name) = non_empty_str(info.name.as_deref()) else {
            continue;
        };
        let url = format!("https://crates.io/crates/{name}");
        let desc = info.description.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let ver = info
            .newest_version
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                info.max_version
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            });

        let mut snippet = String::with_capacity(desc.map_or(0, str::len) + 32);
        if let Some(d) = desc {
            snippet.push_str(d);
        }
        if let Some(v) = ver {
            if !snippet.is_empty() {
                snippet.push_str(" | ");
            }
            let _ = write!(snippet, "v{v}");
        }
        if let Some(dl) = info.downloads {
            if !snippet.is_empty() {
                snippet.push_str(" | ");
            }
            let _ = write!(snippet, "{dl} downloads");
        }
        let title = match ver {
            Some(v) => Some(format!("{name} v{v} - crates.io")),
            None => Some(format!("{name} - crates.io")),
        };

        hits.push(RawHit {
            source: SourceId::CratesIo,
            rank: idx + 1,
            url,
            title,
            snippet: (!snippet.is_empty()).then_some(snippet),
            published: non_empty_str(info.updated_at.as_deref()),
        });
    }
    SourceOutcome::Hits(hits)
}

#[derive(Deserialize)]
struct NpmResp<'a> {
    #[serde(default, borrow)]
    objects: Vec<NpmObj<'a>>,
}

#[derive(Deserialize)]
struct NpmObj<'a> {
    #[serde(default, borrow)]
    package: Option<NpmPkg<'a>>,
}

#[derive(Deserialize)]
struct NpmPkg<'a> {
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    description: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    version: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    date: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    links: Option<NpmLinks<'a>>,
}

#[derive(Deserialize)]
struct NpmLinks<'a> {
    #[serde(default, borrow)]
    npm: Option<Cow<'a, str>>,
}

fn parse_npm_json(raw: &str) -> SourceOutcome {
    let data: NpmResp<'_> = match parse_typed(raw, SourceId::Npm) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let mut hits = Vec::with_capacity(data.objects.len());
    for (idx, obj) in data.objects.iter().enumerate() {
        let Some(pkg) = obj.package.as_ref() else {
            continue;
        };
        let Some(name) = non_empty_str(pkg.name.as_deref()) else {
            continue;
        };
        let url = pkg
            .links
            .as_ref()
            .and_then(|l| non_empty_str(l.npm.as_deref()))
            .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}"));
        let desc = pkg.description.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let ver = pkg.version.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let mut snippet = String::with_capacity(desc.map_or(0, str::len) + 20);
        if let Some(d) = desc {
            snippet.push_str(d);
        }
        if let Some(v) = ver {
            if !snippet.is_empty() {
                snippet.push_str(" | ");
            }
            let _ = write!(snippet, "v{v}");
        }
        let title = match ver {
            Some(v) => Some(format!("{name}@{v} - npm")),
            None => Some(format!("{name} - npm")),
        };

        hits.push(RawHit {
            source: SourceId::Npm,
            rank: idx + 1,
            url,
            title,
            snippet: (!snippet.is_empty()).then_some(snippet),
            published: non_empty_str(pkg.date.as_deref()),
        });
    }
    SourceOutcome::Hits(hits)
}

#[derive(Deserialize)]
struct StackOverflowResp<'a> {
    #[serde(default, borrow)]
    items: Vec<StackOverflowItem<'a>>,
}

#[derive(Deserialize)]
struct StackOverflowItem<'a> {
    #[serde(default, borrow)]
    link: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    title: Option<Cow<'a, str>>,
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    answer_count: Option<u64>,
    #[serde(default)]
    is_answered: Option<bool>,
    #[serde(default, borrow)]
    tags: Vec<Cow<'a, str>>,
}

fn parse_stackoverflow_json(raw: &str) -> SourceOutcome {
    let data: StackOverflowResp<'_> = match parse_typed(raw, SourceId::StackOverflow) {
        Ok(v) => v,
        Err(err) => return err,
    };

    let mut hits = Vec::with_capacity(data.items.len());
    for (idx, item) in data.items.iter().enumerate() {
        let Some(url) = non_empty_str(item.link.as_deref()) else {
            continue;
        };
        let title = item
            .title
            .as_deref()
            .map(|t| decode_entities(t).into_owned())
            .filter(|s| !s.trim().is_empty());

        let score = item.score.unwrap_or(0);
        let answers = item.answer_count.unwrap_or(0);
        let accepted = item.is_answered.unwrap_or(false);

        let mut snippet = String::with_capacity(96);
        let _ = write!(snippet, "Score: {score} | Answers: {answers}");
        if accepted {
            snippet.push_str(" (answered)");
        }
        let mut first_tag = true;
        for tag in item.tags.iter().take(5) {
            if first_tag {
                snippet.push_str(" | Tags: ");
                first_tag = false;
            } else {
                snippet.push_str(", ");
            }
            snippet.push_str(tag.as_ref());
        }

        hits.push(RawHit {
            source: SourceId::StackOverflow,
            rank: idx + 1,
            url,
            title,
            snippet: Some(snippet),
            published: None,
        });
    }
    SourceOutcome::Hits(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn gzip_stored(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u16;
        let nlen = !len;
        let mut out = Vec::with_capacity(payload.len() + 23);
        out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x02, 0xff]);
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&crc32(payload).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out
    }

    async fn serve_once(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hdrs: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req_buf = [0u8; 4096];
            let _ = sock.read(&mut req_buf).await;
            let mut head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (k, v) in hdrs {
                let _ = write!(head, "{k}: {v}\r\n");
            }
            head.push_str("\r\n");
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_fetch_source_all_adapters_including_gzip_and_failures() {
        let client = reqwest::Client::builder()
            .gzip(true)
            .deflate(true)
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut endpoints = NativeEndpoints::production();

        // 1. Crates.io JSON
        let crates_body = serde_json::json!({
            "crates": [{
                "name": "serde",
                "description": "Serialization framework",
                "newest_version": "1.0.220",
                "downloads": 500000,
                "updated_at": "2026-10-01"
            }]
        })
        .to_string()
        .into_bytes();
        endpoints.crates_api =
            serve_once(200, &[("Content-Type", "application/json")], crates_body).await;

        let crates_res =
            fetch_source(&client, &endpoints, SourceId::CratesIo, "serde", deadline, &cancel)
                .await;
        match crates_res {
            SourceOutcome::Hits(hits) => {
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].url, "https://crates.io/crates/serde");
                assert!(hits[0].title.as_deref().unwrap().contains("1.0.220"));
            }
            other => panic!("expected crates hits, got {other:?}"),
        }

        // 2. Npm JSON
        let npm_body = serde_json::json!({
            "objects": [{
                "package": {
                    "name": "vite",
                    "version": "6.0.0",
                    "description": "Next generation frontend tooling",
                    "links": { "npm": "https://www.npmjs.com/package/vite" }
                }
            }]
        })
        .to_string()
        .into_bytes();
        endpoints.npm_api =
            serve_once(200, &[("Content-Type", "application/json")], npm_body).await;
        match fetch_source(&client, &endpoints, SourceId::Npm, "vite", deadline, &cancel).await {
            SourceOutcome::Hits(hits) => {
                assert_eq!(hits[0].url, "https://www.npmjs.com/package/vite");
            }
            other => panic!("expected npm hits, got {other:?}"),
        }

        // 3. GitHub JSON
        let gh_body = serde_json::json!({
            "items": [{
                "html_url": "https://github.com/tokio-rs/axum",
                "full_name": "tokio-rs/axum",
                "description": "Ergonomic web framework",
                "stargazers_count": 20000,
                "language": "Rust"
            }]
        })
        .to_string()
        .into_bytes();
        endpoints.github_api =
            serve_once(200, &[("Content-Type", "application/json")], gh_body).await;
        match fetch_source(&client, &endpoints, SourceId::GitHub, "axum", deadline, &cancel).await {
            SourceOutcome::Hits(hits) => {
                assert_eq!(hits[0].url, "https://github.com/tokio-rs/axum");
                assert!(hits[0].snippet.as_deref().unwrap().contains("20000 stars"));
            }
            other => panic!("expected github hits, got {other:?}"),
        }

        // 4. Wikipedia JSON (strips searchmatch spans)
        let wiki_body = serde_json::json!({
            "query": {
                "search": [{
                    "pageid": 42,
                    "title": "Rust (programming language)",
                    "snippet": "<span class=\"searchmatch\">Rust</span> is a systems language."
                }]
            }
        })
        .to_string()
        .into_bytes();
        endpoints.wikipedia_api =
            serve_once(200, &[("Content-Type", "application/json")], wiki_body).await;
        match fetch_source(&client, &endpoints, SourceId::Wikipedia, "rust", deadline, &cancel)
            .await
        {
            SourceOutcome::Hits(hits) => {
                assert_eq!(hits[0].url, "https://en.wikipedia.org/?curid=42");
                assert_eq!(
                    hits[0].snippet.as_deref(),
                    Some("Rust is a systems language.")
                );
            }
            other => panic!("expected wikipedia hits, got {other:?}"),
        }

        // 5. StackOverflow real gzip-compressed JSON
        let so_json = serde_json::json!({
            "items": [{
                "link": "https://stackoverflow.com/questions/123",
                "title": "How to fix &quot;cannot borrow&quot; in Rust?",
                "score": 99,
                "answer_count": 4,
                "is_answered": true,
                "tags": ["rust", "borrow-checker"]
            }]
        })
        .to_string();
        endpoints.stackoverflow_api = serve_once(
            200,
            &[
                ("Content-Type", "application/json"),
                ("Content-Encoding", "gzip"),
            ],
            gzip_stored(so_json.as_bytes()),
        )
        .await;
        match fetch_source(
            &client,
            &endpoints,
            SourceId::StackOverflow,
            "cannot borrow",
            deadline,
            &cancel,
        )
        .await
        {
            SourceOutcome::Hits(hits) => {
                assert_eq!(
                    hits[0].title.as_deref(),
                    Some("How to fix \"cannot borrow\" in Rust?")
                );
                assert!(hits[0].snippet.as_deref().unwrap().contains("Score: 99"));
            }
            other => panic!("expected stackoverflow gzipped hits, got {other:?}"),
        }

        // 6. DuckDuckGo CAPTCHA & Bing HTTP 503 return Failed
        endpoints.ddg_html = serve_once(
            200,
            &[("Content-Type", "text/html")],
            b"<div class=\"anomaly-modal\">Please verify</div>".to_vec(),
        )
        .await;
        assert!(matches!(
            fetch_source(&client, &endpoints, SourceId::DuckDuckGo, "q", deadline, &cancel).await,
            SourceOutcome::Failed { .. }
        ));

        endpoints.bing_html = serve_once(503, &[], b"busy".to_vec()).await;
        assert!(matches!(
            fetch_source(&client, &endpoints, SourceId::Bing, "q", deadline, &cancel).await,
            SourceOutcome::Failed { .. }
        ));
    }
}
