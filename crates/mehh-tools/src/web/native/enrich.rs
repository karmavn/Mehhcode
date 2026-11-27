//! Bounded parallel Stage 2 Auto-Enrich: fetches top-ranked URLs under strict SSRF/policy
//! checks and replaces short SERP snippets with query-relevant Markdown passages.

use std::sync::Arc;
use std::time::Duration;

use mehh_ai::http_safety::{check_url_host_policy, DnsPolicy};
use reqwest::Url;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::web::html_markdown::{html_to_markdown, looks_like_empty_spa_shell};
use crate::web::mcp_rpc::{read_bounded_prefix, send_cancellable_request, WEB_USER_AGENT};
use crate::web::native::rank::{extract_relevant_passages_with_terms, tokenize_query_terms};
use crate::web::native::NativeSearchState;
use crate::web::types::{ProviderError, SearchHit};
use crate::web::UrlPolicyChecker;

pub(crate) const MAX_ENRICH_HITS: usize = 3;
pub(crate) const MAX_ENRICH_BYTES: usize = 512 * 1024;
pub(crate) const MAX_ENRICH_REDIRECTS: usize = 3;
pub(crate) const ENRICH_BUDGET: Duration = Duration::from_millis(2500);
pub(crate) const ENRICH_MAX_CHARS: usize = 900;
pub(crate) const ENRICH_SKIP_SNIPPET_LEN: usize = 500;

const BINARY_EXTENSIONS: &[&str] = &[
    ".pdf", ".zip", ".tar", ".gz", ".tgz", ".xz", ".7z", ".rar", ".png", ".jpg", ".jpeg", ".gif",
    ".webp", ".svg", ".ico", ".mp3", ".mp4", ".mov", ".avi", ".webm", ".exe", ".dll", ".so",
    ".dylib", ".wasm", ".bin",
];

/// Returns `true` when `hit` is eligible for Stage 2 page fetching and passage extraction.
pub(crate) fn is_enrich_candidate(hit: &SearchHit) -> bool {
    if hit.content.as_deref().map_or(0, str::len) >= ENRICH_SKIP_SNIPPET_LEN {
        return false;
    }
    let Ok(url) = Url::parse(hit.url.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let path_lower = url.path().to_ascii_lowercase();
    !BINARY_EXTENSIONS.iter().any(|ext| path_lower.ends_with(ext))
}

fn is_supported_text_mime(content_type: Option<&str>) -> bool {
    let Some(raw) = content_type else {
        return true;
    };
    let mime = raw
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();
    mime.is_empty()
        || mime.starts_with("text/html")
        || mime == "application/xhtml+xml"
        || mime == "text/markdown"
        || mime == "text/x-markdown"
        || mime == "text/plain"
}

async fn check_hop_allowed(
    dns_policy: DnsPolicy,
    url_policy_checker: Option<&UrlPolicyCheckerRef>,
    url: &Url,
) -> Result<(), ()> {
    if check_url_host_policy(dns_policy, url).is_err() {
        return Err(());
    }
    if let Some(checker) = url_policy_checker {
        if checker.check_url(url.as_str()).await.is_err() {
            return Err(());
        }
    }
    Ok(())
}

type UrlPolicyCheckerRef = Arc<dyn UrlPolicyChecker>;

#[derive(Clone)]
struct EnrichEnv {
    client: reqwest::Client,
    dns_policy: DnsPolicy,
    url_policy_checker: Option<UrlPolicyCheckerRef>,
    native_state: Option<NativeSearchState>,
    deadline: Instant,
    cancel: CancellationToken,
}

async fn fetch_and_extract_passage(
    env: EnrichEnv,
    raw_url: String,
    query_terms: Arc<[String]>,
) -> Option<String> {
    let client = &env.client;
    let dns_policy = env.dns_policy;
    let url_policy_checker = env.url_policy_checker;
    let native_state = env.native_state;
    let deadline = env.deadline;
    let cancel = env.cancel;
    let mut current = Url::parse(raw_url.trim()).ok()?;
    check_hop_allowed(dns_policy, url_policy_checker.as_ref(), &current)
        .await
        .ok()?;

    if let Some(ref state) = native_state {
        if let Some((cached_final, cached_md)) = state.get_cached_page(current.as_str()) {
            let final_allowed = if let Some(ref fu) = cached_final {
                if let Ok(parsed_final) = Url::parse(fu) {
                    check_hop_allowed(dns_policy, url_policy_checker.as_ref(), &parsed_final)
                        .await
                        .is_ok()
                } else {
                    false
                }
            } else {
                true
            };
            if final_allowed {
                return tokio::task::spawn_blocking(move || {
                    extract_relevant_passages_with_terms(&query_terms, &cached_md, ENRICH_MAX_CHARS)
                })
                .await
                .ok()
                .flatten();
            }
        }
    }

    let mut final_resp = None;
    let mut final_url = current.clone();
    for hop in 0..=MAX_ENRICH_REDIRECTS {
        let req = client
            .get(current.clone())
            .header(reqwest::header::USER_AGENT, WEB_USER_AGENT)
            .header(
                reqwest::header::ACCEPT,
                "text/markdown;q=1.0, text/html;q=0.9, text/plain;q=0.8, */*;q=0.1",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

        let resp = match send_cancellable_request(req, deadline, &cancel).await {
            Ok(r) => r,
            Err(ProviderError::Cancelled) | Err(_) => return None,
        };
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if hop >= MAX_ENRICH_REDIRECTS {
                return None;
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let next = current.join(loc).ok()?;
            if !matches!(next.scheme(), "http" | "https") {
                return None;
            }
            check_hop_allowed(dns_policy, url_policy_checker.as_ref(), &next)
                .await
                .ok()?;
            current = next;
            final_url = current.clone();
            continue;
        }
        if status >= 400 {
            return None;
        }
        final_resp = Some(resp);
        break;
    }

    let resp = final_resp?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !is_supported_text_mime(content_type.as_deref()) {
        return None;
    }

    let bytes = read_bounded_prefix(resp, MAX_ENRICH_BYTES, deadline, &cancel)
        .await
        .ok()?;
    if bytes.is_empty() {
        return None;
    }

    let is_plain_or_md = content_type.as_deref().is_some_and(|ct| {
        let lower = ct.to_ascii_lowercase();
        lower.contains("text/markdown") || lower.contains("text/plain")
    });

    let final_url_str = final_url.as_str().to_string();
    let (cacheable_md, passage) = tokio::task::spawn_blocking(move || {
        let body = String::from_utf8_lossy(&bytes);
        if body.trim().is_empty() {
            return (None, None);
        }
        let rendered = if is_plain_or_md {
            body.into_owned()
        } else {
            let md = html_to_markdown(&body, Some(&final_url));
            if looks_like_empty_spa_shell(&body, &md) {
                return (None, None);
            }
            md
        };
        let passage = extract_relevant_passages_with_terms(&query_terms, &rendered, ENRICH_MAX_CHARS);
        let md_arc: Arc<str> = Arc::from(rendered);
        (Some(md_arc), passage)
    })
    .await
    .ok()?;

    if let (Some(state), Some(md_arc)) = (native_state, cacheable_md) {
        let final_opt = (final_url_str != raw_url).then_some(final_url_str);
        state.cache_page(&raw_url, final_opt, md_arc);
    }

    passage
}

/// Fetches up to `MAX_ENRICH_HITS` top candidates concurrently within `deadline` and `ENRICH_BUDGET`,
/// upgrading `hit.content` when richer query-matched passages are extracted.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enrich_top_hits(
    client: &reqwest::Client,
    dns_policy: DnsPolicy,
    url_policy_checker: Option<&Arc<dyn UrlPolicyChecker>>,
    native_state: Option<&NativeSearchState>,
    query: &str,
    hits: &mut [SearchHit],
    deadline: Instant,
    cancel: &CancellationToken,
) {
    let candidates: Vec<(usize, String)> = hits
        .iter()
        .enumerate()
        .filter(|(_, h)| is_enrich_candidate(h))
        .take(MAX_ENRICH_HITS)
        .map(|(idx, h)| (idx, h.url.clone()))
        .collect();

    if candidates.is_empty() || cancel.is_cancelled() {
        return;
    }

    let query_terms: Arc<[String]> = Arc::from(tokenize_query_terms(query));
    let enrich_deadline = deadline.min(Instant::now() + ENRICH_BUDGET);
    let env = EnrichEnv {
        client: client.clone(),
        dns_policy,
        url_policy_checker: url_policy_checker.cloned(),
        native_state: native_state.cloned(),
        deadline: enrich_deadline,
        cancel: cancel.clone(),
    };
    let mut set = JoinSet::new();
    for (idx, url) in candidates {
        let env = env.clone();
        let query_terms = query_terms.clone();
        set.spawn(async move {
            let passage = fetch_and_extract_passage(env, url, query_terms).await;
            (idx, passage)
        });
    }

    let enrich_sleep = tokio::time::sleep_until(enrich_deadline);
    tokio::pin!(enrich_sleep);

    while let Some(joined) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            set.abort_all();
            None
        }
        _ = &mut enrich_sleep => {
            set.abort_all();
            None
        }
        res = set.join_next() => res,
    } {
        if let Ok((idx, Some(passage))) = joined {
            let existing_len = hits[idx].content.as_deref().map_or(0, str::len);
            if passage.len() > existing_len {
                hits[idx].content = Some(passage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mehh_ai::http_safety::build_safe_http_client;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_page_server(
        routes: Vec<(&'static str, u16, &'static str, &'static str, Duration)>,
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
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    if let Some((_, status, ctype, body, delay)) =
                        routes_clone.into_iter().find(|(p, _, _, _, _)| *p == path)
                    {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
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
    async fn test_enrich_upgrades_snippets_and_skips_binary_and_spa() {
        let base = spawn_page_server(vec![
            (
                "/rich",
                200,
                "text/html",
                "<html><body><h1>Tokio Guide</h1><p>Use tokio spawn_blocking for CPU tasks and sync I/O safely across worker threads.</p></body></html>",
                Duration::ZERO,
            ),
            (
                "/spa",
                200,
                "text/html",
                "<html><body><div id=\"root\"></div><script>window.__APP__=1;</script></body></html>",
                Duration::ZERO,
            ),
            (
                "/slow",
                200,
                "text/html",
                "<html><body><p>Slow tokio spawn_blocking page</p></body></html>",
                Duration::from_secs(4),
            ),
        ])
        .await;

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut hits = vec![
            SearchHit {
                url: format!("{base}/rich"),
                title: Some("Rich".into()),
                published: None,
                content: Some("short snippet".into()),
            },
            SearchHit {
                url: format!("{base}/file.pdf"),
                title: Some("PDF".into()),
                published: None,
                content: Some("pdf snippet".into()),
            },
            SearchHit {
                url: format!("{base}/spa"),
                title: Some("SPA".into()),
                published: None,
                content: Some("spa snippet".into()),
            },
            SearchHit {
                url: format!("{base}/slow"),
                title: Some("Slow".into()),
                published: None,
                content: Some("slow snippet".into()),
            },
        ];

        assert!(!is_enrich_candidate(&hits[1]));
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_millis(450);
        enrich_top_hits(
            &client,
            DnsPolicy::AllowPrivate,
            None,
            None,
            "tokio spawn_blocking",
            &mut hits,
            deadline,
            &cancel,
        )
        .await;

        assert!(hits[0].content.as_deref().unwrap().contains("spawn_blocking"));
        assert_eq!(hits[1].content.as_deref(), Some("pdf snippet"));
        assert_eq!(hits[2].content.as_deref(), Some("spa snippet"));
        assert_eq!(hits[3].content.as_deref(), Some("slow snippet"));
    }

    #[tokio::test]
    async fn test_enrich_enforces_url_policy_and_dns_policy_fail_closed() {
        struct DenyExamplePolicy;

        #[async_trait]
        impl UrlPolicyChecker for DenyExamplePolicy {
            async fn check_url(&self, url: &str) -> Result<(), String> {
                if url.contains("denied") {
                    Err("blocked by policy".to_string())
                } else {
                    Ok(())
                }
            }
        }

        let base = spawn_page_server(vec![(
            "/denied",
            200,
            "text/html",
            "<html><body><p>Secret page text</p></body></html>",
            Duration::ZERO,
        )])
        .await;

        // 1. DnsPolicy::PublicOnly blocks loopback URL before even connecting.
        let public_client =
            build_safe_http_client(DnsPolicy::PublicOnly, Duration::from_secs(5), None).unwrap();
        let mut hits = vec![SearchHit {
            url: format!("{base}/denied"),
            title: Some("Loopback".into()),
            published: None,
            content: Some("original snippet".into()),
        }];
        let cancel = CancellationToken::new();
        enrich_top_hits(
            &public_client,
            DnsPolicy::PublicOnly,
            None,
            None,
            "secret",
            &mut hits,
            Instant::now() + Duration::from_secs(2),
            &cancel,
        )
        .await;
        assert_eq!(hits[0].content.as_deref(), Some("original snippet"));

        // 2. UrlPolicyChecker blocks denied URL even when DNS policy allows private.
        let private_client =
            build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None).unwrap();
        let checker: Arc<dyn UrlPolicyChecker> = Arc::new(DenyExamplePolicy);
        enrich_top_hits(
            &private_client,
            DnsPolicy::AllowPrivate,
            Some(&checker),
            None,
            "secret",
            &mut hits,
            Instant::now() + Duration::from_secs(2),
            &cancel,
        )
        .await;
        assert_eq!(hits[0].content.as_deref(), Some("original snippet"));
    }

    #[tokio::test]
    async fn test_enrich_truncates_oversized_page_at_512k_instead_of_discarding() {
        let large_html = format!(
            "<html><body><h1>Tokio Prefix</h1><p>Use spawn_blocking safely for blocking work.</p><p>{}</p></body></html>",
            "padding ".repeat(80_000)
        );
        assert!(large_html.len() > MAX_ENRICH_BYTES);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{large_html}",
                large_html.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let mut hits = vec![SearchHit {
            url: format!("http://{addr}/huge"),
            title: Some("Huge Docs".into()),
            published: None,
            content: Some("tiny".into()),
        }];
        enrich_top_hits(
            &client,
            DnsPolicy::AllowPrivate,
            None,
            None,
            "tokio spawn_blocking",
            &mut hits,
            Instant::now() + Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            hits[0]
                .content
                .as_deref()
                .unwrap()
                .contains("spawn_blocking safely"),
            "expected oversized page prefix to still enrich, got {:?}",
            hits[0].content
        );
    }

    #[tokio::test]
    async fn test_enrich_url_cache_reuses_markdown_and_still_enforces_url_policy() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fc_bg = fetch_count.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let fc = fc_bg.clone();
                tokio::spawn(async move {
                    fc.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let body = "<html><body><h1>Guide</h1><p>Tokio async runtime details and Rust futures guide.</p></body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        struct TogglePolicy(Arc<AtomicBool>);
        #[async_trait]
        impl UrlPolicyChecker for TogglePolicy {
            async fn check_url(&self, _url: &str) -> Result<(), String> {
                if self.0.load(Ordering::SeqCst) {
                    Err("blocked now".to_string())
                } else {
                    Ok(())
                }
            }
        }

        let blocked = Arc::new(AtomicBool::new(false));
        let checker: Arc<dyn UrlPolicyChecker> = Arc::new(TogglePolicy(blocked.clone()));
        let state = NativeSearchState::new();
        let client = build_safe_http_client(DnsPolicy::AllowPrivate, Duration::from_secs(5), None)
            .unwrap();
        let url = format!("http://{addr}/cached");

        let mut hits1 = vec![SearchHit {
            url: url.clone(),
            title: Some("Guide".into()),
            published: None,
            content: Some("short".into()),
        }];
        enrich_top_hits(
            &client,
            DnsPolicy::AllowPrivate,
            Some(&checker),
            Some(&state),
            "tokio",
            &mut hits1,
            Instant::now() + Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        assert!(hits1[0].content.as_deref().unwrap().contains("Tokio async"));

        // 2nd query with different term hits URL cache without re-fetching HTTP.
        let mut hits2 = vec![SearchHit {
            url: url.clone(),
            title: Some("Guide".into()),
            published: None,
            content: Some("short".into()),
        }];
        enrich_top_hits(
            &client,
            DnsPolicy::AllowPrivate,
            Some(&checker),
            Some(&state),
            "futures",
            &mut hits2,
            Instant::now() + Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        assert!(hits2[0].content.as_deref().unwrap().contains("futures"));

        // Policy flip must still block cached page fail-closed.
        blocked.store(true, Ordering::SeqCst);
        let mut hits3 = vec![SearchHit {
            url,
            title: Some("Guide".into()),
            published: None,
            content: Some("short".into()),
        }];
        enrich_top_hits(
            &client,
            DnsPolicy::AllowPrivate,
            Some(&checker),
            Some(&state),
            "tokio",
            &mut hits3,
            Instant::now() + Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(hits3[0].content.as_deref(), Some("short"));
    }
}
