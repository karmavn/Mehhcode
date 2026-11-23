//! OAuth 2.0 PKCE, dynamic client registration, discovery, callback server, and token storage for MCP.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

/// RFC 8414 & OIDC Authorization Server / Protected Resource Metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// OAuth 2.0 Access & Refresh tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Converts an OAuth `expires_in` (SECONDS) into the absolute Unix-seconds
/// timestamp stored in `McpAuthEntry::tokens_expire_at`. `bearer_token()` and
/// `do_refresh()` compare it against `SystemTime::as_secs()`.
pub fn mcp_token_expiry_seconds(expires_in: u64) -> i64 {
    let now = mehh_ai::now_ms() / 1000;
    now.saturating_add(i64::try_from(expires_in).unwrap_or(i64::MAX))
}

/// Stored authentication state per MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAuthEntry {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_expire_at: Option<i64>,
}

const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OAUTH_BODY_BYTES: usize = 256 * 1024;
const MAX_OAUTH_ERROR_BYTES: usize = 16 * 1024;
const MAX_OAUTH_DISCOVERY_DEPTH: u8 = 1;

/// Returns the standard default path for MCP authentication state: `~/.mehh/agent/mcp-auth.json`.
/// Fails closed if neither `USERPROFILE` nor `HOME` is set to a non-empty path.
pub fn default_mcp_auth_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot resolve MCP auth path: neither USERPROFILE nor HOME is set"
            )
        })?;
    Ok(PathBuf::from(home)
        .join(".mehh")
        .join("agent")
        .join("mcp-auth.json"))
}

/// Validates that an OAuth URL uses `https://` or loopback `http://` (`localhost`, `127.0.0.1`, `[::1]`).
pub fn validate_oauth_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw)
        .with_context(|| format!("Invalid OAuth URL '{raw}'"))?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OAuth URL '{raw}' is missing a host"))?;

    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback_host(host) => Ok(url),
        "http" => anyhow::bail!(
            "Insecure non-loopback HTTP scheme rejected for OAuth URL '{raw}'"
        ),
        other => anyhow::bail!("Unsupported URL scheme '{other}' in OAuth URL '{raw}'"),
    }
}

/// DNS resolver that rejects private/local addresses at connection time.
///
/// Checking DNS before sending a request is vulnerable to rebinding: the
/// second resolution performed by reqwest can return a different address.
/// Resolving inside the connector makes validation and connection atomic.
pub(crate) struct SafeDnsResolver;

impl reqwest::dns::Resolve for SafeDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str();
            let clean = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_end_matches('.');
            let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((clean, 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            if addresses.is_empty() {
                return Err("DNS resolution returned no addresses".into());
            }
            if !is_loopback_host(clean) {
                for addr in &addresses {
                    if is_private_or_local_ip(addr.ip()) {
                        return Err(format!(
                            "DNS rebinding blocked: '{host}' resolved to private/local IP {}",
                            addr.ip()
                        )
                        .into());
                    }
                }
            }
            let addrs: reqwest::dns::Addrs = Box::new(addresses.into_iter());
            Ok(addrs)
        })
    }
}

/// Builds an HTTP client for OAuth requests with timeouts, no redirects and SSRF-safe DNS.
/// Fails closed if the client cannot be built instead of falling back to an
/// insecure default client.
pub fn oauth_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(OAUTH_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(SafeDnsResolver))
        .build()
        .expect("failed to build hardened OAuth HTTP client")
}

fn is_loopback_host(host: &str) -> bool {
    let clean = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    if clean.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = clean.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

fn is_local_or_internal_hostname(host: &str) -> bool {
    let clean = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    let lower = clean.to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".localhost")
        || lower == "localhost.localdomain"
        || lower.ends_with(".localhost.localdomain")
}

fn is_private_or_local_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            let is_cgnat = octets[0] == 100 && (octets[1] & 0xc0) == 64;
            let is_this_net = octets[0] == 0;
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || is_cgnat
                || is_this_net
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            let segs = v6.segments();
            // ::ffff:0:0/96 (IPv4-mapped IPv6) or ::0.0.0.0/96 (IPv4-compatible IPv6)
            if segs[0..5] == [0, 0, 0, 0, 0] && (segs[5] == 0xffff || segs[5] == 0) {
                let v4 = std::net::Ipv4Addr::new(
                    (segs[6] >> 8) as u8,
                    segs[6] as u8,
                    (segs[7] >> 8) as u8,
                    segs[7] as u8,
                );
                return is_private_or_local_ip(std::net::IpAddr::V4(v4));
            }
            // fc00::/7 (ULA) or fe80::/10 (link-local)
            (segs[0] & 0xfe00) == 0xfc00 || (segs[0] & 0xffc0) == 0xfe80
        }
    }
}

fn is_disallowed_discovery_redirect(parent_url: &reqwest::Url, target_url: &reqwest::Url) -> bool {
    let parent_is_loopback = parent_url.host_str().is_some_and(is_loopback_host);
    if parent_is_loopback {
        return false;
    }
    let Some(host) = target_url.host_str() else {
        return true;
    };
    if is_local_or_internal_hostname(host) {
        return true;
    }
    let clean = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = clean.parse::<std::net::IpAddr>() {
        return is_private_or_local_ip(ip);
    }
    false
}

async fn validate_oauth_target_dns(
    allow_local: bool,
    target_url: &reqwest::Url,
) -> anyhow::Result<()> {
    if allow_local {
        return Ok(());
    }
    let host = target_url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OAuth URL '{target_url}' is missing a host"))?;
    if is_local_or_internal_hostname(host) {
        anyhow::bail!("Local OAuth host '{host}' rejected for non-loopback server");
    }
    let clean = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = clean.parse::<std::net::IpAddr>() {
        if is_private_or_local_ip(ip) {
            anyhow::bail!("Private or local OAuth IP '{ip}' rejected for '{target_url}'");
        }
        return Ok(());
    }
    let port = target_url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((clean, port))
        .await
        .with_context(|| format!("Failed to resolve OAuth host '{clean}'"))?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("OAuth host '{clean}' resolved to no addresses");
    }
    for addr in addrs {
        if is_private_or_local_ip(addr.ip()) {
            anyhow::bail!(
                "OAuth host '{clean}' resolved to disallowed private/local IP '{}'",
                addr.ip()
            );
        }
    }
    Ok(())
}

/// Validates an outbound OAuth URL (scheme, host literals, and DNS IPs when non-loopback).
pub async fn validate_oauth_request_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = validate_oauth_url(raw)?;
    let allow_local = url.host_str().is_some_and(is_loopback_host);
    validate_oauth_target_dns(allow_local, &url).await?;
    Ok(url)
}

async fn read_bounded_oauth_bytes(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            anyhow::bail!("OAuth HTTP response Content-Length ({len} bytes) exceeds limit of {max_bytes} bytes");
        }
    }
    let mut buf = Vec::new();
    while let Some(chunk) = tokio::time::timeout(OAUTH_HTTP_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| anyhow::anyhow!("Timed out reading OAuth HTTP response body"))??
    {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("OAuth HTTP response body exceeded limit of {max_bytes} bytes");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

const REDACTED_SECRET: &str = "[REDACTED]";

/// Returns true for JSON keys whose values are credentials.
fn is_secret_json_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    normalized == "token"
        || normalized == "secret"
        || normalized == "key"
        || normalized == "authorization"
        || normalized == "password"
        || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("password")
        || normalized.ends_with("apikey")
        || normalized.ends_with("secretkey")
        || normalized.ends_with("privatekey")
}

/// True when free text mentions a credential concept (for example an OAuth
/// `error_description` naming the parameter that failed).
fn is_secret_bearing_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["token", "secret", "password", "authorization", "key"]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_secret_json_key(key)
                    || (key.eq_ignore_ascii_case("error_description")
                        && child.as_str().is_some_and(is_secret_bearing_text))
                {
                    *child = serde_json::Value::String(REDACTED_SECRET.to_string());
                } else {
                    redact_json_value(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn is_opaque_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
}

/// Replaces `Bearer <token>` and `sk-...` shapes in free text, where no JSON
/// key identifies the value as a credential.
fn redact_secret_shaped_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].len() >= 7 && bytes[i..i + 7].eq_ignore_ascii_case(b"bearer ") {
            let mut j = i + 7;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && is_opaque_token_byte(bytes[j]) {
                out.push_str("Bearer [REDACTED]");
                while j < bytes.len() && is_opaque_token_byte(bytes[j]) {
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        if bytes[i..].starts_with(b"sk-") {
            let mut j = i + 3;
            while j < bytes.len() && is_opaque_token_byte(bytes[j]) {
                j += 1;
            }
            out.push_str(REDACTED_SECRET);
            i = j;
            continue;
        }
        let ch = text[i..]
            .chars()
            .next()
            .expect("index is always a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Percent-encodes a secret's non-unreserved bytes in both `%XX` spellings.
/// Providers echo token values encoded and disagree on hex letter casing.
fn percent_encoded_secret_forms(secret: &str) -> (String, String) {
    let mut upper = String::with_capacity(secret.len());
    let mut lower = String::with_capacity(secret.len());
    for &b in secret.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            upper.push(b as char);
            lower.push(b as char);
        } else {
            upper.push_str(&format!("%{b:02X}"));
            lower.push_str(&format!("%{b:02x}"));
        }
    }
    (upper, lower)
}

/// Redacts known request secrets (plain and percent-encoded), JSON credential
/// keys, and bearer/sk- token shapes before an error body reaches a message.
pub(crate) fn redact_oauth_error_body(body: &str, secrets: &[&str]) -> String {
    let mut redacted = body.to_string();
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        redacted = redacted.replace(secret, REDACTED_SECRET);
        let (upper, lower) = percent_encoded_secret_forms(secret);
        redacted = redacted.replace(&upper, REDACTED_SECRET);
        redacted = redacted.replace(&lower, REDACTED_SECRET);
    }
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&redacted) {
        redact_json_value(&mut value);
        redacted = value.to_string();
    }
    redact_secret_shaped_values(&redacted)
}

/// Persistent store for MCP OAuth credentials keyed by a length-prefixed name plus URL.
pub struct McpAuthStore {
    path: PathBuf,
    lock_path: PathBuf,
    process_lock: Arc<tokio::sync::Mutex<()>>,
    entries: RwLock<HashMap<String, McpAuthEntry>>,
    #[cfg(test)]
    pub(crate) load_count: std::sync::atomic::AtomicUsize,
}

impl McpAuthStore {
    /// Creates a store at `~/.mehh/agent/mcp-auth.json`, failing closed on missing HOME or corrupt JSON.
    pub fn try_new() -> anyhow::Result<Self> {
        Self::try_with_path(default_mcp_auth_path()?)
    }

    /// Creates a store at `path`, failing closed on invalid JSON or read errors other than `NotFound`.
    pub fn try_with_path(path: PathBuf) -> anyhow::Result<Self> {
        let entries = Self::load_from_path(&path)?;
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let process_lock = mehh_ai::oauth::get_path_lock_mutex(&lock_path);
        Ok(Self {
            path,
            lock_path,
            process_lock,
            entries: RwLock::new(entries),
            #[cfg(test)]
            load_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Generates an unambiguous key: the byte-length-prefixed name followed by the URL.
    /// Length-prefixing the name prevents a `|` in either field from merging two identities.
    pub fn store_key(server_name: &str, server_url: &str) -> String {
        format!("{}:{}|{}", server_name.len(), server_name, server_url)
    }

    /// Acquires an advisory file lock on `<path>.lock`.
    pub async fn lock(&self) -> anyhow::Result<mehh_ai::oauth::FileLockGuard> {
        mehh_ai::oauth::acquire_file_lock(&self.lock_path, self.process_lock.clone()).await
    }

    fn load_from_path(path: &Path) -> anyhow::Result<HashMap<String, McpAuthEntry>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to read MCP auth store '{}'", path.display())
                });
            }
        };
        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let map = serde_json::from_str(clean).with_context(|| {
            format!("Failed to parse MCP auth store '{}'", path.display())
        })?;
        Ok(map)
    }

    async fn load_fresh(&self) -> anyhow::Result<HashMap<String, McpAuthEntry>> {
        #[cfg(test)]
        self.load_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || Self::load_from_path(&path))
            .await
            .context("auth store load task panicked")?
    }

    async fn save_internal(&self, map: &HashMap<String, McpAuthEntry>) -> anyhow::Result<()> {
        let path = self.path.clone();
        let map = map.clone();
        tokio::task::spawn_blocking(move || Self::save_internal_blocking(&path, &map))
            .await
            .context("auth store save task panicked")?
    }

    fn save_internal_blocking(path: &Path, map: &HashMap<String, McpAuthEntry>) -> anyhow::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            anyhow::anyhow!("MCP auth store path '{}' has no parent directory", path.display())
        })?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create directory '{}'", parent.display())
        })?;

        let json = serde_json::to_string_pretty(map)?;
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mcp-auth.json");
        let tmp_path = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));

        let write_res: anyhow::Result<()> = (|| {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp_path).with_context(|| {
                format!("Failed to create temp auth file '{}'", tmp_path.display())
            })?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
            }
            std::fs::rename(&tmp_path, path).with_context(|| {
                format!(
                    "Failed to atomically replace '{}' with '{}'",
                    path.display(),
                    tmp_path.display()
                )
            })?;
            Ok(())
        })();

        if write_res.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write_res
    }

    /// Retrieves the stored entry for a server if present.
    ///
    /// Disk is authoritative: another process may have rotated a token since
    /// this store was created. The in-memory map is only a fallback for a
    /// transient read failure.
    pub async fn get(&self, server_name: &str, server_url: &str) -> Option<McpAuthEntry> {
        let key = Self::store_key(server_name, server_url);
        match self.load_fresh().await {
            Ok(disk_map) => {
                let res = disk_map.get(&key).cloned();
                *self.entries.write().await = disk_map;
                res
            }
            Err(_) => self.entries.read().await.get(&key).cloned(),
        }
    }

    /// Saves an entry for a server to memory and persists to disk under advisory file lock.
    pub async fn set(
        &self,
        server_name: &str,
        server_url: &str,
        entry: McpAuthEntry,
    ) -> anyhow::Result<()> {
        let _lock = self.lock().await?;
        let key = Self::store_key(server_name, server_url);
        let mut disk_map = self.load_fresh().await?;
        disk_map.insert(key, entry);
        self.save_internal(&disk_map).await?;
        *self.entries.write().await = disk_map;
        Ok(())
    }

    /// Removes an entry for a server from memory and disk under advisory file lock.
    pub async fn remove(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> anyhow::Result<Option<McpAuthEntry>> {
        let _lock = self.lock().await?;
        let key = Self::store_key(server_name, server_url);
        let mut disk_map = self.load_fresh().await?;
        let removed = disk_map.remove(&key);
        if removed.is_some() {
            self.save_internal(&disk_map).await?;
        }
        *self.entries.write().await = disk_map;
        Ok(removed)
    }

    /// Returns all stored entries.
    pub async fn list(&self) -> HashMap<String, McpAuthEntry> {
        if let Ok(disk_map) = self.load_fresh().await {
            *self.entries.write().await = disk_map.clone();
            disk_map
        } else {
            self.entries.read().await.clone()
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Builds RFC 8414 & OIDC discovery URL candidates for an MCP server endpoint.
pub fn build_discovery_urls(server_url: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let trimmed = server_url.trim_end_matches('/');

    let parsed = reqwest::Url::parse(trimmed).ok();
    let (base, path) = if let Some(ref u) = parsed {
        let origin = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
        let port_part = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        let base = format!("{origin}{port_part}");
        let p = u.path().trim_end_matches('/');
        (base, p.to_string())
    } else {
        (trimmed.to_string(), String::new())
    };

    if !path.is_empty() && path != "/" {
        urls.push(format!("{base}/.well-known/oauth-protected-resource{path}"));
        urls.push(format!("{base}/.well-known/oauth-protected-resource"));
        urls.push(format!("{base}/.well-known/oauth-authorization-server{path}"));
        urls.push(format!("{base}/.well-known/oauth-authorization-server"));
        urls.push(format!("{base}/.well-known/openid-configuration{path}"));
        urls.push(format!("{base}/.well-known/openid-configuration"));
        urls.push(format!("{base}{path}/.well-known/openid-configuration"));
    } else {
        urls.push(format!("{base}/.well-known/oauth-protected-resource"));
        urls.push(format!("{base}/.well-known/oauth-authorization-server"));
        urls.push(format!("{base}/.well-known/openid-configuration"));
    }

    // Deduplicate preserving insertion order
    let mut deduped = Vec::new();
    for u in urls {
        if !deduped.contains(&u) {
            deduped.push(u);
        }
    }
    deduped
}

/// Discovers OAuth 2.0 metadata from a server URL using RFC 8414 candidate URLs.
pub async fn discover_oauth_metadata(
    client: &reqwest::Client,
    server_url: &str,
) -> anyhow::Result<OAuthMetadata> {
    discover_oauth_metadata_inner(client, server_url, 0).await
}

async fn discover_oauth_metadata_inner(
    client: &reqwest::Client,
    server_url: &str,
    depth: u8,
) -> anyhow::Result<OAuthMetadata> {
    let base_parsed = validate_oauth_url(server_url)?;
    let parent_is_loopback = base_parsed.host_str().is_some_and(is_loopback_host);
    validate_oauth_target_dns(parent_is_loopback, &base_parsed).await?;
    let urls = build_discovery_urls(server_url);

    for url in &urls {
        let Ok(candidate_url) = validate_oauth_url(url) else {
            continue;
        };
        if is_disallowed_discovery_redirect(&base_parsed, &candidate_url)
            || validate_oauth_target_dns(parent_is_loopback, &candidate_url)
                .await
                .is_err()
        {
            continue;
        }
        let resp = match tokio::time::timeout(
            OAUTH_HTTP_TIMEOUT,
            client
                .get(url)
                .header("Accept", "application/json")
                .send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => continue,
        };

        if !resp.status().is_success() {
            continue;
        }

        let body_bytes = match read_bounded_oauth_bytes(resp, MAX_OAUTH_BODY_BYTES).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let body_val: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // If protected resource metadata points to authorization servers, follow at most 1 hop safely
        if depth < MAX_OAUTH_DISCOVERY_DEPTH {
            if let Some(auth_servers) = body_val
                .get("authorization_servers")
                .and_then(|v| v.as_array())
            {
                for auth_srv in auth_servers {
                    if let Some(srv_str) = auth_srv.as_str() {
                        if let Ok(srv_url) = validate_oauth_url(srv_str) {
                            if is_disallowed_discovery_redirect(&base_parsed, &srv_url)
                                || validate_oauth_target_dns(parent_is_loopback, &srv_url)
                                    .await
                                    .is_err()
                            {
                                continue;
                            }
                            if let Ok(meta) = Box::pin(discover_oauth_metadata_inner(
                                client,
                                srv_str,
                                depth + 1,
                            ))
                            .await
                            {
                                return Ok(meta);
                            }
                        }
                    }
                }
            }
        }

        // Direct metadata check
        let auth_ep = body_val
            .get("authorization_endpoint")
            .and_then(|v| v.as_str());
        let token_ep = body_val.get("token_endpoint").and_then(|v| v.as_str());

        if let (Some(auth_endpoint), Some(token_endpoint)) = (auth_ep, token_ep) {
            let Ok(auth_url) = validate_oauth_url(auth_endpoint) else {
                continue;
            };
            let Ok(token_url) = validate_oauth_url(token_endpoint) else {
                continue;
            };
            if is_disallowed_discovery_redirect(&base_parsed, &auth_url)
                || is_disallowed_discovery_redirect(&base_parsed, &token_url)
                || validate_oauth_target_dns(parent_is_loopback, &auth_url)
                    .await
                    .is_err()
                || validate_oauth_target_dns(parent_is_loopback, &token_url)
                    .await
                    .is_err()
            {
                continue;
            }
            let registration_endpoint = match body_val
                .get("registration_endpoint")
                .and_then(|v| v.as_str())
            {
                Some(ep) => match validate_oauth_url(ep) {
                    Ok(u)
                        if !is_disallowed_discovery_redirect(&base_parsed, &u)
                            && validate_oauth_target_dns(parent_is_loopback, &u)
                                .await
                                .is_ok() =>
                    {
                        Some(ep.to_string())
                    }
                    _ => None,
                },
                None => None,
            };
            let issuer = body_val
                .get("issuer")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let scopes_supported = body_val
                .get("scopes_supported")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(|str_v| str_v.to_string()))
                        .collect()
                });
            let code_challenge_methods_supported = body_val
                .get("code_challenge_methods_supported")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(|str_v| str_v.to_string()))
                        .collect()
                });

            return Ok(OAuthMetadata {
                issuer,
                authorization_endpoint: auth_endpoint.to_string(),
                token_endpoint: token_endpoint.to_string(),
                registration_endpoint,
                scopes_supported,
                code_challenge_methods_supported,
            });
        }
    }

    anyhow::bail!("Failed to discover OAuth metadata for '{server_url}'")
}

/// Registers a dynamic OAuth 2.0 client via RFC 7591.
pub async fn register_dynamic_client(
    client: &reqwest::Client,
    metadata: &OAuthMetadata,
    redirect_uri: &str,
    client_name: &str,
) -> anyhow::Result<(String, Option<String>)> {
    let reg_endpoint = metadata
        .registration_endpoint
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata does not provide a registration_endpoint"))?;
    validate_oauth_request_url(reg_endpoint).await?;

    let payload = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native"
    });

    let resp = tokio::time::timeout(
        OAUTH_HTTP_TIMEOUT,
        client
            .post(reg_endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&payload)
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out registering OAuth client at '{reg_endpoint}'"))?
    .with_context(|| format!("Failed to send dynamic client registration to '{reg_endpoint}'"))?;

    let status = resp.status();
    if !status.is_success() {
        let err_bytes = read_bounded_oauth_bytes(resp, MAX_OAUTH_ERROR_BYTES)
            .await
            .unwrap_or_default();
        let err_text = String::from_utf8_lossy(&err_bytes);
        anyhow::bail!(
            "Dynamic client registration failed ({status}): {}",
            redact_oauth_error_body(&err_text, &[])
        );
    }

    let body_bytes = read_bounded_oauth_bytes(resp, MAX_OAUTH_BODY_BYTES).await?;
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)?;
    let client_id = body
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Registration response missing 'client_id'"))?
        .to_string();

    let client_secret = body
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok((client_id, client_secret))
}

/// Generates an RFC 7636 PKCE S256 verifier and challenge.
pub fn generate_mcp_pkce() -> (String, String) {
    let u1 = uuid::Uuid::new_v4();
    let u2 = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(u1.as_bytes());
    bytes[16..].copy_from_slice(u2.as_bytes());

    let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    (code_verifier, code_challenge)
}

/// Decodes one ASCII hex nibble, or `None` for a non-hex byte.
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes an application/x-www-form-urlencoded component (`+` and `%XX`).
fn percent_decode_form(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                // Decode both nibbles from raw bytes; slicing `value` at byte
                // offsets is not guaranteed to land on UTF-8 char boundaries.
                match (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_callback_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<String> {
    let mut buf = [0u8; 8192];
    let mut total = 0;
    while total < buf.len() {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf[total..])).await??;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf[..total]).into_owned())
}

async fn write_callback_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Runs a local loopback HTTP callback server on a pre-bound listener awaiting authorization callback.
/// The caller owns the bind so the port cannot be stolen between binding and listening.
/// Ignores speculative connections and unrelated paths, and never accepts a callback with a
/// mismatched `state`.
pub async fn run_loopback_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    tokio::time::timeout(timeout, async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let request_str = match tokio::time::timeout(
                Duration::from_secs(5),
                read_callback_request(&mut stream),
            )
            .await
            {
                Ok(Ok(s)) => s,
                _ => continue, // preconnect socket with no request; keep waiting
            };
            let request_line = request_str.lines().next().unwrap_or("");
            let path_and_query = request_line.split_whitespace().nth(1).unwrap_or("");
            let query_str = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");

            let mut params: HashMap<String, String> = HashMap::new();
            for pair in query_str.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    params.insert(percent_decode_form(k), percent_decode_form(v));
                }
            }

            if params.get("state").map(String::as_str) != Some(expected_state) {
                write_callback_response(&mut stream, "400 Bad Request", "OAuth state mismatch").await;
                continue;
            }
            if let Some(error) = params.get("error") {
                let desc = params.get("error_description").map(String::as_str).unwrap_or("");
                let body = format!("OAuth authorization failed: {error} {desc}");
                write_callback_response(&mut stream, "400 Bad Request", &body).await;
                anyhow::bail!("OAuth callback error: {error} - {desc}");
            }
            let Some(auth_code) = params.get("code").cloned() else {
                write_callback_response(&mut stream, "400 Bad Request", "Missing authorization code").await;
                continue;
            };

            let html = "<!DOCTYPE html><html><body style=\"font-family: sans-serif; text-align: center; padding: 40px;\">\
                        <h2>Authentication Successful</h2><p>You can close this tab and return to the application.</p>\
                        </body></html>";
            let ok_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            let _ = stream.write_all(ok_response.as_bytes()).await;
            let _ = stream.flush().await;
            return Ok(auth_code);
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Loopback OAuth callback timed out after {}s", timeout.as_secs()))?
}

#[tokio::test]
    async fn test_loopback_callback_ignores_preconnect_and_decodes_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            run_loopback_callback(listener, "st%2Fate", Duration::from_secs(5)).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Speculative connection with no request at all.
        let pre = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        drop(pre);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let request = "GET /callback?code=abc%2Fdef%2Bghi%3D&state=st%252Fate HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();
        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "abc/def+ghi=");
    }

/// Refreshes OAuth 2.0 tokens using a refresh token grant against the token endpoint.
pub async fn refresh_mcp_tokens(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> anyhow::Result<OAuthTokens> {
    validate_oauth_request_url(token_endpoint).await?;
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    if let Some(secret) = client_secret {
        params.push(("client_secret", secret));
    }

    let resp = tokio::time::timeout(
        OAUTH_HTTP_TIMEOUT,
        client
            .post(token_endpoint)
            .form(&params)
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out refreshing OAuth token at '{token_endpoint}'"))?
    .with_context(|| format!("Failed to POST refresh token request to '{token_endpoint}'"))?;

    let status = resp.status();
    if !status.is_success() {
        let err_bytes = read_bounded_oauth_bytes(resp, MAX_OAUTH_ERROR_BYTES)
            .await
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&err_bytes);
        let mut secrets = vec![refresh_token];
        if let Some(secret) = client_secret {
            secrets.push(secret);
        }
        anyhow::bail!(
            "OAuth token refresh failed ({status}): {}",
            redact_oauth_error_body(&body, &secrets)
        );
    }

    let body_bytes = read_bounded_oauth_bytes(resp, MAX_OAUTH_BODY_BYTES).await?;
    let tokens: OAuthTokens = serde_json::from_slice(&body_bytes)?;
    Ok(tokens)
}

/// Trait providing Bearer token resolution and 401 challenge handling for MCP transports.
#[async_trait]
pub trait McpBearerProvider: Send + Sync {
    /// Returns the active access token, performing proactive refresh if near expiration.
    async fn bearer_token(&self) -> anyhow::Result<Option<String>>;

    /// Invoked when the transport encounters an HTTP 401 Unauthorized.
    /// `failed_token` is the token that produced the 401, used to avoid a
    /// thundering herd of refreshes when several requests fail together.
    /// Returns `Ok(true)` if token was refreshed and request should be retried.
    async fn on_unauthorized(
        &self,
        failed_token: Option<&str>,
        www_authenticate: Option<&str>,
    ) -> anyhow::Result<bool>;
}

/// Bearer provider implementation backed by `McpAuthStore` with single-flight mutex on 401.
pub struct StoredMcpBearerProvider {
    auth_store: Arc<McpAuthStore>,
    server_name: String,
    server_url: String,
    client: reqwest::Client,
    token_endpoint: Arc<RwLock<Option<String>>>,
    refresh_lock: Mutex<()>,
}

impl StoredMcpBearerProvider {
    /// Creates a bearer provider backed by the given auth store.
    pub fn new(
        auth_store: Arc<McpAuthStore>,
        server_name: impl Into<String>,
        server_url: impl Into<String>,
        token_endpoint: Option<String>,
    ) -> Self {
        let client = oauth_http_client();
        Self {
            auth_store,
            server_name: server_name.into(),
            server_url: server_url.into(),
            client,
            token_endpoint: Arc::new(RwLock::new(token_endpoint)),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Returns the refreshed access token when a refresh was performed.
    async fn do_refresh(&self, entry: &McpAuthEntry) -> anyhow::Result<Option<String>> {
        let refresh_token = match entry
            .tokens
            .as_ref()
            .and_then(|t| t.refresh_token.as_ref())
        {
            Some(rt) => rt.clone(),
            None => return Ok(None),
        };

        let client_id = match entry.client_id.as_ref() {
            Some(cid) => cid.clone(),
            None => return Ok(None),
        };

        let endpoint = {
            let ep_guard = self.token_endpoint.read().await;
            ep_guard.clone()
        };

        let endpoint = match endpoint {
            Some(ep) => ep,
            None => {
                let meta = discover_oauth_metadata(&self.client, &self.server_url).await?;
                let mut ep_guard = self.token_endpoint.write().await;
                *ep_guard = Some(meta.token_endpoint.clone());
                meta.token_endpoint
            }
        };

        let mut new_tokens = refresh_mcp_tokens(
            &self.client,
            &endpoint,
            &client_id,
            entry.client_secret.as_deref(),
            &refresh_token,
        )
        .await?;
        // RFC 6749 section 6: the authorization server MAY omit refresh_token.
        // Reuse the previous one when that happens, otherwise the client is locked out.
        if new_tokens.refresh_token.is_none() {
            new_tokens.refresh_token = Some(refresh_token.clone());
        }
        let expire_at = new_tokens.expires_in.map(mcp_token_expiry_seconds);
        let new_access_token = new_tokens.access_token.clone();

        let mut updated = entry.clone();
        updated.tokens = Some(new_tokens);
        updated.tokens_expire_at = expire_at;

        self.auth_store
            .set(&self.server_name, &self.server_url, updated)
            .await?;
        Ok(Some(new_access_token))
    }
}

#[async_trait]
impl McpBearerProvider for StoredMcpBearerProvider {
    async fn bearer_token(&self) -> anyhow::Result<Option<String>> {
        let entry = match self.auth_store.get(&self.server_name, &self.server_url).await {
            Some(e) => e,
            None => return Ok(None),
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Fast path: not near expiry, the single load above is sufficient.
        if !entry.tokens_expire_at.is_some_and(|exp| exp <= now + 30) {
            return Ok(entry.tokens.map(|t| t.access_token));
        }

        // Near expiry: serialize refreshes and re-read the store under the
        // lock so a refresh already performed by another task/process is seen
        // instead of reusing a stale entry (and its stale refresh token).
        let _guard = self.refresh_lock.lock().await;
        let fresh = match self.auth_store.get(&self.server_name, &self.server_url).await {
            Some(e) => e,
            None => return Ok(None),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if !fresh.tokens_expire_at.is_some_and(|exp| exp <= now + 30) {
            return Ok(fresh.tokens.map(|t| t.access_token));
        }
        if let Ok(Some(refreshed)) = self.do_refresh(&fresh).await {
            return Ok(Some(refreshed));
        }
        // Refresh failed (e.g. another caller rotated the refresh token).
        // Re-read once more so a concurrent refresh becomes visible rather
        // than returning the expired access token from `fresh`.
        Ok(self
            .auth_store
            .get(&self.server_name, &self.server_url)
            .await
            .and_then(|e| e.tokens.map(|t| t.access_token)))
    }

    async fn on_unauthorized(
        &self,
        failed_token: Option<&str>,
        _www_authenticate: Option<&str>,
    ) -> anyhow::Result<bool> {
        let _guard = self.refresh_lock.lock().await;
        let entry = match self.auth_store.get(&self.server_name, &self.server_url).await {
            Some(e) => e,
            None => return Ok(false),
        };
        // Another request may already have refreshed the token while we waited on the lock.
        if let (Some(failed), Some(current)) = (
            failed_token,
            entry.tokens.as_ref().map(|t| t.access_token.as_str()),
        ) {
            if current != failed {
                return Ok(true);
            }
        }
        Ok(self.do_refresh(&entry).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_discovery_urls_with_path() {
        let urls = build_discovery_urls("https://example.com/api/mcp");
        assert!(urls.contains(&"https://example.com/.well-known/oauth-protected-resource/api/mcp".to_string()));
        assert!(urls.contains(&"https://example.com/.well-known/oauth-protected-resource".to_string()));
        assert!(urls.contains(&"https://example.com/.well-known/oauth-authorization-server/api/mcp".to_string()));
        assert!(urls.contains(&"https://example.com/.well-known/oauth-authorization-server".to_string()));
        assert!(urls.contains(&"https://example.com/.well-known/openid-configuration/api/mcp".to_string()));
        assert!(urls.contains(&"https://example.com/.well-known/openid-configuration".to_string()));
        assert!(urls.contains(&"https://example.com/api/mcp/.well-known/openid-configuration".to_string()));
    }

    #[test]
    fn test_build_discovery_urls_root() {
        let urls = build_discovery_urls("https://example.com");
        assert_eq!(
            urls,
            vec![
                "https://example.com/.well-known/oauth-protected-resource",
                "https://example.com/.well-known/oauth-authorization-server",
                "https://example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn test_generate_mcp_pkce_properties() {
        let (verifier, challenge) = generate_mcp_pkce();
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);

        // Verify challenge is base64url(sha256(verifier))
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge, expected_challenge);
    }

    #[tokio::test]
    async fn test_mcp_auth_store_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!(
            "mehh_auth_test_{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let path = temp_dir.join("mcp-auth.json");
        let store = McpAuthStore::try_with_path(path.clone()).unwrap();

        let entry = McpAuthEntry {
            server_url: "https://example.com/mcp".to_string(),
            client_id: Some("client-123".to_string()),
            client_secret: Some("secret-456".to_string()),
            tokens: Some(OAuthTokens {
                access_token: "token-abc".to_string(),
                refresh_token: Some("refresh-def".to_string()),
                expires_in: Some(3600),
                token_type: Some("Bearer".to_string()),
                scope: Some("read write".to_string()),
            }),
            tokens_expire_at: Some(1800000000),
        };

        store.set("test_server", "https://example.com/mcp", entry.clone()).await.unwrap();

        let retrieved = store.get("test_server", "https://example.com/mcp").await;
        assert!(retrieved.is_some());
        let r = retrieved.unwrap();
        assert_eq!(r.client_id.as_deref(), Some("client-123"));
        assert_eq!(r.tokens.unwrap().access_token, "token-abc");

        // Verify persistence from new store reading same path
        let store2 = McpAuthStore::try_with_path(path).unwrap();
        let retrieved2 = store2.get("test_server", "https://example.com/mcp").await;
        assert!(retrieved2.is_some());

        // Remove
        let removed = store.remove("test_server", "https://example.com/mcp").await.unwrap();
        assert!(removed.is_some());
        assert!(store.get("test_server", "https://example.com/mcp").await.is_none());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_validate_oauth_url_rules() {
        assert!(validate_oauth_url("https://auth.example.com/token").is_ok());
        assert!(validate_oauth_url("http://localhost:8080/cb").is_ok());
        assert!(validate_oauth_url("http://127.0.0.1:8080/cb").is_ok());
        assert!(validate_oauth_url("http://[::1]:8080/cb").is_ok());
        assert!(validate_oauth_url("http://example.com/token").is_err());
        assert!(validate_oauth_url("file:///etc/passwd").is_err());

        let pub_parent = validate_oauth_url("https://mcp.example.com/mcp").unwrap();
        let local_target = validate_oauth_url("http://127.0.0.1:9000/").unwrap();
        let mapped_loopback = validate_oauth_url("https://[::ffff:127.0.0.1]/token").unwrap();
        let mapped_metadata = validate_oauth_url("https://[::ffff:169.254.169.254]/token").unwrap();
        let cgnat_target = validate_oauth_url("https://100.64.0.1/token").unwrap();
        let mapped_cgnat = validate_oauth_url("https://[::ffff:100.64.0.1]/token").unwrap();
        let sub_localhost = validate_oauth_url("https://meta.localhost/token").unwrap();
        let dot_localhost = validate_oauth_url("https://sub.LOCALHOST./token").unwrap();
        let pub_target = validate_oauth_url("https://auth.example.com/").unwrap();
        assert!(is_disallowed_discovery_redirect(&pub_parent, &local_target));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &mapped_loopback));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &mapped_metadata));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &cgnat_target));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &mapped_cgnat));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &sub_localhost));
        assert!(is_disallowed_discovery_redirect(&pub_parent, &dot_localhost));
        assert!(!is_disallowed_discovery_redirect(&pub_parent, &pub_target));
        let _no_redirect_client = oauth_http_client();
    }

    #[tokio::test]
    async fn test_validate_oauth_request_url_rejects_local_and_cgnat() {
        assert!(validate_oauth_request_url("https://100.64.0.1/token")
            .await
            .is_err());
        assert!(validate_oauth_request_url("https://[::ffff:169.254.169.254]/token")
            .await
            .is_err());
        assert!(validate_oauth_request_url("https://meta.localhost/token")
            .await
            .is_err());
        assert!(validate_oauth_request_url("http://127.0.0.1:8080/callback")
            .await
            .is_ok());
    }

    #[test]
    fn test_mcp_auth_store_fails_closed_on_corrupt_json_and_strips_bom() {
        let dir = crate::TempTestDir::new("oauth_store");
        let bad_path = dir.path().join("bad.json");
        std::fs::write(&bad_path, "{ not json").unwrap();
        assert!(McpAuthStore::try_with_path(bad_path).is_err());

        let bom_path = dir.path().join("bom.json");
        std::fs::write(&bom_path, "\u{feff}{}").unwrap();
        let ok_store = McpAuthStore::try_with_path(bom_path).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(ok_store.list()).is_empty());
    }

    #[tokio::test]
    async fn test_run_loopback_callback_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            run_loopback_callback(listener, "state_expected_123", Duration::from_secs(5)).await
        });

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let request = "GET /callback?code=test_code_auth&state=state_expected_123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let resp_str = String::from_utf8_lossy(&response);
        assert!(resp_str.starts_with("HTTP/1.1 200 OK"));

        let code_res = server_task.await.unwrap();
        assert_eq!(code_res.unwrap(), "test_code_auth");
    }

    #[tokio::test]
    async fn test_run_loopback_callback_decodes_non_ascii_query() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            run_loopback_callback(listener, "state", Duration::from_secs(5)).await
        });

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        // A raw non-ASCII byte right after a `%` used to make the decoder slice
        // the query &str at a non-char boundary and panic. The trailing "%aé"
        // exercises that case; the leading escapes verify normal decoding.
        let request = "GET /callback?code=caf%C3%A9%2F%aé&state=state HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();

        let code = server_task.await.unwrap().unwrap();
        assert_eq!(code, "café/%aé");
    }

    #[tokio::test]
    async fn mcp_auth_store_lock_preserves_concurrent_server_writes() {
        let dir = crate::TempTestDir::new("mcp_concurrent_writes");
        let auth_path = dir.path().join("mcp-auth.json");

        let store1 = McpAuthStore::try_with_path(auth_path.clone()).unwrap();
        let store2 = McpAuthStore::try_with_path(auth_path.clone()).unwrap();

        let t1 = tokio::spawn(async move {
            let entry1 = McpAuthEntry {
                server_url: "http://server1.local".to_string(),
                client_id: Some("client1".to_string()),
                client_secret: None,
                tokens: None,
                tokens_expire_at: None,
            };
            store1.set("server1", "http://server1.local", entry1).await.unwrap();
        });

        let t2 = tokio::spawn(async move {
            let entry2 = McpAuthEntry {
                server_url: "http://server2.local".to_string(),
                client_id: Some("client2".to_string()),
                client_secret: None,
                tokens: None,
                tokens_expire_at: None,
            };
            store2.set("server2", "http://server2.local", entry2).await.unwrap();
        });

        t1.await.unwrap();
        t2.await.unwrap();

        let verify_store = McpAuthStore::try_with_path(auth_path).unwrap();
        let list = verify_store.list().await;
        assert_eq!(list.len(), 2, "Expected both concurrent writes to be retained in store without clobbering");
        assert!(list.contains_key(&McpAuthStore::store_key("server1", "http://server1.local")));
        assert!(list.contains_key(&McpAuthStore::store_key("server2", "http://server2.local")));
    }

    #[tokio::test]
    async fn test_store_key_collision_separates_server_tokens() {
        let dir = crate::TempTestDir::new("oauth_key_collision");
        let store = McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap();

        let key_a = McpAuthStore::store_key("a|b", "c");
        let key_b = McpAuthStore::store_key("a", "b|c");
        assert_ne!(
            key_a, key_b,
            "distinct server identities must not map to the same store key"
        );

        let entry_a = McpAuthEntry {
            server_url: "c".to_string(),
            client_id: None,
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "token_for_a_b".to_string(),
                refresh_token: None,
                expires_in: None,
                token_type: None,
                scope: None,
            }),
            tokens_expire_at: None,
        };
        let entry_b = McpAuthEntry {
            server_url: "b|c".to_string(),
            client_id: None,
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "token_for_a".to_string(),
                refresh_token: None,
                expires_in: None,
                token_type: None,
                scope: None,
            }),
            tokens_expire_at: None,
        };

        store.set("a|b", "c", entry_a).await.unwrap();
        store.set("a", "b|c", entry_b).await.unwrap();

        let got_a = store.get("a|b", "c").await.unwrap();
        let got_b = store.get("a", "b|c").await.unwrap();
        assert_eq!(got_a.tokens.unwrap().access_token, "token_for_a_b");
        assert_eq!(got_b.tokens.unwrap().access_token, "token_for_a");
    }

    #[tokio::test]
    async fn mcp_oauth_tokens_expire_at_unit_triggers_proactive_refresh_after_expiry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let refresh_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rc = refresh_count.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("POST /token"), "unexpected request: {req}");
            rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = serde_json::json!({
                "access_token": "refreshed-access-token",
                "refresh_token": "rotated-refresh-token",
                "expires_in": 3600u64,
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        // Build the entry exactly as mcp_manager.rs does.
        let tokens = OAuthTokens {
            access_token: "expired-access-token".to_string(),
            refresh_token: Some("original-refresh-token".to_string()),
            expires_in: Some(0),
            token_type: Some("Bearer".to_string()),
            scope: None,
        };
        let expires_at = tokens.expires_in.map(mcp_token_expiry_seconds);
        let entry = McpAuthEntry {
            server_url: "https://example.test/mcp".to_string(),
            client_id: Some("client-123".to_string()),
            client_secret: None,
            tokens: Some(tokens),
            tokens_expire_at: expires_at,
        };

        let dir = crate::TempTestDir::new("oauth_expiry_units");
        let store = Arc::new(
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap(),
        );
        store
            .set("srv", "https://example.test/mcp", entry)
            .await
            .unwrap();

        let provider = StoredMcpBearerProvider::new(
            store,
            "srv",
            "https://example.test/mcp",
            Some(format!("http://127.0.0.1:{port}/token")),
        );
        let token = provider.bearer_token().await.unwrap();
        server.abort();

        assert_eq!(
            refresh_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "proactive refresh must fire when tokens_expire_at is in seconds"
        );
        assert_eq!(token.as_deref(), Some("refreshed-access-token"));
    }

    #[tokio::test]
    async fn test_bearer_token_loads_auth_store_once() {
        use std::sync::atomic::Ordering;

        let dir = crate::TempTestDir::new("oauth_bearer_single_load");
        let store = Arc::new(
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap(),
        );
        let entry = McpAuthEntry {
            server_url: "https://example.com/mcp".to_string(),
            client_id: Some("client-id".to_string()),
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "valid-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_in: Some(3600),
                token_type: None,
                scope: None,
            }),
            tokens_expire_at: None,
        };
        store
            .set("srv", "https://example.com/mcp", entry)
            .await
            .unwrap();
        let loads_after_setup = store.load_count.load(Ordering::SeqCst);

        let provider = StoredMcpBearerProvider::new(
            store.clone(),
            "srv",
            "https://example.com/mcp",
            None,
        );
        let token = provider.bearer_token().await.unwrap();
        assert_eq!(token.as_deref(), Some("valid-token"));
        assert_eq!(
            store.load_count.load(Ordering::SeqCst) - loads_after_setup,
            1,
            "a successful bearer_token() must load the auth store exactly once"
        );
    }

    #[tokio::test]
    async fn test_bearer_token_rereads_store_under_refresh_lock() {
        use std::sync::atomic::Ordering;

        let dir = crate::TempTestDir::new("oauth_bearer_reread");
        let store = Arc::new(
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap(),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stale_entry = McpAuthEntry {
            server_url: "https://example.com/mcp".to_string(),
            client_id: Some("client-id".to_string()),
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "expired-token".to_string(),
                refresh_token: Some("old-refresh-token".to_string()),
                expires_in: Some(3600),
                token_type: None,
                scope: None,
            }),
            tokens_expire_at: Some(now - 1), // already expired: forces the refresh path
        };
        store
            .set("srv", "https://example.com/mcp", stale_entry)
            .await
            .unwrap();
        let loads_after_setup = store.load_count.load(Ordering::SeqCst);

        let provider = Arc::new(StoredMcpBearerProvider::new(
            store.clone(),
            "srv",
            "https://example.com/mcp",
            // Loopback port with no listener: if the stale entry is refreshed
            // the request fails fast, which is exactly the rotating-token case.
            Some("http://127.0.0.1:1/token".to_string()),
        ));

        // Hold the refresh lock so bearer_token() loads the stale entry, then
        // blocks before deciding how to refresh it.
        let guard = provider.refresh_lock.lock().await;
        let p = provider.clone();
        let task = tokio::spawn(async move { p.bearer_token().await });

        // Wait until the initial load happened while the lock is still held.
        for _ in 0..100 {
            if store.load_count.load(Ordering::SeqCst) > loads_after_setup {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            store.load_count.load(Ordering::SeqCst),
            loads_after_setup + 1,
            "bearer_token() must have loaded the stale entry and now wait on the refresh lock"
        );

        // Simulate another caller/process refreshing with a rotated refresh
        // token while our caller is blocked on the refresh lock.
        let writer_store =
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap();
        let fresh_entry = McpAuthEntry {
            server_url: "https://example.com/mcp".to_string(),
            client_id: Some("client-id".to_string()),
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "new-access-token".to_string(),
                refresh_token: Some("rotated-refresh-token".to_string()),
                expires_in: Some(3600),
                token_type: None,
                scope: None,
            }),
            tokens_expire_at: Some(now + 3600),
        };
        writer_store
            .set("srv", "https://example.com/mcp", fresh_entry)
            .await
            .unwrap();

        drop(guard);
        let token = task.await.unwrap().unwrap();

        assert_eq!(
            token.as_deref(),
            Some("new-access-token"),
            "a fresher token written under the refresh lock must win over the stale entry"
        );
        assert_eq!(
            store.load_count.load(Ordering::SeqCst) - loads_after_setup,
            2,
            "the near-expiry path must re-read the store under the refresh lock"
        );
    }

    #[tokio::test]
    async fn refresh_mcp_tokens_redacts_echoed_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = "refresh-secret-DO-NOT-LEAK";
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    r#"{{"error":"invalid_grant","error_description":"Invalid refresh_token: {secret}"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let err = refresh_mcp_tokens(
            &reqwest::Client::new(),
            &format!("http://{addr}/token"),
            "client-id",
            None,
            secret,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(secret), "secret leaked into error: {msg}");
        assert!(msg.contains("[REDACTED]"), "expected redaction marker: {msg}");
    }

    #[tokio::test]
    async fn register_dynamic_client_redacts_echoed_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = "registration-secret-DO-NOT-LEAK";
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    r#"{{"error":"invalid_client_metadata","client_secret":"{secret}"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let metadata = OAuthMetadata {
            issuer: None,
            authorization_endpoint: "https://example.test/authorize".to_string(),
            token_endpoint: "https://example.test/token".to_string(),
            registration_endpoint: Some(format!("http://{addr}/register")),
            scopes_supported: None,
            code_challenge_methods_supported: None,
        };
        let err = register_dynamic_client(
            &reqwest::Client::new(),
            &metadata,
            "http://127.0.0.1/callback",
            "test-client",
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(secret), "secret leaked into error: {msg}");
        assert!(msg.contains("[REDACTED]"), "expected redaction marker: {msg}");
    }

    #[test]
    fn redact_oauth_error_body_redacts_percent_encoded_secret_echoes() {
        let secret = "a/b+c";
        let upper = r#"{"error":"invalid_grant","error_description":"Invalid refresh_token: a%2Fb%2Bc"}"#;
        let redacted = redact_oauth_error_body(upper, &[secret]);
        assert!(!redacted.contains("a%2Fb%2Bc"), "encoded secret leaked: {redacted}");
        assert!(!redacted.contains(secret), "decoded secret leaked: {redacted}");
        assert!(redacted.contains("[REDACTED]"), "expected marker: {redacted}");

        let lower = r#"{"error":"invalid_grant","error_description":"Invalid refresh_token: a%2fb%2bc"}"#;
        let redacted_lower = redact_oauth_error_body(lower, &[secret]);
        assert!(
            !redacted_lower.contains("a%2fb%2bc"),
            "lowercase encoded secret leaked: {redacted_lower}"
        );
        assert!(redacted_lower.contains("[REDACTED]"), "expected marker: {redacted_lower}");

        let without_known_secret = redact_oauth_error_body(upper, &[]);
        assert!(
            without_known_secret.contains(r#""error_description":"[REDACTED]""#),
            "token-bearing error_description must be redacted by key: {without_known_secret}"
        );
    }

    #[test]
    fn mcp_token_expiry_seconds_saturates_on_extreme_expires_in() {
        let now = mehh_ai::now_ms() / 1000;
        let expiry = mcp_token_expiry_seconds(u64::MAX);
        assert_eq!(
            expiry,
            i64::MAX,
            "extreme expires_in must saturate, not wrap into the past"
        );
        assert!(expiry > now, "stored expiry must stay in the future");
    }

    #[tokio::test]
    async fn refresh_with_extreme_expires_in_does_not_cause_repeated_refreshes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let refresh_count = Arc::new(AtomicUsize::new(0));
        let rc = refresh_count.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                rc.fetch_add(1, Ordering::SeqCst);
                let body = serde_json::json!({
                    "access_token": "refreshed-access-token",
                    "refresh_token": "rotated-refresh-token",
                    "expires_in": u64::MAX,
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let dir = crate::TempTestDir::new("oauth_extreme_expires_in");
        let store = Arc::new(
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap(),
        );
        let entry = McpAuthEntry {
            server_url: "https://example.test/mcp".to_string(),
            client_id: Some("client-123".to_string()),
            client_secret: None,
            tokens: Some(OAuthTokens {
                access_token: "expired-access-token".to_string(),
                refresh_token: Some("original-refresh-token".to_string()),
                expires_in: Some(0),
                token_type: Some("Bearer".to_string()),
                scope: None,
            }),
            tokens_expire_at: Some(0),
        };
        store
            .set("srv", "https://example.test/mcp", entry)
            .await
            .unwrap();

        let provider = StoredMcpBearerProvider::new(
            store.clone(),
            "srv",
            "https://example.test/mcp",
            Some(format!("http://127.0.0.1:{port}/token")),
        );
        let first = provider.bearer_token().await.unwrap();
        let second = provider.bearer_token().await.unwrap();
        server.abort();

        assert_eq!(first.as_deref(), Some("refreshed-access-token"));
        assert_eq!(second.as_deref(), Some("refreshed-access-token"));
        assert_eq!(
            refresh_count.load(Ordering::SeqCst),
            1,
            "a far-future expiry from the token endpoint must prevent a second refresh"
        );
        let stored = store.get("srv", "https://example.test/mcp").await.unwrap();
        assert_eq!(
            stored.tokens_expire_at,
            Some(i64::MAX),
            "extreme expires_in must be stored as a saturated future expiry"
        );
    }
}
