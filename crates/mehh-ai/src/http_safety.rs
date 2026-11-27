//! Shared SSRF protection, safe HTTP client construction, and URL validation helpers.

use std::borrow::Cow;
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

/// Returns the normalized lowercased host for `url`, stripping IPv6 brackets and FQDN trailing dots.
pub fn normalized_url_host(url: &reqwest::Url) -> Option<String> {
    url.host_str()
        .map(clean_host)
        .filter(|h| !h.is_empty())
        .map(str::to_ascii_lowercase)
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
    v4.is_link_local()
        || v4 == Ipv4Addr::new(100, 100, 100, 200)
        || v4 == Ipv4Addr::new(192, 0, 0, 192)
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

fn transition_embedded_v4_hits_metadata(v6: Ipv6Addr) -> bool {
    if embedded_v4_from_v6(v6).is_some_and(is_always_blocked_v4) {
        return true;
    }
    let segs = v6.segments();
    // RFC 8215 / RFC 6052 local NAT64 (64:ff9b:1::/48) supports /48, /56, /64, and /96 embeddings.
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001 {
        let b = v6.octets();
        let candidates = [
            Ipv4Addr::new(b[6], b[7], b[9], b[10]),
            Ipv4Addr::new(b[7], b[9], b[10], b[11]),
            Ipv4Addr::new(b[9], b[10], b[11], b[12]),
            Ipv4Addr::new(b[12], b[13], b[14], b[15]),
        ];
        if candidates.into_iter().any(is_always_blocked_v4) {
            return true;
        }
    }
    // RFC 3056 6to4 (2002::/16)
    if segs[0] == 0x2002 {
        let v4 = Ipv4Addr::new(
            (segs[1] >> 8) as u8,
            segs[1] as u8,
            (segs[2] >> 8) as u8,
            segs[2] as u8,
        );
        if is_always_blocked_v4(v4) {
            return true;
        }
    }
    // RFC 4380 Teredo (2001:0000::/32): obscured IPv4 in segments 6-7
    if segs[0] == 0x2001 && segs[1] == 0x0000 {
        let hi = !segs[6];
        let lo = !segs[7];
        let v4 = Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
        if is_always_blocked_v4(v4) {
            return true;
        }
    }
    false
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
            // IPv6 link-local (fe80::/10)
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            transition_embedded_v4_hits_metadata(v6)
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
                || v4.is_documentation()
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
            // Documentation (2001:db8::/32), ULA (fc00::/7), link-local (fe80::/10),
            // site-local (fec0::/10)
            (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001)
                || segs[0] == 0x2002
                || (segs[0] == 0x2001 && segs[1] == 0x0000)
                || (segs[0] == 0x0100 && segs[1..4] == [0, 0, 0])
                || (segs[0] == 0x2001 && segs[1] == 0x0002 && segs[2] == 0)
                || (segs[0] == 0x2001 && (segs[1] & 0xfff0) == 0x0020)
                || (segs[0] == 0x2001 && segs[1] == 0x0db8)
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
    if clean.is_empty() {
        return Err("URL is missing a host".to_string());
    }
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

/// Returns the URL path with trailing slashes removed when `base_url` (or `default_base`
/// if empty) parses as a URL, or falls back to the trimmed raw string without trailing slashes.
pub(crate) fn normalized_endpoint_path(base_url: &str, default_base: &str) -> String {
    let trimmed = base_url.trim();
    let raw = if trimmed.is_empty() {
        default_base
    } else {
        trimmed
    };
    if let Ok(parsed) = reqwest::Url::parse(raw) {
        parsed.path().trim_end_matches('/').to_string()
    } else {
        raw.trim_end_matches('/').to_string()
    }
}

/// Appends `extra_segments` to `base_url` (falling back to `default_base` when empty)
/// while preserving any existing query parameters on `base_url`.
pub(crate) fn append_endpoint_segments_preserving_query(
    base_url: &str,
    default_base: &str,
    extra_segments: &[&str],
) -> String {
    let trimmed = base_url.trim();
    let raw = if trimmed.is_empty() {
        default_base
    } else {
        trimmed
    };

    if let Ok(mut parsed) = reqwest::Url::parse(raw) {
        let mut new_path = parsed.path().trim_end_matches('/').to_string();
        for seg in extra_segments {
            let seg = seg.trim_matches('/');
            if !seg.is_empty() {
                new_path.push('/');
                new_path.push_str(seg);
            }
        }
        if parsed.path() != new_path {
            parsed.set_path(&new_path);
        }
        if parsed.query().is_none() && parsed.fragment().is_none() {
            return parsed.as_str().trim_end_matches('/').to_string();
        }
        return parsed.to_string();
    }

    let fallback_base = raw.trim_end_matches('/');
    if extra_segments.is_empty() {
        fallback_base.to_string()
    } else {
        format!("{fallback_base}/{}", extra_segments.join("/"))
    }
}

fn is_sensitive_query_key(s: &str) -> bool {
    matches!(
        s,
        "token"
            | "key"
            | "api_key"
            | "apikey"
            | "secret"
            | "password"
            | "passwd"
            | "auth"
            | "authorization"
            | "access_token"
            | "refresh_token"
            | "sig"
            | "signature"
            | "x-amz-signature"
            | "x-amz-credential"
            | "session"
            | "session_id"
            | "sessionid"
            | "jwt"
            | "bearer"
    )
}

fn has_sensitive_key_suffix(s: &str) -> bool {
    if s.len() < 5 {
        return false;
    }
    match s.as_bytes()[s.len() - 1] {
        b'n' => s.ends_with("token"),
        b't' => s.ends_with("secret"),
        b'd' => s.ends_with("password") || s.ends_with("passwd"),
        b'e' => s.ends_with("signature"),
        b'l' => s.ends_with("credential"),
        b'y' => {
            s.ends_with("key")
                && (s.ends_with("apikey")
                    || s.ends_with("accesskey")
                    || s.ends_with("secretkey")
                    || s.ends_with("authkey")
                    || s.ends_with("privatekey"))
        }
        _ => false,
    }
}

fn has_sensitive_token_prefix(raw: &str) -> bool {
    let trimmed = raw.trim();
    let candidate = match trimmed.as_bytes().first() {
        Some(b'b' | b'B')
            if trimmed
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("bearer ")) =>
        {
            trimmed[7..].trim_start()
        }
        _ => trimmed,
    };
    match candidate.as_bytes().first() {
        Some(b's') => candidate.starts_with("sk-"),
        Some(b'g') => candidate.starts_with("ghp_") || candidate.starts_with("gho_"),
        Some(b'A') => candidate.starts_with("AKIA"),
        Some(b'e') => candidate.starts_with("eyJ"),
        _ => false,
    }
}

fn percent_decode_ascii(input: &str) -> Cow<'_, str> {
    if !input.contains('%') {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

fn query_pair_is_sensitive(key: &str, value: &str) -> bool {
    let kb = key.as_bytes();
    let mut has_upper = false;
    let mut has_sep = false;
    let mut idx = 0;
    while idx < kb.len() {
        let b = kb[idx];
        if b.is_ascii_uppercase() {
            has_upper = true;
        } else if b == b'-' || b == b'_' {
            has_sep = true;
        }
        idx += 1;
    }
    let lower_key: Cow<'_, str> = if has_upper {
        Cow::Owned(key.to_ascii_lowercase())
    } else {
        Cow::Borrowed(key)
    };
    let compact_key: Cow<'_, str> = if has_sep {
        let mut stripped = String::with_capacity(lower_key.len());
        for ch in lower_key.chars() {
            if ch != '-' && ch != '_' {
                stripped.push(ch);
            }
        }
        Cow::Owned(stripped)
    } else {
        Cow::Borrowed(lower_key.as_ref())
    };
    if is_sensitive_query_key(lower_key.as_ref())
        || is_sensitive_query_key(compact_key.as_ref())
        || has_sensitive_key_suffix(compact_key.as_ref())
    {
        return true;
    }
    if has_sensitive_token_prefix(value) {
        return true;
    }
    if let Cow::Owned(decoded_val) = percent_decode_ascii(value) {
        if has_sensitive_token_prefix(&decoded_val) {
            return true;
        }
    }
    false
}

/// Returns true when the URL carries query parameters or path/query tokens that could leak secrets.
pub fn url_contains_potential_secret(url: &reqwest::Url) -> bool {
    if let Some(raw_query) = url.query() {
        if !raw_query.contains('%') && !raw_query.contains('+') {
            for piece in raw_query.split('&') {
                if piece.is_empty() {
                    continue;
                }
                let (key, value) = piece.split_once('=').unwrap_or((piece, ""));
                if query_pair_is_sensitive(key, value) {
                    return true;
                }
            }
        } else {
            for (key, value) in url.query_pairs() {
                if query_pair_is_sensitive(&key, &value) {
                    return true;
                }
            }
        }
    }
    if let Some(segments) = url.path_segments() {
        for segment in segments {
            if has_sensitive_token_prefix(segment) {
                return true;
            }
            if let Cow::Owned(decoded_seg) = percent_decode_ascii(segment) {
                if has_sensitive_token_prefix(&decoded_seg) {
                    return true;
                }
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
            "192.0.0.192",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "::ffff:0:169.254.169.254",
            "64:ff9b::169.254.169.254",
            "64:ff9b:1::169.254.169.254",
            // RFC 8215 / RFC 6052 /48, /56, /64 embeddings of 169.254.169.254
            "64:ff9b:1:a9fe:00a9:fe00::",
            "64:ff9b:1:00a9:00fe:a9fe::",
            "64:ff9b:1::00a9:fea9:fe00:0",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:5601:5601",
        ];
        for raw in ips {
            let ip: IpAddr = raw.parse().unwrap();
            assert!(is_always_blocked_ip(ip), "expected metadata IP {raw} blocked");
            assert!(
                !ip_allowed_for_policy(DnsPolicy::AllowPrivate, "example.com", ip),
                "AllowPrivate must still block metadata IP {raw}"
            );
        }

        // Public IPv4 embedded in 6to4 must remain blocked as private/special-use,
        // while not falsely matching the cloud metadata floor.
        let public_6to4: IpAddr = "2002:0101:0101::1".parse().unwrap();
        assert!(is_private_or_local_ip(public_6to4));
        assert!(!is_always_blocked_ip(public_6to4));

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
            "https://example.com/hook?authorization=xyz",
            "https://example.com/hook?aws_access_key=abc",
            "https://example.com/hook?secret_key=abc",
            "https://example.com/hook?auth_key=abc",
            "https://example.com/hook?q=Bearer%20eyJhbGciOiJIUzI1NiJ9",
            "https://example.com/hook?X-Amz-Signature=deadbeef",
            "https://example.com/hook?client_secret=abc",
            "https://example.com/hook?id_token=abc",
            "https://example.com/hook?x-amz-security-token=abc",
            "https://example.com/hook?x-goog-signature=abc",
            "https://example.com/hook?session-id=abc",
            "https://example.com/hook?sessionId=abc",
            "https://example.com/hook?cl%C3%A9_token=abc",
            "https://example.com/path/sk-ant-123456",
            "https://example.com/path/sk%2Dant-123456",
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

        let dot_host = reqwest::Url::parse("http://./").unwrap();
        assert_eq!(normalized_url_host(&dot_host), None);
    }

    #[test]
    fn check_url_host_policy_rejects_dot_host() {
        let dot_host = reqwest::Url::parse("http://./").unwrap();
        assert!(check_url_host_policy(DnsPolicy::PublicOnly, &dot_host).is_err());
        assert!(check_url_host_policy(DnsPolicy::AllowPrivate, &dot_host).is_err());
    }

    #[test]
    fn append_endpoint_segments_preserves_query_params() {
        assert_eq!(
            append_endpoint_segments_preserving_query(
                "https://proxy.example.com/v1?api-version=2024",
                "https://api.openai.com/v1",
                &["chat", "completions"],
            ),
            "https://proxy.example.com/v1/chat/completions?api-version=2024"
        );
        assert_eq!(
            append_endpoint_segments_preserving_query(
                "https://proxy.example.com/v1/messages/?api-version=2024",
                "https://api.anthropic.com/v1",
                &[],
            ),
            "https://proxy.example.com/v1/messages?api-version=2024"
        );
        assert_eq!(
            append_endpoint_segments_preserving_query(
                "",
                "https://api.openai.com/v1",
                &["chat", "completions"],
            ),
            "https://api.openai.com/v1/chat/completions"
        );
    }
}
