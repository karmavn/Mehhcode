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

/// Builds an HTTP client for OAuth requests with timeouts and HTTP redirects disabled.
pub fn oauth_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(OAUTH_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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

/// Persistent store for MCP OAuth credentials keyed by `format!("{server_name}|{server_url}")`.
pub struct McpAuthStore {
    path: PathBuf,
    lock_path: PathBuf,
    process_lock: Arc<tokio::sync::Mutex<()>>,
    entries: RwLock<HashMap<String, McpAuthEntry>>,
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
        })
    }

    /// Generates key formatted as `server_name|server_url`.
    pub fn store_key(server_name: &str, server_url: &str) -> String {
        format!("{server_name}|{server_url}")
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

    fn save_internal(&self, map: &HashMap<String, McpAuthEntry>) -> anyhow::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            anyhow::anyhow!("MCP auth store path '{}' has no parent directory", self.path.display())
        })?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create directory '{}'", parent.display())
        })?;

        let json = serde_json::to_string_pretty(map)?;
        let file_name = self
            .path
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
            std::fs::rename(&tmp_path, &self.path).with_context(|| {
                format!(
                    "Failed to atomically replace '{}' with '{}'",
                    self.path.display(),
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
    pub async fn get(&self, server_name: &str, server_url: &str) -> Option<McpAuthEntry> {
        let key = Self::store_key(server_name, server_url);
        {
            let map = self.entries.read().await;
            if let Some(entry) = map.get(&key) {
                return Some(entry.clone());
            }
        }
        if let Ok(disk_map) = Self::load_from_path(&self.path) {
            let res = disk_map.get(&key).cloned();
            *self.entries.write().await = disk_map;
            res
        } else {
            None
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
        let mut disk_map = Self::load_from_path(&self.path)?;
        disk_map.insert(key, entry);
        self.save_internal(&disk_map)?;
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
        let mut disk_map = Self::load_from_path(&self.path)?;
        let removed = disk_map.remove(&key);
        if removed.is_some() {
            self.save_internal(&disk_map)?;
        }
        *self.entries.write().await = disk_map;
        Ok(removed)
    }

    /// Returns all stored entries.
    pub async fn list(&self) -> HashMap<String, McpAuthEntry> {
        if let Ok(disk_map) = Self::load_from_path(&self.path) {
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
        anyhow::bail!("Dynamic client registration failed ({status}): {err_text}");
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

/// Runs a local loopback HTTP callback server on `127.0.0.1:{port}` awaiting authorization callback.
pub async fn run_loopback_callback(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("Failed to bind loopback TCP listener on port {port}"))?;

    tokio::time::timeout(timeout, async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 4096];
        let mut total_read = 0;

        while total_read < buf.len() {
            let n = stream.read(&mut buf[total_read..]).await?;
            if n == 0 {
                break;
            }
            total_read += n;
            if buf[..total_read].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let request_str = String::from_utf8_lossy(&buf[..total_read]);
        let request_line = request_str.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let _method = parts.next().unwrap_or("");
        let path_and_query = parts.next().unwrap_or("");

        let query_str = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut code: Option<String> = None;
        let mut state: Option<String> = None;

        for pair in query_str.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "code" {
                    code = Some(v.to_string());
                } else if k == "state" {
                    state = Some(v.to_string());
                }
            }
        }

        if state.as_deref() != Some(expected_state) {
            let err_response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOAuth state mismatch";
            let _ = stream.write_all(err_response.as_bytes()).await;
            let _ = stream.flush().await;
            anyhow::bail!("OAuth callback state mismatch: expected {expected_state}, got {state:?}");
        }

        let auth_code = match code {
            Some(c) => c,
            None => {
                let err_response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nMissing authorization code";
                let _ = stream.write_all(err_response.as_bytes()).await;
                let _ = stream.flush().await;
                anyhow::bail!("OAuth callback did not contain authorization code");
            }
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

        Ok(auth_code)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Loopback OAuth callback timed out after {}s", timeout.as_secs()))?
}

/// Refreshes OAuth 2.0 tokens using a refresh token grant against the token endpoint.
pub async fn refresh_mcp_tokens(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> anyhow::Result<OAuthTokens> {
    validate_oauth_request_url(token_endpoint).await?;
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];

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
        anyhow::bail!("OAuth token refresh failed ({status}): {body}");
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
    /// Returns `Ok(true)` if token was refreshed and request should be retried.
    async fn on_unauthorized(&self, www_authenticate: Option<&str>) -> anyhow::Result<bool>;
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

    async fn do_refresh(&self, entry: &McpAuthEntry) -> anyhow::Result<bool> {
        let refresh_token = match entry
            .tokens
            .as_ref()
            .and_then(|t| t.refresh_token.as_ref())
        {
            Some(rt) => rt.clone(),
            None => return Ok(false),
        };

        let client_id = match entry.client_id.as_ref() {
            Some(cid) => cid.clone(),
            None => return Ok(false),
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

        let new_tokens =
            refresh_mcp_tokens(&self.client, &endpoint, &client_id, &refresh_token).await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let expire_at = new_tokens.expires_in.map(|exp| now + exp as i64);

        let mut updated = entry.clone();
        updated.tokens = Some(new_tokens);
        updated.tokens_expire_at = expire_at;

        self.auth_store
            .set(&self.server_name, &self.server_url, updated)
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl McpBearerProvider for StoredMcpBearerProvider {
    async fn bearer_token(&self) -> anyhow::Result<Option<String>> {
        let entry_opt = self.auth_store.get(&self.server_name, &self.server_url).await;
        let entry = match entry_opt {
            Some(e) => e,
            None => return Ok(None),
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Proactive refresh within 30-second skew window
        if let Some(exp) = entry.tokens_expire_at {
            if exp <= now + 30 {
                let _guard = self.refresh_lock.lock().await;
                if let Some(current) = self.auth_store.get(&self.server_name, &self.server_url).await {
                    if let Some(current_exp) = current.tokens_expire_at {
                        if current_exp <= now + 30 {
                            let _ = self.do_refresh(&current).await;
                        }
                    }
                }
            }
        }

        let final_entry = self.auth_store.get(&self.server_name, &self.server_url).await;
        Ok(final_entry.and_then(|e| e.tokens).map(|t| t.access_token))
    }

    async fn on_unauthorized(&self, _www_authenticate: Option<&str>) -> anyhow::Result<bool> {
        let _guard = self.refresh_lock.lock().await;
        let entry = match self.auth_store.get(&self.server_name, &self.server_url).await {
            Some(e) => e,
            None => return Ok(false),
        };
        self.do_refresh(&entry).await
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
        // Bind ephemeral port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // release so run_loopback_callback can rebind

        let server_task = tokio::spawn(async move {
            run_loopback_callback(port, "state_expected_123", Duration::from_secs(5)).await
        });

        // Give listener brief time to spin up
        tokio::time::sleep(Duration::from_millis(100)).await;

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
        assert!(list.contains_key("server1|http://server1.local"));
        assert!(list.contains_key("server2|http://server2.local"));
    }
}
