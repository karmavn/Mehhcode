//! Built-in keyless web search and hybrid local-first web fetch tools.

pub mod fetch_tool;
pub mod html_markdown;
pub mod mcp_rpc;
pub mod output;
pub mod providers;
pub mod router;
pub mod search_tool;
pub mod types;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mehh_ai::http_safety::{build_safe_http_client, DnsPolicy};

pub use fetch_tool::WebFetchTool;
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

impl WebRuntime {
    /// Builds hardened HTTP clients and shared router; fails closed if client setup fails.
    pub fn try_new(config: WebConfig) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(config.effective_timeout_seconds());
        let local_client = build_safe_http_client(
            config.local_dns_policy(),
            Duration::from_secs(15),
            Some(timeout),
        )?;
        let router_client = build_safe_http_client(
            DnsPolicy::PublicOnly,
            Duration::from_secs(15),
            Some(Duration::from_secs(30)),
        )?;
        let router = WebRouter::new(config.endpoints.clone(), router_client);
        Ok(Self {
            config,
            local_fetch_client: local_client,
            router,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(config: WebConfig) -> Self {
        let timeout = Duration::from_secs(config.effective_timeout_seconds());
        let local_client = build_safe_http_client(
            config.local_dns_policy(),
            Duration::from_secs(5),
            Some(timeout),
        )
        .unwrap();
        let router_client = build_safe_http_client(
            DnsPolicy::AllowPrivate,
            Duration::from_secs(5),
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        let router = WebRouter::new(config.endpoints.clone(), router_client);
        Self {
            config,
            local_fetch_client: local_client,
            router,
        }
    }
}
