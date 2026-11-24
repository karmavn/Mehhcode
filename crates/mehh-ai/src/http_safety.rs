//! Shared SSRF protection, safe HTTP client construction, and URL validation helpers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

/// Controls which IP ranges [`SafeDnsResolver`] and [`check_url_host_policy`] permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsPolicy {
    /// Rejects all loopback, private, link-local, CGNAT, special-use, and cloud metadata addresses.
    PublicOnly,
    /// Allows loopback targets (`localhost`, `127.0.0.0/8`, `::1`) while blocking all other private/local IPs and metadata.
    PublicAndLoopback,
    /// Allows loopback and private network targets (for opt-in local dev servers) while still enforcing the cloud metadata floor.
    AllowPrivate,
}

/// DNS resolver that validates resolved addresses against a [`DnsPolicy`] at TCP connect time
/// to prevent DNS-rebinding (TOCTOU) attacks.
#[derive(Debug, Clone, Copy)]
pub struct SafeDnsResolver {
    policy: DnsPolicy,
}

impl SafeDnsResolver {
    pub fn new(policy: DnsPolicy) -> Self {
        Self { policy }
    }
}

impl reqwest::dns::Resolve for SafeDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy;
        Box::pin(async move {
            let host = name.as_str();
            let clean = clean_host(host);
            if is_always_blocked_hostname(clean) {
                return Err(format!("Blocked cloud metadata hostname: '{host}'").into());
            }
            if policy == DnsPolicy::PublicOnly && is_local_or_internal_hostname(clean) {
                return Err(format!("Local or internal hostname blocked: '{host}'").into());
            }
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((clean, 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            if addresses.is_empty() {
                return Err("DNS resolution returned no addresses".into());
            }
            for addr in &addresses {
                if !ip_allowed_for_policy(policy, clean, addr.ip()) {
                    return Err(format!(
                        "DNS policy blocked connection to '{host}' resolving to {}",
                        addr.ip()
                    )
                    .into());
                }
            }
            let addrs: reqwest::dns::Addrs = Box::new(addresses.into_iter());
            Ok(addrs)
        })
    }
}

fn clean_host(host: &str) -> &str {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
}

/// Returns true for cloud metadata hostnames that must always be blocked regardless of policy.
pub fn is_always_blocked_hostname(host: &str) -> bool {
    let clean = clean_host(host);
    clean.eq_ignore_ascii_case("metadata.google.internal")
        || clean.eq_ignore_ascii_case("metadata.goog")
}

/// Returns true for hostnames identifying the local machine.
pub fn is_loopback_host(host: &str) -> bool {
    let clean = clean_host(host);
    if clean.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = clean.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Returns true for localhost and reserved local domain suffixes.
pub fn is_local_or_internal_hostname(host: &str) -> bool {
    let lower = clean_host(host).to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower == "localhost.localdomain"
        || lower.ends_with(".localhost.localdomain")
        || is_always_blocked_hostname(&lower)
}

fn is_always_blocked_v4(v4: Ipv4Addr) -> bool {
    v4.is_link_local() || v4 == Ipv4Addr::new(100, 100, 100, 200)
}

fn embedded_v4_from_v6(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let segs = v6.segments();
    // ::ffff:0:0/96 (IPv4-mapped), ::/96 (IPv4-compatible),
    // ::ffff:0:0:0/96 (RFC 2765 IPv4-translated), 64:ff9b::/96 (NAT64)
    let embeds_v4 = (segs[0..5] == [0, 0, 0, 0, 0] && (segs[5] == 0xffff || segs[5] == 0))
        || (segs[0..4] == [0, 0, 0, 0] && segs[4] == 0xffff && segs[5] == 0)
        || (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6] == [0, 0, 0, 0]);
    if embeds_v4 {
        Some(Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            segs[6] as u8,
            (segs[7] >> 8) as u8,
            segs[7] as u8,
        ))
    } else {
        None
    }
}

/// Returns true for link-local and cloud metadata IP ranges blocked under every policy.
pub fn is_always_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_always_blocked_v4(v4),
        IpAddr::V6(v6) => {
            // AWS IPv6 metadata endpoint (fd00:ec2::254)
            if v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254) {
                return true;
            }
            embedded_v4_from_v6(v6).is_some_and(is_always_blocked_v4)
        }
    }
}

/// Returns true for private, loopback, link-local, CGNAT, special-use, or reserved IP addresses.
pub fn is_private_or_local_ip(ip: IpAddr) -> bool {
    if is_always_blocked_ip(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            let is_cgnat = octets[0] == 100 && (octets[1] & 0xc0) == 64; // 100.64.0.0/10
            let is_this_net = octets[0] == 0; // 0.0.0.0/8
            let is_ietf_proto = octets[0] == 192 && octets[1] == 0 && octets[2] == 0; // 192.0.0.0/24
            let is_benchmark = octets[0] == 198 && (octets[1] & 0xfe) == 18; // 198.18.0.0/15
            let is_reserved = octets[0] >= 240; // 240.0.0.0/4
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || is_cgnat
                || is_this_net
                || is_ietf_proto
                || is_benchmark
                || is_reserved
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            if let Some(v4) = embedded_v4_from_v6(v6) {
                return is_private_or_local_ip(IpAddr::V4(v4));
            }
            let segs = v6.segments();
            // Local NAT64 (64:ff9b:1::/48), 6to4 (2002::/16), Teredo (2001:0000::/32),
            // Discard-only (100::/64), Benchmarking (2001:2::/48), ORCHIDv2 (2001:20::/28),
            // ULA (fc00::/7), link-local (fe80::/10), site-local (fec0::/10)
            (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001)
                || segs[0] == 0x2002
                || (segs[0] == 0x2001 && segs[1] == 0x0000)
                || (segs[0] == 0x0100 && segs[1..4] == [0, 0, 0])
                || (segs[0] == 0x2001 && segs[1] == 0x0002 && segs[2] == 0)
                || (segs[0] == 0x2001 && (segs[1] & 0xfff0) == 0x0020)
                || (segs[0] & 0xfe00) == 0xfc00
                || (segs[0] & 0xffc0) == 0xfe80
                || (segs[0] & 0xffc0) == 0xfec0
        }
    }
}

/// Checks whether connecting to `ip` under `host` is allowed by `policy`.
pub fn ip_allowed_for_policy(policy: DnsPolicy, host: &str, ip: IpAddr) -> bool {
    if is_always_blocked_ip(ip) {
        return false;
    }
    match policy {
        DnsPolicy::PublicOnly => !is_private_or_local_ip(ip),
        DnsPolicy::PublicAndLoopback => {
            (is_loopback_host(host) && ip.is_loopback()) || !is_private_or_local_ip(ip)
        }
        DnsPolicy::AllowPrivate => true,
    }
}

/// Validates a URL's host or IP literal against `policy` before dispatching an HTTP request
/// (closing IP-literal bypasses where `reqwest` does not invoke DNS resolution).
pub fn check_url_host_policy(policy: DnsPolicy, url: &reqwest::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "URL is missing a host".to_string())?;
    let clean = clean_host(host);
    if is_always_blocked_hostname(clean) {
        return Err(format!("Blocked cloud metadata hostname: '{host}'"));
    }
    if policy == DnsPolicy::PublicOnly && is_local_or_internal_hostname(clean) {
        return Err(format!("Blocked local or internal hostname: '{host}'"));
    }
    if let Ok(ip) = clean.parse::<IpAddr>() {
        if !ip_allowed_for_policy(policy, clean, ip) {
            return Err(format!("Blocked private or restricted IP target: {ip}"));
        }
    }
    Ok(())
}

/// Builds a `reqwest::Client` with no automatic redirects and connect-time [`SafeDnsResolver`].
/// Fails closed if client construction fails.
pub fn build_safe_http_client(
    policy: DnsPolicy,
    connect_timeout: Duration,
    request_timeout: Option<Duration>,
) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(SafeDnsResolver::new(policy)));
    if let Some(timeout) = request_timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .build()
        .context("failed to build SSRF-hardened HTTP client")
}

/// Normalizes a caller-supplied URL (`http`/`https` only, repairs whitespace after `://`,
/// rejects embedded credentials, and strips `#fragment`).
pub fn normalize_and_validate_http_url(raw: &str) -> Result<reqwest::Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL must not be empty".to_string());
    }
    let repaired = if let Some(idx) = trimmed.find("://") {
        let scheme = &trimmed[..idx];
        let valid_scheme_chars = !scheme.is_empty()
            && scheme
                .bytes()
                .enumerate()
                .all(|(i, b)| {
                    if i == 0 {
                        b.is_ascii_alphabetic()
                    } else {
                        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
                    }
                });
        if valid_scheme_chars {
            let after = &trimmed[idx + 3..];
            let stripped = after.trim_start();
            if stripped.len() != after.len() {
                format!("{scheme}://{stripped}")
            } else {
                trimmed.to_string()
            }
        } else {
            trimmed.to_string()
        }
    } else {
        trimmed.to_string()
    };

    let mut url = reqwest::Url::parse(&repaired).map_err(|e| format!("Invalid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must use http:// or https://".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not include embedded credentials".to_string());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("URL is missing a host".to_string());
    }
    url.set_fragment(None);
    Ok(url)
}

const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "token",
    "key",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "auth",
    "access_token",
    "refresh_token",
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-credential",
    "session",
    "session_id",
    "jwt",
    "bearer",
];

const SENSITIVE_TOKEN_PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "AKIA", "eyJ"];

/// Returns true when the URL carries query parameters or path/query tokens that could leak secrets.
pub fn url_contains_potential_secret(url: &reqwest::Url) -> bool {
    for (key, value) in url.query_pairs() {
        let lower_key = key.to_ascii_lowercase();
        if SENSITIVE_QUERY_KEYS.contains(&lower_key.as_str()) {
            return true;
        }
        if SENSITIVE_TOKEN_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
        {
            return true;
        }
    }
    if let Some(segments) = url.path_segments() {
        for segment in segments {
            if SENSITIVE_TOKEN_PREFIXES
                .iter()
                .any(|prefix| segment.starts_with(prefix))
            {
                return true;
            }
        }
    }
    false
}

/// Checks whether a URL is safe to relay to third-party remote extractors:
/// rejects private/local/single-label destinations and secret-bearing URLs.
pub fn is_eligible_for_remote_extract(url: &reqwest::Url) -> Result<(), &'static str> {
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return Err("URL is missing a host");
    };
    let clean = clean_host(host);
    let lower = clean.to_ascii_lowercase();
    if is_local_or_internal_hostname(&lower)
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err("Local or internal hostnames cannot be sent to external extractors");
    }
    if let Ok(ip) = clean.parse::<IpAddr>() {
        if is_private_or_local_ip(ip) {
            return Err("Private or restricted IP targets cannot be sent to external extractors");
        }
    } else if !clean.contains('.') {
        return Err("Single-label internal hostnames cannot be sent to external extractors");
    }
    if url_contains_potential_secret(url) {
        return Err("URL containing sensitive query or token segments cannot be sent to external extractors");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_always_blocked_metadata_floor() {
        let ips = [
            "169.254.169.254",
            "169.254.170.2",
            "169.254.169.253",
            "100.100.100.200",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "::ffff:0:169.254.169.254",
            "64:ff9b::169.254.169.254",
        ];
        for raw in ips {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(is_always_blocked_ip(ip), "expected metadata IP {raw} blocked");
            assert!(
                !ip_allowed_for_policy(DnsPolicy::AllowPrivate, "example.com", ip),
                "AllowPrivate must still block metadata IP {raw}"
            );
        }

        for host in ["metadata.google.internal.", "METADATA.GOOG"] {
            assert!(is_always_blocked_hostname(host));
            let dummy = reqwest::Url::parse(&format!("http://{host}/")).unwrap();
            assert!(check_url_host_policy(DnsPolicy::AllowPrivate, &dummy).is_err());
        }
    }

    #[test]
    fn test_private_and_special_use_ips() {
        let blocked_public = [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "::1",
            "100.64.0.1",
            "fc00::1",
            "64:ff9b::10.0.0.1",
            "100::1",
            "2001:2::1",
            "2001:20::1",
        ];
        for raw in blocked_public {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(is_private_or_local_ip(ip), "expected {raw} private/local");
            assert!(!ip_allowed_for_policy(DnsPolicy::PublicOnly, "example.com", ip));
        }

        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(ip_allowed_for_policy(DnsPolicy::PublicAndLoopback, "localhost", loopback));
        assert!(ip_allowed_for_policy(DnsPolicy::AllowPrivate, "localhost", loopback));

        let lan: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!ip_allowed_for_policy(DnsPolicy::PublicAndLoopback, "localhost", lan));
        assert!(!ip_allowed_for_policy(DnsPolicy::PublicAndLoopback, "example.com", lan));
        assert!(ip_allowed_for_policy(DnsPolicy::AllowPrivate, "intranet", lan));
    }

    #[test]
    fn test_normalize_and_validate_http_url() {
        let normalized = normalize_and_validate_http_url("  https:// docs.rs/path#frag  ").unwrap();
        assert_eq!(normalized.as_str(), "https://docs.rs/path");

        assert!(normalize_and_validate_http_url("file:///etc/passwd").is_err());
        assert!(normalize_and_validate_http_url("https://user:pass@example.com").is_err());
        assert!(normalize_and_validate_http_url("https://user@example.com").is_err());
    }

    #[test]
    fn test_is_eligible_for_remote_extract() {
        let ok_url = normalize_and_validate_http_url("https://docs.rs/reqwest/latest").unwrap();
        assert!(is_eligible_for_remote_extract(&ok_url).is_ok());

        let rejected = [
            "http://localhost:8080/api",
            "http://127.0.0.1/api",
            "http://10.0.0.5/api",
            "http://sub.localhost/api",
            "http://service.internal/metrics",
            "http://intranet/docs",
            "https://example.com/hook?api_key=abc",
            "https://example.com/hook?token=xyz",
            "https://example.com/hook?X-Amz-Signature=deadbeef",
            "https://example.com/path/sk-ant-123456",
            "https://example.com/path/ghp_123456",
            "https://example.com/path/eyJhbGciOiJIUzI1NiJ9",
        ];
        for raw in rejected {
            let parsed = reqwest::Url::parse(raw).unwrap();
            assert!(
                is_eligible_for_remote_extract(&parsed).is_err(),
                "expected remote extract rejection for {raw}"
            );
        }
    }
}
