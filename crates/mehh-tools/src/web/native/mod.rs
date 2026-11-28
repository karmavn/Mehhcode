//! MehhNative: Mehhcode's in-process metasearch engine and bounded Auto-Enrich pipeline.

pub mod enrich;
pub mod html_parse;
pub mod rank;
pub mod sources;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use mehh_ai::http_safety::DnsPolicy;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use sources::NativeEndpoints;

use crate::web::types::{ProviderError, SearchHit};
use crate::web::UrlPolicyChecker;
use enrich::enrich_top_hits;
use rank::{canonicalize_url, classify_query, fuse_and_rank, RawHit, SourceId};
use sources::{fetch_source_url, SourceOutcome};

const STAGE1_BUDGET: Duration = Duration::from_millis(3500);
const STAGE1_STRAGGLER_GRACE: Duration = Duration::from_millis(700);
const STAGE1_QUORUM_MIN_HITS: usize = 6;
const MIN_REMAINING_FOR_ENRICH: Duration = Duration::from_millis(600);
const MAX_RETURNED_HITS: usize = 10;

const ENRICH_CACHE_MAX_ENTRIES: usize = 64;
const ENRICH_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const ENRICH_CACHE_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct CachedEnrichPage {
    pub checked_urls: Arc<[String]>,
    pub markdown: Arc<str>,
    pub inserted_at: StdInstant,
    pub last_used: StdInstant,
}

#[derive(Default)]
struct EnrichPageCache {
    entries: HashMap<String, CachedEnrichPage>,
    total_bytes: usize,
}

impl EnrichPageCache {
    fn prune_expired(&mut self, now: StdInstant) {
        let mut removed_bytes = 0usize;
        self.entries.retain(|_, entry| {
            let keep = now.duration_since(entry.inserted_at) < ENRICH_CACHE_TTL;
            if !keep {
                removed_bytes += entry.markdown.len();
            }
            keep
        });
        self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
    }

    fn get(&mut self, raw_url: &str) -> Option<(Arc<[String]>, Arc<str>)> {
        let key = canonicalize_url(raw_url)?;
        let now = StdInstant::now();
        if let Some(entry) = self.entries.get_mut(&key) {
            if now.duration_since(entry.inserted_at) < ENRICH_CACHE_TTL {
                entry.last_used = now;
                return Some((entry.checked_urls.clone(), entry.markdown.clone()));
            }
        }
        if let Some(expired) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(expired.markdown.len());
        }
        None
    }

    fn insert(&mut self, raw_url: &str, checked_urls: Arc<[String]>, markdown: Arc<str>) {
        if markdown.len() > ENRICH_CACHE_MAX_TOTAL_BYTES {
            return;
        }
        let Some(key) = canonicalize_url(raw_url) else {
            return;
        };
        let now = StdInstant::now();
        self.prune_expired(now);
        if let Some(prev) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.markdown.len());
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= ENRICH_CACHE_MAX_ENTRIES
                || self.total_bytes + markdown.len() > ENRICH_CACHE_MAX_TOTAL_BYTES)
        {
            let Some(lru_key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&lru_key) {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.markdown.len());
            }
        }
        self.total_bytes += markdown.len();
        self.entries.insert(
            key,
            CachedEnrichPage {
                checked_urls,
                markdown,
                inserted_at: now,
                last_used: now,
            },
        );
    }
}

#[derive(Default)]
struct NativeSearchStateInner {
    source_cooldowns: Mutex<HashMap<SourceId, StdInstant>>,
    page_cache: Mutex<EnrichPageCache>,
}

/// Shared mutable runtime state for `MehhNative` across queries (per-source cooldowns + page cache).
#[derive(Clone, Default)]
pub struct NativeSearchState(Arc<NativeSearchStateInner>);

impl NativeSearchState {
    pub fn new() -> Self {
        Self::default()
    }

    fn filter_sources(&self, mut selected: Vec<SourceId>) -> Vec<SourceId> {
        let now = StdInstant::now();
        let mut guard = mehh_ai::lock_recover(&self.0.source_cooldowns);
        guard.retain(|_, until| *until > now);
        if guard.is_empty() {
            return selected;
        }
        selected.retain(|s| !guard.contains_key(s));
        if !selected.iter().any(|s| s.is_general_web()) {
            // If both general web sources are cooling down, probe the earliest-expiring one
            // so recovery or Router failover still happens deterministically.
            let probe = [SourceId::DuckDuckGo, SourceId::Bing]
                .into_iter()
                .min_by_key(|s| guard.get(s).copied())
                .unwrap_or(SourceId::DuckDuckGo);
            selected.insert(0, probe);
        }
        selected
    }

    fn record_source_success(&self, source: SourceId) {
        let mut guard = mehh_ai::lock_recover(&self.0.source_cooldowns);
        guard.remove(&source);
    }

    fn record_source_cooldown(&self, source: SourceId, dur: Duration) {
        let mut guard = mehh_ai::lock_recover(&self.0.source_cooldowns);
        guard.insert(source, StdInstant::now() + dur);
    }

    pub(crate) fn get_cached_page(&self, raw_url: &str) -> Option<(Arc<[String]>, Arc<str>)> {
        let mut guard = mehh_ai::lock_recover(&self.0.page_cache);
        guard.get(raw_url)
    }

    pub(crate) fn cache_page(&self, raw_url: &str, checked_urls: Arc<[String]>, markdown: Arc<str>) {
        let mut guard = mehh_ai::lock_recover(&self.0.page_cache);
        guard.insert(raw_url, checked_urls, markdown);
    }
}

/// Shared HTTP clients, policy context, and endpoints for `search_mehh`.
pub struct NativeSearchContext<'a> {
    pub client: &'a reqwest::Client,
    pub enrich_client: &'a reqwest::Client,
    pub enrich_dns_policy: DnsPolicy,
    pub url_policy_checker: Option<&'a Arc<dyn UrlPolicyChecker>>,
    pub endpoints: &'a NativeEndpoints,
    pub state: Option<&'a NativeSearchState>,
}

/// Executes a full `MehhNative` search: Stage 1 parallel metasearch + RRF fusion,
/// followed by Stage 2 bounded parallel Auto-Enrich on top hits.
pub async fn search_mehh(
    ctx: &NativeSearchContext<'_>,
    query: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    search_mehh_with_stage1_budget(ctx, query, timeout, STAGE1_BUDGET, cancel).await
}

async fn search_mehh_with_stage1_budget(
    ctx: &NativeSearchContext<'_>,
    query: &str,
    timeout: Duration,
    stage1_budget: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<SearchHit>, ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }

    let now = Instant::now();
    let total_deadline = now + timeout;
    let stage1_deadline = total_deadline.min(now + stage1_budget);
    let mut selected_sources = classify_query(query);
    if let Some(state) = ctx.state {
        selected_sources = state.filter_sources(selected_sources);
    }

    let query_arc: Arc<str> = Arc::from(query);
    let source_dns_policy = ctx.enrich_dns_policy;
    let mut pending_sources: Vec<SourceId> = selected_sources.clone();
    let mut set: JoinSet<(SourceId, SourceOutcome)> = JoinSet::new();
    for source in selected_sources {
        let client = ctx.client.clone();
        let endpoint = ctx.endpoints.endpoint_for(source).to_string();
        let query = query_arc.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            let outcome = fetch_source_url(
                &client,
                &endpoint,
                source,
                &query,
                source_dns_policy,
                stage1_deadline,
                &cancel,
            )
            .await;
            (source, outcome)
        });
    }

    let mut raw_hits: Vec<RawHit> = Vec::new();
    let mut general_ok_count = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut cancelled = false;
    let mut straggler_tightened = false;

    let stage1_sleep = tokio::time::sleep_until(stage1_deadline);
    tokio::pin!(stage1_sleep);

    while let Some(res) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            set.abort_all();
            cancelled = true;
            None
        }
        _ = &mut stage1_sleep => {
            set.abort_all();
            if !straggler_tightened {
                if let Some(state) = ctx.state {
                    for src in &pending_sources {
                        state.record_source_cooldown(*src, sources::TRANSIENT_SOURCE_COOLDOWN);
                    }
                }
                if raw_hits.is_empty() {
                    failures.push("stage 1 metasearch timed out".to_string());
                }
            }
            None
        }
        joined = set.join_next() => joined,
    } {
        if let Ok((source, outcome)) = res {
            pending_sources.retain(|s| *s != source);
            match outcome {
                SourceOutcome::Hits(hits) => {
                    if source.is_general_web() {
                        general_ok_count += 1;
                    }
                    if !hits.is_empty() {
                        if let Some(state) = ctx.state {
                            state.record_source_success(source);
                        }
                    }
                    raw_hits.extend(hits);
                    if !straggler_tightened
                        && !set.is_empty()
                        && general_ok_count >= 1
                        && raw_hits.len() >= STAGE1_QUORUM_MIN_HITS
                    {
                        let grace_deadline =
                            (Instant::now() + STAGE1_STRAGGLER_GRACE).min(stage1_deadline);
                        if grace_deadline < stage1_deadline {
                            stage1_sleep.as_mut().reset(grace_deadline);
                            straggler_tightened = true;
                        }
                    }
                }
                SourceOutcome::Failed { reason, cooldown } => {
                    if let (Some(state), Some(dur)) = (ctx.state, cooldown) {
                        state.record_source_cooldown(source, dur);
                    }
                    failures.push(reason);
                }
                SourceOutcome::Cancelled => {
                    cancelled = true;
                    set.abort_all();
                    break;
                }
            }
        }
    }

    if cancelled || cancel.is_cancelled() {
        return Err(ProviderError::Cancelled);
    }

    let mut hits = fuse_and_rank(raw_hits, MAX_RETURNED_HITS);
    if hits.is_empty() && (general_ok_count == 0 || !failures.is_empty()) {
        let detail = if failures.is_empty() {
            "Mehh Search received no results from web sources".to_string()
        } else {
            format!("Mehh Search sources unavailable: {}", failures.join("; "))
        };
        return Err(ProviderError::VendorUnavailable {
            status: None,
            detail,
        });
    }

    if !hits.is_empty()
        && total_deadline.saturating_duration_since(Instant::now()) >= MIN_REMAINING_FOR_ENRICH
    {
        enrich_top_hits(
            ctx.enrich_client,
            ctx.enrich_dns_policy,
            ctx.url_policy_checker,
            ctx.state,
            query,
            &mut hits,
            total_deadline,
            cancel,
        )
        .await;
    }

    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use mehh_ai::http_safety::build_safe_http_client;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_routes(
        routes: Vec<(&'static str, u16, &'static str, String)>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let routes_clone = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let raw_target = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let path = raw_target.split('?').next().unwrap_or("/");
                    if let Some((_, status, ctype, body)) =
                        routes_clone.into_iter().find(|(p, _, _, _)| *p == path)
                    {
                        let resp = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_search_mehh_fails_over_when_web_sources_captcha_or_fail() {
        let base = serve_routes(vec![
            (
                "/ddg",
                200,
                "text/html",
                "<div class=\"anomaly-modal\">CAPTCHA</div>".to_string(),
            ),
            ("/bing", 503, "text/plain", "Service Unavailable".to_string()),
            (
                "/wiki",
                200,
                "application/json",
                serde_json::json!({"query":{"search":[]}}).to_string(),
            ),
        ])
        .await;

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("{base}/ddg");
        endpoints.bing_html = format!("{base}/bing");
        endpoints.wikipedia_api = format!("{base}/wiki");

        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: None,
        };
        let err = search_mehh(&ctx, "what is photosynthesis", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.should_failover());
    }

    #[tokio::test]
    async fn test_search_mehh_end_to_end_fanout_rrf_and_enrich() {
        let b64_guide =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("PLACEHOLDER_URL/guide");
        let base = serve_routes(vec![
            (
                "/ddg",
                200,
                "text/html",
                "<html><body><div class=\"result web-result\"><a class=\"result__a\" href=\"http://other.invalid/only-ddg\">Only DDG</a><div class=\"result__snippet\">Short ddg</div></div><div class=\"result web-result\"><a class=\"result__a\" href=\"PLACEHOLDER_URL/guide\">Guide</a><div class=\"result__snippet\">Brief snippet</div></div></body></html>".to_string(),
            ),
            (
                "/bing",
                200,
                "text/html",
                format!(
                    "<ol><li class=\"b_algo\"><h2><a href=\"https://www.bing.com/ck/a?!&amp;u=a1{b64_guide}\">Rust Cargo Guide</a></h2><p>Bing snippet</p></li></ol>"
                ),
            ),
            (
                "/crates",
                200,
                "application/json",
                serde_json::json!({
                    "crates": [{
                        "name": "serde",
                        "description": "Serialization framework",
                        "newest_version": "1.0.220",
                        "downloads": 999
                    }]
                })
                .to_string(),
            ),
            (
                "/guide",
                200,
                "text/html",
                "<html><body><h2>Cargo Workspace</h2><p>In Rust cargo workspaces, shared dependencies live under [workspace.dependencies] for consistent builds.</p></body></html>".to_string(),
            ),
        ])
        .await;

        // Re-serve with concrete base URL inside HTML/ck redirect since mock routes contain URLs.
        let guide_url = format!("{base}/guide");
        let only_ddg_url = format!("{base}/only-ddg");
        let b64_real = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&guide_url);
        let base2 = serve_routes(vec![
            (
                "/ddg",
                200,
                "text/html",
                format!(
                    "<html><body><div class=\"result web-result\"><a class=\"result__a\" href=\"{only_ddg_url}\">Only DDG</a><div class=\"result__snippet\">Short ddg</div></div><div class=\"result web-result\"><a class=\"result__a\" href=\"{guide_url}\">Guide</a><div class=\"result__snippet\">Brief</div></div></body></html>"
                ),
            ),
            (
                "/bing",
                200,
                "text/html",
                format!(
                    "<ol><li class=\"b_algo\"><h2><a href=\"https://www.bing.com/ck/a?!&amp;u=a1{b64_real}\">Rust Cargo Guide</a></h2><p>Bing snippet</p></li></ol>"
                ),
            ),
            (
                "/crates",
                200,
                "application/json",
                serde_json::json!({
                    "crates": [{
                        "name": "serde",
                        "description": "Serialization framework",
                        "newest_version": "1.0.220",
                        "downloads": 999
                    }]
                })
                .to_string(),
            ),
        ])
        .await;

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("{base2}/ddg");
        endpoints.bing_html = format!("{base2}/bing");
        endpoints.crates_api = format!("{base2}/crates");

        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: None,
        };
        let hits = search_mehh(&ctx, "rust cargo guide", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();

        assert!(!hits.is_empty());
        // Consensus /guide hit appears in both DDG and Bing so ranks #1 and gets Stage 2 enriched!
        assert_eq!(hits[0].url, guide_url);
        assert!(hits[0]
            .content
            .as_deref()
            .unwrap()
            .contains("[workspace.dependencies]"));
        assert!(hits.iter().any(|h| h.url == "https://crates.io/crates/serde"));
    }

    #[tokio::test]
    async fn test_stage1_straggler_cutoff_bounds_slow_source_wait() {
        use std::fmt::Write as _;

        let mut ddg_html = String::from("<html><body>");
        for i in 0..6 {
            let _ = write!(
                ddg_html,
                "<div class=\"result web-result\"><a class=\"result__a\" href=\"https://docs.rs/p{i}\">Result {i}</a><div class=\"result__snippet\">Long enough snippet {i} for avoiding enrich wait in straggler test and keeping stage 1 isolated {pad}</div></div>",
                pad = "x".repeat(480)
            );
        }
        ddg_html.push_str("</body></html>");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let ddg_clone = ddg_html.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/");
                    if path == "/ddg" {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ddg_clone}",
                            ddg_clone.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    } else {
                        // Slow straggler (Bing / Wiki): stalls 3.2s
                        tokio::time::sleep(Duration::from_millis(3200)).await;
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("http://{addr}/ddg");
        endpoints.bing_html = format!("http://{addr}/slow-bing");
        endpoints.wikipedia_api = format!("http://{addr}/slow-wiki");

        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: None,
        };

        let start = StdInstant::now();
        let hits = search_mehh(
            &ctx,
            "what is adaptive straggler cutoff",
            Duration::from_secs(6),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(hits.len(), 6);
        assert!(
            elapsed < Duration::from_millis(2200),
            "straggler cutoff should stop waiting well before 3.5s budget; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_native_per_source_cooldown_skips_captcha_source_and_recovers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ddg_calls = Arc::new(AtomicUsize::new(0));
        let ddg_calls_bg = ddg_calls.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let ddg_calls_req = ddg_calls_bg.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/");
                    let body = match path {
                        "/ddg" => {
                            let call = ddg_calls_req.fetch_add(1, Ordering::SeqCst);
                            if call == 0 {
                                "<div class=\"anomaly-modal\">CAPTCHA</div>".to_string()
                            } else {
                                format!(
                                    "<div class=\"result web-result\"><a class=\"result__a\" href=\"https://ddg.example/recovered\">Recovered</a><div class=\"result__snippet\">{}</div></div>",
                                    "long ".repeat(110)
                                )
                            }
                        }
                        "/bing" => format!(
                            "<ol><li class=\"b_algo\"><h2><a href=\"https://bing.example/ok\">Bing OK</a></h2><p>{}</p></li></ol>",
                            "detailed ".repeat(65)
                        ),
                        _ => serde_json::json!({"query":{"search":[]}}).to_string(),
                    };
                    let ctype = if path == "/wiki" { "application/json" } else { "text/html" };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("http://{addr}/ddg");
        endpoints.bing_html = format!("http://{addr}/bing");
        endpoints.wikipedia_api = format!("http://{addr}/wiki");

        let state = NativeSearchState::new();
        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: Some(&state),
        };

        // 1st query: DDG serves CAPTCHA -> put on cooldown; Bing succeeds.
        let r1 = search_mehh(&ctx, "what is test1", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(r1[0].url, "https://bing.example/ok");
        assert_eq!(ddg_calls.load(Ordering::SeqCst), 1);

        // 2nd query: DDG is skipped while on cooldown.
        let r2 = search_mehh(&ctx, "what is test2", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(r2[0].url, "https://bing.example/ok");
        assert_eq!(ddg_calls.load(Ordering::SeqCst), 1);

        // Expire DDG cooldown -> 3rd query hits DDG again and recovers.
        state
            .0
            .source_cooldowns
            .lock()
            .unwrap()
            .insert(SourceId::DuckDuckGo, StdInstant::now() - Duration::from_secs(1));
        let r3 = search_mehh(&ctx, "what is test3", Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(ddg_calls.load(Ordering::SeqCst), 2);
        assert!(r3.iter().any(|h| h.url == "https://ddg.example/recovered"));
    }

    #[tokio::test]
    async fn test_search_mehh_3xx_failure_triggers_vendor_unavailable_failover() {
        let base = serve_routes(vec![
            ("/ddg", 302, "text/html", String::new()),
            ("/bing", 302, "text/html", String::new()),
            (
                "/wiki",
                200,
                "application/json",
                serde_json::json!({"query":{"search":[]}}).to_string(),
            ),
        ])
        .await;

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("{base}/ddg");
        endpoints.bing_html = format!("{base}/bing");
        endpoints.wikipedia_api = format!("{base}/wiki");

        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: None,
        };
        let err = search_mehh(
            &ctx,
            "what is 3xx failure failover",
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.should_failover(), "expected VendorUnavailable failover, got {err:?}");
    }

    #[tokio::test]
    async fn test_stage1_timeout_puts_hanging_source_on_cooldown() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bing_calls = Arc::new(AtomicUsize::new(0));
        let bing_calls_bg = bing_calls.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let bing_calls_req = bing_calls_bg.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/");
                    match path {
                        "/ddg" => {
                            let body = format!(
                                "<div class=\"result web-result\"><a class=\"result__a\" href=\"https://ddg.example/1\">One</a><div class=\"result__snippet\">{}</div></div>",
                                "long ".repeat(110)
                            );
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        "/bing" => {
                            bing_calls_req.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        _ => {
                            let body = serde_json::json!({"query":{"search":[]}}).to_string();
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                    }
                });
            }
        });

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut endpoints = NativeEndpoints::production();
        endpoints.ddg_html = format!("http://{addr}/ddg");
        endpoints.bing_html = format!("http://{addr}/bing");
        endpoints.wikipedia_api = format!("http://{addr}/wiki");

        let state = NativeSearchState::new();
        let ctx = NativeSearchContext {
            client: &client,
            enrich_client: &client,
            enrich_dns_policy: DnsPolicy::AllowPrivate,
            url_policy_checker: None,
            endpoints: &endpoints,
            state: Some(&state),
        };

        let hits = search_mehh_with_stage1_budget(
            &ctx,
            "what is hanging source cooldown",
            Duration::from_secs(5),
            Duration::from_millis(250),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(bing_calls.load(Ordering::SeqCst), 1);
        {
            let guard = state.0.source_cooldowns.lock().unwrap();
            assert!(
                guard.contains_key(&SourceId::Bing),
                "hanging Bing source must enter cooldown when Stage 1 times out"
            );
        }

        // Second query must skip Bing while it is cooling down.
        let _hits2 = search_mehh_with_stage1_budget(
            &ctx,
            "what is hanging source cooldown second",
            Duration::from_secs(5),
            Duration::from_millis(250),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(bing_calls.load(Ordering::SeqCst), 1);
    }
}
