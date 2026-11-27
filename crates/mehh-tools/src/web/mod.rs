//! Built-in keyless web search and hybrid local-first web fetch tools.

pub mod fetch_tool;
pub mod html_markdown;
pub mod mcp_rpc;
pub mod native;
pub mod output;
pub mod providers;
pub mod router;
pub mod search_tool;
pub mod types;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use mehh_ai::http_safety::{build_safe_http_client, DnsPolicy};

pub use fetch_tool::WebFetchTool;
pub use native::NativeEndpoints;
pub use providers::WebEndpoints;
pub use router::WebRouter;
pub use search_tool::WebSearchTool;
pub use types::{ProviderId, WebProviderChoice};

/// Dependency-inverted async policy hook invoked on initial URL, every redirect hop,
/// and any post-redirect URL returned by remote extract providers.
#[async_trait]
pub trait UrlPolicyChecker: Send + Sync {
    async fn check_url(&self, url: &str) -> Result<(), String>;
}

/// Configuration for the built-in `web_search` and `web_fetch` tools.
#[derive(Clone)]
pub struct WebConfig {
    pub enabled: bool,
    pub provider: WebProviderChoice,
    pub allow_private_urls: bool,
    pub fetch_fallback: bool,
    pub timeout_seconds: u64,
    pub endpoints: WebEndpoints,
    pub url_policy_checker: Option<Arc<dyn UrlPolicyChecker>>,
    #[cfg(test)]
    pub(crate) allow_remote_extract_for_loopback_tests: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: WebProviderChoice::Auto,
            allow_private_urls: false,
            fetch_fallback: true,
            timeout_seconds: 30,
            endpoints: WebEndpoints::production(),
            url_policy_checker: None,
            #[cfg(test)]
            allow_remote_extract_for_loopback_tests: false,
        }
    }
}

impl std::fmt::Debug for WebConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("allow_private_urls", &self.allow_private_urls)
            .field("fetch_fallback", &self.fetch_fallback)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("has_url_policy_checker", &self.url_policy_checker.is_some())
            .finish()
    }
}

impl WebConfig {
    pub fn with_url_policy_checker(mut self, checker: Arc<dyn UrlPolicyChecker>) -> Self {
        self.url_policy_checker = Some(checker);
        self
    }

    pub fn effective_timeout_seconds(&self) -> u64 {
        self.timeout_seconds.clamp(1, 120)
    }

    pub fn local_dns_policy(&self) -> DnsPolicy {
        if self.allow_private_urls {
            DnsPolicy::AllowPrivate
        } else {
            DnsPolicy::PublicOnly
        }
    }
}

/// Shared runtime backing `web_search` and `web_fetch` within an agent session (and child subagents).
pub struct WebRuntime {
    pub config: WebConfig,
    pub local_fetch_client: reqwest::Client,
    pub router: WebRouter,
}

/// Deferred initializer for [`WebRuntime`] so CLI startup pays zero HTTP/TLS setup unless a web tool runs.
pub struct LazyWebRuntime {
    config: WebConfig,
    cell: OnceLock<Result<Arc<WebRuntime>, String>>,
}

impl LazyWebRuntime {
    pub fn new(config: WebConfig) -> Self {
        Self {
            config,
            cell: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Result<&Arc<WebRuntime>, String> {
        self.cell
            .get_or_init(|| {
                WebRuntime::try_new(self.config.clone())
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

#[derive(Clone)]
pub(crate) enum WebRuntimeHandle {
    Eager(Result<Arc<WebRuntime>, String>),
    Lazy(Arc<LazyWebRuntime>),
}

impl WebRuntimeHandle {
    pub(crate) fn resolve(&self) -> Result<&Arc<WebRuntime>, String> {
        match self {
            Self::Eager(res) => res.as_ref().map_err(Clone::clone),
            Self::Lazy(lazy) => lazy.get(),
        }
    }
}

impl WebRuntime {
    /// Builds hardened HTTP clients and shared router; fails closed if client setup fails.
    pub fn try_new(config: WebConfig) -> anyhow::Result<Self> {
        let local_policy = config.local_dns_policy();
        let local_client = build_safe_http_client(
            local_policy,
            Duration::from_secs(15),
            None,
        )?;
        let router_client = if local_policy == DnsPolicy::PublicOnly {
            local_client.clone()
        } else {
            build_safe_http_client(
                DnsPolicy::PublicOnly,
                Duration::from_secs(15),
                None,
            )?
        };
        let router = WebRouter::with_native_config(
            config.endpoints.clone(),
            router_client.clone(),
            router_client,
            DnsPolicy::PublicOnly,
            config.url_policy_checker.clone(),
        );
        Ok(Self {
            config,
            local_fetch_client: local_client,
            router,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(config: WebConfig) -> Self {
        let local_client = build_safe_http_client(
            config.local_dns_policy(),
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        let router_client = build_safe_http_client(
            DnsPolicy::AllowPrivate,
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        let router = WebRouter::with_native_config(
            config.endpoints.clone(),
            router_client.clone(),
            router_client,
            DnsPolicy::AllowPrivate,
            config.url_policy_checker.clone(),
        );
        Self {
            config,
            local_fetch_client: local_client,
            router,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_try_new_router_blocks_loopback_even_when_allow_private_urls_is_true() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicBool::new(false));
        let accepted_clone = accepted.clone();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                accepted_clone.store(true, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            }
        });

        let mut cfg = WebConfig {
            provider: WebProviderChoice::Fixed(ProviderId::Mehh),
            allow_private_urls: true,
            ..Default::default()
        };
        let port = addr.port();
        let loopback = format!("http://localhost:{port}/");
        cfg.endpoints.native.ddg_html = loopback.clone();
        cfg.endpoints.native.bing_html = loopback.clone();
        cfg.endpoints.native.wikipedia_api = loopback;

        let runtime = WebRuntime::try_new(cfg).unwrap();
        let cancel = CancellationToken::new();
        let err = runtime
            .router
            .search(
                WebProviderChoice::Fixed(ProviderId::Mehh),
                "what is ssrf",
                Duration::from_secs(3),
                &cancel,
                None,
            )
            .await
            .unwrap_err();
        assert!(err.should_failover());
        assert!(!accepted.load(Ordering::SeqCst));
    }
}
