//! Closed domain types for Mehhcode's keyless web search and extract engine.

use std::fmt;
use std::time::Duration;

/// Built-in keyless web search/extract providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Exa,
    Parallel,
    Firecrawl,
    Tavily,
    TinyFish,
    Keenable,
}

impl ProviderId {
    /// All providers capable of `web_search`.
    pub const ALL_SEARCH: [ProviderId; 6] = [
        ProviderId::Exa,
        ProviderId::Parallel,
        ProviderId::Firecrawl,
        ProviderId::Tavily,
        ProviderId::TinyFish,
        ProviderId::Keenable,
    ];

    /// All providers capable of `web_fetch` Stage 2 remote page extraction.
    pub const ALL_EXTRACT: [ProviderId; 4] = [
        ProviderId::Exa,
        ProviderId::Parallel,
        ProviderId::Firecrawl,
        ProviderId::Keenable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
            Self::Firecrawl => "firecrawl",
            Self::Tavily => "tavily",
            Self::TinyFish => "tinyfish",
            Self::Keenable => "keenable",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Exa => "Exa",
            Self::Parallel => "Parallel",
            Self::Firecrawl => "Firecrawl",
            Self::Tavily => "Tavily",
            Self::TinyFish => "TinyFish",
            Self::Keenable => "Keenable",
        }
    }

    pub fn from_str_id(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "exa" => Some(Self::Exa),
            "parallel" => Some(Self::Parallel),
            "firecrawl" => Some(Self::Firecrawl),
            "tavily" => Some(Self::Tavily),
            "tinyfish" => Some(Self::TinyFish),
            "keenable" => Some(Self::Keenable),
            _ => None,
        }
    }

    pub fn supports_extract(self) -> bool {
        matches!(
            self,
            Self::Exa | Self::Parallel | Self::Firecrawl | Self::Keenable
        )
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configured provider routing mode from settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WebProviderChoice {
    #[default]
    Auto,
    Fixed(ProviderId),
    Disabled,
}

/// One normalized search hit returned by any keyless provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub url: String,
    pub title: Option<String>,
    pub published: Option<String>,
    pub content: Option<String>,
}

/// Extracted web page returned by Stage 2 keyless extract providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPage {
    pub url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub content: String,
}

/// Failure classification distinguishing failover-eligible vendor issues from terminal/origin errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    RateLimited {
        retry_after: Option<Duration>,
        detail: String,
    },
    VendorUnavailable {
        status: Option<u16>,
        detail: String,
    },
    OriginBlocked {
        status: u16,
        detail: String,
    },
    BadRequest(String),
    Cancelled,
}

impl ProviderError {
    pub fn should_failover(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::VendorUnavailable { .. }
        )
    }

    pub fn cooldown_duration(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => {
                Some(retry_after.unwrap_or(Duration::from_secs(60)))
            }
            Self::VendorUnavailable { status, .. } => {
                let secs = if matches!(status, Some(401 | 402 | 403)) {
                    300
                } else {
                    30
                };
                Some(Duration::from_secs(secs))
            }
            Self::OriginBlocked { .. } | Self::BadRequest(_) | Self::Cancelled => None,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited { detail, .. } => write!(f, "Rate limited: {detail}"),
            Self::VendorUnavailable { detail, .. } => write!(f, "{detail}"),
            Self::OriginBlocked { status, detail } => {
                write!(f, "HTTP {status} from target site: {detail}")
            }
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_capabilities_and_error_failover() {
        assert_eq!(ProviderId::ALL_SEARCH.len(), 6);
        assert_eq!(ProviderId::ALL_EXTRACT.len(), 4);
        assert!(ProviderId::Exa.supports_extract());
        assert!(!ProviderId::Tavily.supports_extract());
        assert!(!ProviderId::TinyFish.supports_extract());
        assert_eq!(ProviderId::from_str_id(" TinyFish "), Some(ProviderId::TinyFish));

        let rl = ProviderError::RateLimited {
            retry_after: None,
            detail: "429".into(),
        };
        assert!(rl.should_failover());
        assert_eq!(rl.cooldown_duration(), Some(Duration::from_secs(60)));

        let auth_refusal = ProviderError::VendorUnavailable {
            status: Some(403),
            detail: "blocked".into(),
        };
        assert!(auth_refusal.should_failover());
        assert_eq!(
            auth_refusal.cooldown_duration(),
            Some(Duration::from_secs(300))
        );

        let origin = ProviderError::OriginBlocked {
            status: 429,
            detail: "origin rate limited".into(),
        };
        assert!(!origin.should_failover());
        assert_eq!(origin.cooldown_duration(), None);
        assert!(!ProviderError::BadRequest("bad".into()).should_failover());
        assert!(!ProviderError::Cancelled.should_failover());
    }
}
