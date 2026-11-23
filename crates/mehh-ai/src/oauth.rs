//! OAuth PKCE flow, device code flow, loopback callback listener, and credential storage.

use std::collections::HashMap;
use std::time::Duration;
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::types::now_ms;

/// Stored credential format corresponding to `~/.mehh/agent/auth.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StoredCredential {
    #[serde(rename = "api_key")]
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
    },
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth {
        access: String,
        refresh: String,
        expires: i64,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "clientId", alias = "client_id")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scopes: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "availableModelIds", alias = "available_model_ids")]
        available_model_ids: Option<Vec<String>>,
    },
}

impl StoredCredential {
    /// Returns "api_key" or "oauth" based on credential variant.
    pub fn credential_type(&self) -> &'static str {
        match self {
            StoredCredential::ApiKey { .. } => "api_key",
            StoredCredential::OAuth { .. } => "oauth",
        }
    }
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub credential_type: String,
}

impl CredentialInfo {
    pub fn new(provider_id: impl Into<String>, credential_type: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            credential_type: credential_type.into(),
        }
    }
}

/// A PKCE code verifier and code challenge pair.
#[derive(Debug, Clone, PartialEq)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Generates a PKCE code verifier and S256 code challenge pair.
pub fn generate_pkce() -> PkcePair {
    let u1 = uuid::Uuid::new_v4();
    let u2 = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(u1.as_bytes());
    bytes[16..].copy_from_slice(u2.as_bytes());

    let verifier = BASE64_URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = BASE64_URL_SAFE_NO_PAD.encode(hash);

    PkcePair { verifier, challenge }
}

/// Percent-encodes a string according to RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut res = String::with_capacity(s.len() * 3 / 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                res.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(&mut res, "%{:02X}", b);
            }
        }
    }
    res
}

/// Supported OAuth provider targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProviderKind {
    Anthropic,
    OpenAiChatGpt,
    GitHubCopilot,
}

/// Device code flow challenge returned by GitHub Copilot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceCodeChallenge {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

/// Builds the browser authorization URL for PKCE-based OAuth providers.
pub fn build_authorize_url(
    kind: OAuthProviderKind,
    challenge: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<String> {
    match kind {
        OAuthProviderKind::Anthropic => {
            let client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
            let scopes = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
            Ok(format!(
                "https://claude.ai/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
                client_id,
                percent_encode(redirect_uri),
                percent_encode(scopes),
                percent_encode(challenge),
                percent_encode(state),
            ))
        }
        OAuthProviderKind::OpenAiChatGpt => {
            let client_id = "dynamic_agent_client";
            let scopes = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
            let audience = "https://api.openai.com/v1";
            Ok(format!(
                "https://auth.openai.com/api/accounts/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&audience={}",
                client_id,
                percent_encode(redirect_uri),
                percent_encode(scopes),
                percent_encode(challenge),
                percent_encode(state),
                percent_encode(audience),
            ))
        }
        OAuthProviderKind::GitHubCopilot => {
            anyhow::bail!("GitHub Copilot uses device code flow, not browser redirect")
        }
    }
}

const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// A preconnect socket may stay open without sending any bytes. Never let one
/// block the real callback behind it.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum characters of a provider error body kept in an error message.
const MAX_OAUTH_ERROR_BODY_CHARS: usize = 512;

/// Converts a provider relative TTL (seconds) into an absolute expiry (ms),
/// saturating instead of overflowing on hostile provider values.
fn expiry_from_ttl_secs(ttl_secs: i64) -> i64 {
    now_ms().saturating_add(ttl_secs.saturating_mul(1000))
}

/// Converts a Copilot `expires_at` (seconds) into an expiry (ms) with a
/// five-minute early refresh margin, saturating on hostile provider values.
fn copilot_expiry_from_expires_at(expires_at: i64) -> i64 {
    expires_at.saturating_mul(1000).saturating_sub(300_000)
}

/// Removes credentials from a provider error body and caps its length.
pub(crate) fn redact_error_body(body: &str, secrets: &[&str]) -> String {
    static JSON_SECRET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)("(?:(?:access|refresh|client|api|auth|session)[_-]?(?:token|secret|key)|token|secret|authorization|password)"\s*:\s*)"(?:[^"\\]|\\.)*""#,
        )
        .expect("valid secret-key regex")
    });
    static BEARER_TOKEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+").expect("valid bearer regex")
    });
    static SK_KEY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"sk-[A-Za-z0-9_-]+").expect("valid sk-key regex")
    });

    // Replace exact request secrets first: providers often echo them in free
    // text fields such as `error_description`, which no key-based regex sees.
    let mut redacted = body.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    let redacted = JSON_SECRET.replace_all(&redacted, r#"$1"[REDACTED]""#);
    let redacted = BEARER_TOKEN.replace_all(&redacted, "Bearer [REDACTED]");
    let redacted = SK_KEY.replace_all(&redacted, "[REDACTED]");
    // Truncate last: cutting first can remove the closing quote a regex needs,
    // leaving a secret prefix intact.
    crate::retry::truncate_error_body(&redacted, MAX_OAUTH_ERROR_BODY_CHARS)
}

/// Rejects addresses a refresh token must never be sent to.
fn reject_unsafe_ip(ip: std::net::IpAddr, host: &str) -> anyhow::Result<()> {
    let disallowed = match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
                || o[0] == 0 // "this network" 0.0.0.0/8
                || o[0] >= 240 // reserved / broadcast
        }
        std::net::IpAddr::V6(v6) => {
            let o = v6.octets();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (0xfc..=0xfd).contains(&o[0]) // unique local fc00::/7
                || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80) // link-local fe80::/10
                || v6.to_ipv4().is_some()
        }
    };
    if disallowed {
        anyhow::bail!("OAuth token URL resolves to a disallowed address ({host})");
    }
    Ok(())
}

/// DANGER: `MEHH_OAUTH_ALLOW_INSECURE_REFRESH=1` (or `true`) permits plain http
/// and loopback/private token URLs. This exists solely for local mock servers;
/// it sends refresh tokens unencrypted and is unsafe in production.
fn insecure_refresh_allowed() -> bool {
    std::env::var("MEHH_OAUTH_ALLOW_INSECURE_REFRESH")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// A token URL plus the exact addresses validated for it. The addresses are
/// pinned into the refresh client so `reqwest` cannot resolve the hostname a
/// second time to a different, possibly private, address (DNS rebinding/TOCTOU).
struct ValidatedTokenUrl {
    url: reqwest::Url,
    /// `Some((host, addrs))` for hostname URLs; IP literals connect directly.
    pinned: Option<(String, Vec<std::net::SocketAddr>)>,
}

/// Validates a (possibly env-overridden) token URL before a refresh token is sent to it.
async fn validate_token_url(url: &str) -> anyhow::Result<ValidatedTokenUrl> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("Invalid OAuth token URL: {e}"))?;
    let allow_insecure = insecure_refresh_allowed();
    if parsed.scheme() != "https" && !(allow_insecure && parsed.scheme() == "http") {
        anyhow::bail!("OAuth token URL must use https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("OAuth token URL must not contain userinfo");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("OAuth token URL has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let pinned = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        // A literal is the address we connect to; there is no second resolution.
        if !allow_insecure {
            reject_unsafe_ip(ip, host)?;
        }
        None
    } else {
        let resolved: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to resolve OAuth token URL host '{host}': {e}"))?
            .collect();
        if resolved.is_empty() {
            anyhow::bail!("OAuth token URL host '{host}' did not resolve");
        }
        if !allow_insecure {
            for addr in &resolved {
                reject_unsafe_ip(addr.ip(), host)?;
            }
        }
        Some((host.to_string(), resolved))
    };
    Ok(ValidatedTokenUrl {
        url: parsed,
        pinned,
    })
}

/// Builds the client used for token refreshes. Redirects stay disabled, and a
/// hostname URL is pinned to the addresses that were validated: `resolve_to_addrs`
/// keeps the Host header and TLS SNI bound to the original hostname.
fn build_refresh_client(
    pinned: Option<(&str, &[std::net::SocketAddr])>,
) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(OAUTH_HTTP_TIMEOUT);
    if let Some((host, addrs)) = pinned {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build OAuth refresh client: {e}"))
}

async fn send_oauth_request(
    req: reqwest::RequestBuilder,
    secrets: &[&str],
) -> anyhow::Result<reqwest::Response> {
    let resp = tokio::time::timeout(OAUTH_HTTP_TIMEOUT, req.send())
        .await
        .map_err(|_| anyhow::anyhow!("OAuth HTTP request timed out after 30s"))??;
    if resp.status().is_success() {
        return Ok(resp);
    }
    // Keep the provider's explanation, but never echo credentials back to logs.
    // Read only byte-capped here: redact_error_body performs the single final
    // truncation after redaction, so the secret's closing quote must still exist.
    let status = resp.status();
    let body = crate::retry::read_error_body_bytes_capped(resp).await;
    anyhow::bail!("OAuth HTTP {status}: {}", redact_error_body(&body, secrets))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Percent-decodes a string, leaving malformed escapes untouched.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Exchanges an authorization code and PKCE verifier for OAuth access and refresh tokens.
pub async fn exchange_code_for_tokens(
    client: &reqwest::Client,
    kind: OAuthProviderKind,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<StoredCredential> {
    match kind {
        OAuthProviderKind::Anthropic => {
            let token_url = "https://platform.claude.com/v1/oauth/token";
            let mut params = HashMap::new();
            params.insert("grant_type", "authorization_code");
            params.insert("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
            params.insert("code", code);
            params.insert("code_verifier", verifier);
            params.insert("redirect_uri", redirect_uri);

            let res = send_oauth_request(client.post(token_url).form(&params), &[verifier, code]).await?;

            let json: serde_json::Value = res.json().await?;
            let access = json["access_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing access_token in token response"))?
                .to_string();
            let refresh = json["refresh_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing refresh_token in token response"))?
                .to_string();
            let expires_in = json["expires_in"].as_i64().unwrap_or(3600);

            Ok(StoredCredential::OAuth {
                access,
                refresh,
                expires: expiry_from_ttl_secs(expires_in),
                client_id: Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string()),
                scopes: None,
                available_model_ids: None,
            })
        }
        OAuthProviderKind::OpenAiChatGpt => {
            let token_url = "https://auth.openai.com/api/accounts/oauth/token";
            let mut params = HashMap::new();
            params.insert("grant_type", "authorization_code");
            params.insert("client_id", "dynamic_agent_client");
            params.insert("code", code);
            params.insert("code_verifier", verifier);
            params.insert("redirect_uri", redirect_uri);

            let res = send_oauth_request(client.post(token_url).form(&params), &[verifier, code]).await?;

            let json: serde_json::Value = res.json().await?;
            let access = json["access_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing access_token in token response"))?
                .to_string();
            let refresh = json["refresh_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing refresh_token in token response"))?
                .to_string();
            let expires_in = json["expires_in"].as_i64().unwrap_or(3600);
            let client_id = json["client_id"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| Some("dynamic_agent_client".to_string()));

            Ok(StoredCredential::OAuth {
                access,
                refresh,
                expires: expiry_from_ttl_secs(expires_in),
                client_id,
                scopes: None,
                available_model_ids: None,
            })
        }
        OAuthProviderKind::GitHubCopilot => {
            anyhow::bail!("GitHub Copilot uses device code flow, not authorization code exchange")
        }
    }
}

/// Refreshes an expired OAuth credential using its refresh token.
pub async fn refresh_oauth_credential(
    kind: OAuthProviderKind,
    cred: &StoredCredential,
) -> anyhow::Result<StoredCredential> {
    let (refresh_token, client_id_opt, scopes, models) = match cred {
        StoredCredential::OAuth {
            refresh,
            client_id,
            scopes,
            available_model_ids,
            ..
        } => (refresh.clone(), client_id.clone(), scopes.clone(), available_model_ids.clone()),
        _ => anyhow::bail!("Provided credential is not OAuth"),
    };

    match kind {
        OAuthProviderKind::Anthropic => {
            let token_url = std::env::var("ANTHROPIC_TOKEN_URL")
                .unwrap_or_else(|_| "https://platform.claude.com/v1/oauth/token".to_string());
            let ValidatedTokenUrl { url, pinned } = validate_token_url(&token_url).await?;
            let client = build_refresh_client(
                pinned
                    .as_ref()
                    .map(|(host, addrs)| (host.as_str(), addrs.as_slice())),
            )?;
            let mut params = HashMap::new();
            params.insert("grant_type", "refresh_token");
            params.insert("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
            params.insert("refresh_token", &refresh_token);

            let res = send_oauth_request(client.post(url).form(&params), &[refresh_token.as_str()]).await?;

            let json: serde_json::Value = res.json().await?;
            let access = json["access_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing access_token in refresh response"))?
                .to_string();
            let new_refresh = json["refresh_token"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or(refresh_token);
            let expires_in = json["expires_in"].as_i64().unwrap_or(3600);

            Ok(StoredCredential::OAuth {
                access,
                refresh: new_refresh,
                expires: expiry_from_ttl_secs(expires_in),
                client_id: client_id_opt.or_else(|| Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string())),
                scopes,
                available_model_ids: models,
            })
        }
        OAuthProviderKind::OpenAiChatGpt => {
            let token_url = std::env::var("OPENAI_TOKEN_URL")
                .unwrap_or_else(|_| "https://auth.openai.com/api/accounts/oauth/token".to_string());
            let ValidatedTokenUrl { url, pinned } = validate_token_url(&token_url).await?;
            let client = build_refresh_client(
                pinned
                    .as_ref()
                    .map(|(host, addrs)| (host.as_str(), addrs.as_slice())),
            )?;
            let c_id = client_id_opt
                .as_deref()
                .unwrap_or("dynamic_agent_client");
            let mut params = HashMap::new();
            params.insert("grant_type", "refresh_token");
            params.insert("client_id", c_id);
            params.insert("refresh_token", &refresh_token);

            let res = send_oauth_request(client.post(url).form(&params), &[refresh_token.as_str()]).await?;

            let json: serde_json::Value = res.json().await?;
            let access = json["access_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing access_token in refresh response"))?
                .to_string();
            let new_refresh = json["refresh_token"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or(refresh_token);
            let expires_in = json["expires_in"].as_i64().unwrap_or(3600);

            Ok(StoredCredential::OAuth {
                access,
                refresh: new_refresh,
                expires: expiry_from_ttl_secs(expires_in),
                client_id: client_id_opt.or_else(|| Some("dynamic_agent_client".to_string())),
                scopes,
                available_model_ids: models,
            })
        }
        OAuthProviderKind::GitHubCopilot => {
            let client = build_refresh_client(None)?;
            let res = send_oauth_request(
                client
                    .get("https://api.github.com/copilot_internal/v2/token")
                    .header("Authorization", format!("Bearer {refresh_token}"))
                    .header("User-Agent", "GitHubCopilotChat/0.35.0")
                    .header("Editor-Version", "vscode/1.107.0")
                    .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
                    .header("Copilot-Integration-Id", "vscode-chat")
                    .header("Accept", "application/json"),
                &[refresh_token.as_str()],
            )
            .await?;

            let json: serde_json::Value = res.json().await?;
            let token = json["token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing token in Copilot token response"))?
                .to_string();
            let expires_at = json["expires_at"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("Missing expires_at in Copilot token response"))?;

            Ok(StoredCredential::OAuth {
                access: token,
                refresh: refresh_token,
                expires: copilot_expiry_from_expires_at(expires_at),
                client_id: Some("Iv1.b507a08c87ecfe98".to_string()),
                scopes,
                available_model_ids: models,
            })
        }
    }
}

/// Initiates the GitHub Copilot Device Code Flow.
pub async fn start_copilot_device_flow(client: &reqwest::Client) -> anyhow::Result<DeviceCodeChallenge> {
    let mut params = HashMap::new();
    params.insert("client_id", "Iv1.b507a08c87ecfe98");
    params.insert("scope", "read:user");

    let res = send_oauth_request(
        client
            .post("https://github.com/login/device/code")
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .form(&params),
        &[],
    )
    .await?;

    let json: serde_json::Value = res.json().await?;
    let device_code = json["device_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing device_code"))?
        .to_string();
    let user_code = json["user_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing user_code"))?
        .to_string();
    let verification_uri = json["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device")
        .to_string();
    let interval = json["interval"].as_u64().unwrap_or(5);
    let expires_in = json["expires_in"].as_u64().unwrap_or(900);

    Ok(DeviceCodeChallenge {
        device_code,
        user_code,
        verification_uri,
        interval_secs: interval,
        expires_in_secs: expires_in,
    })
}

/// Outcome of a single device-code poll.
#[derive(Debug)]
pub enum CopilotDevicePoll {
    /// Authorization has not been granted yet.
    Pending,
    /// RFC 8628 `slow_down`: the caller must increase its poll interval by 5s.
    SlowDown,
    /// Authorization completed.
    Complete(StoredCredential),
}

/// Polls GitHub for token completion during Copilot Device Code Flow.
pub async fn poll_copilot_device_token(
    client: &reqwest::Client,
    device_code: &str,
) -> anyhow::Result<CopilotDevicePoll> {
    let mut params = HashMap::new();
    params.insert("client_id", "Iv1.b507a08c87ecfe98");
    params.insert("device_code", device_code);
    params.insert("grant_type", "urn:ietf:params:oauth:grant-type:device_code");

    let res = send_oauth_request(
        client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .form(&params),
        &[device_code],
    )
    .await?;

    let json: serde_json::Value = res.json().await?;

    if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
        match err {
            "authorization_pending" => return Ok(CopilotDevicePoll::Pending),
            "slow_down" => return Ok(CopilotDevicePoll::SlowDown),
            other => {
                let desc = json
                    .get("error_description")
                    .and_then(|d| d.as_str())
                    .unwrap_or(other);
                anyhow::bail!("GitHub OAuth device flow error: {desc}");
            }
        }
    }

    let github_token = json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token found in GitHub response"))?
        .to_string();

    // Exchange GitHub token for Copilot internal session token
    let copilot_res = send_oauth_request(
        client
            .get("https://api.github.com/copilot_internal/v2/token")
            .header("Authorization", format!("Bearer {github_token}"))
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .header("Editor-Version", "vscode/1.107.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
            .header("Copilot-Integration-Id", "vscode-chat")
            .header("Accept", "application/json"),
        &[github_token.as_str()],
    )
    .await?;

    let copilot_json: serde_json::Value = copilot_res.json().await?;
    let token = copilot_json["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing token in Copilot token exchange"))?
        .to_string();
    let expires_at = copilot_json["expires_at"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing expires_at in Copilot token exchange"))?;

    Ok(CopilotDevicePoll::Complete(StoredCredential::OAuth {
        access: token,
        refresh: github_token,
        expires: copilot_expiry_from_expires_at(expires_at),
        client_id: Some("Iv1.b507a08c87ecfe98".to_string()),
        scopes: None,
        available_model_ids: None,
    }))
}

/// Listens on the provided loopback listener for an OAuth redirect callback with code and state.
///
/// The caller owns the listener so it can be bound (and its port known) before
/// the browser is launched; ownership is transferred here for the wait.
pub async fn wait_for_loopback_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let port = listener.local_addr().ok().map(|a| a.port());
    tokio::time::timeout(timeout, async {
        // Browsers open preconnect sockets and request /favicon.ico. Ignore
        // anything that is not the OAuth callback instead of failing login.
        loop {
            let (mut socket, _) = listener.accept().await?;
            // A browser request can span several TCP segments; one read() may
            // cut the URL mid-parameter. Accumulate until the header terminator
            // is present, with a hard cap, so a partial request is never parsed.
            const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
            let mut request_buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut complete_request: Option<String> = None;
            loop {
                let n = match tokio::time::timeout(CALLBACK_READ_TIMEOUT, socket.read(&mut chunk)).await {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => return Err(e.into()),
                    // A preconnect socket that sends nothing must not block the
                    // real callback behind it.
                    Err(_) if request_buf.is_empty() => break,
                    Err(_) => anyhow::bail!("Timed out reading OAuth callback request"),
                };
                if n == 0 {
                    break;
                }
                request_buf.extend_from_slice(&chunk[..n]);
                if request_buf.len() > MAX_CALLBACK_REQUEST_BYTES {
                    anyhow::bail!("OAuth callback request exceeds maximum size");
                }
                if let Some(end) = request_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    complete_request =
                        Some(String::from_utf8_lossy(&request_buf[..end]).into_owned());
                    break;
                }
            }
            let Some(request_str) = complete_request else {
                continue;
            };

            let path = request_str
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

            let mut code_opt = None;
            let mut state_opt = None;
            let mut error_opt = None;
            let mut error_desc = None;
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let decoded = percent_decode(v);
                    match k {
                        "code" => code_opt = Some(decoded),
                        "state" => state_opt = Some(decoded),
                        "error" => error_opt = Some(decoded),
                        "error_description" => error_desc = Some(decoded),
                        _ => {}
                    }
                }
            }

            if let Some(error) = error_opt {
                let html = format!(
                    "<!DOCTYPE html><html><body><h1>Authentication Failed</h1><p>{}</p></body></html>",
                    html_escape(&error)
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                match error_desc {
                    Some(desc) => anyhow::bail!("OAuth provider error: {error} - {desc}"),
                    None => anyhow::bail!("OAuth provider error: {error}"),
                }
            }

            if code_opt.is_none() && state_opt.is_none() {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }

            let state = state_opt
                .ok_or_else(|| anyhow::anyhow!("Missing state in callback URL"))?;
            if state != expected_state {
                anyhow::bail!("State mismatch in callback URL");
            }
            let code = code_opt
                .ok_or_else(|| anyhow::anyhow!("Missing code in callback URL"))?;

            let html = "<!DOCTYPE html><html><body><h1>Authentication Successful</h1><p>You can close this tab and return to the application.</p></body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                html.len(),
                html
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;

            return Ok(code);
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Timed out waiting for loopback callback on port {}",
            port.unwrap_or(0)
        )
    })?
}

pub type CredentialModifyFuture<'a> =
    futures::future::BoxFuture<'a, anyhow::Result<Option<StoredCredential>>>;

pub type CredentialModifyFn =
    Box<dyn FnOnce(Option<StoredCredential>) -> CredentialModifyFuture<'static> + Send>;

/// App-owned credential storage, keyed by provider id, one credential per provider.
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential, returning None if missing.
    async fn read(&self, provider_id: &str) -> anyhow::Result<Option<StoredCredential>>;

    /// List stored credential metadata without resolving or exposing secrets.
    async fn list(&self) -> anyhow::Result<Vec<CredentialInfo>>;

    /// Serialized write — the only write path. `modify_fn` receives the current
    /// credential and returns the new credential (or None to leave unchanged).
    async fn modify(
        &self,
        provider_id: &str,
        modify_fn: CredentialModifyFn,
    ) -> anyhow::Result<Option<StoredCredential>>;

    /// Removes a credential (logout).
    async fn delete(&self, provider_id: &str) -> anyhow::Result<()>;
}

/// In-process lock registry keyed by canonical normalized lock path to prevent
/// blocking Tokio runtime worker threads on OS file locks.
static FILE_LOCK_REGISTRY: std::sync::LazyLock<
    std::sync::Mutex<HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Returns or creates an in-process mutex for the given lock file path.
pub fn get_path_lock_mutex(path: &std::path::Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let normalized = normalize_lock_path(path);
    let mut reg = FILE_LOCK_REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    reg.entry(normalized)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Normalizes a path for lock registry keying.
pub fn normalize_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    let p = if let Ok(c) = std::fs::canonicalize(path) {
        c
    } else if let Some(parent) = path.parent() {
        if let Ok(c_parent) = std::fs::canonicalize(parent) {
            if let Some(file_name) = path.file_name() {
                c_parent.join(file_name)
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    #[cfg(windows)]
    {
        std::path::PathBuf::from(p.to_string_lossy().to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        p
    }
}

/// RAII file lock guard holding an open file OS lock and an in-process Tokio mutex guard.
pub struct FileLockGuard {
    file: Option<std::fs::File>,
    _in_process: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        // `unlock` is a non-blocking syscall. Doing it here guarantees the OS
        // lock is released before the in-process mutex guard field is dropped;
        // a detached blocking task would let another caller acquire first.
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

/// Acquires an advisory file lock on `<path>.lock`, first serializing in-process tasks
/// via `tokio::sync::Mutex` without blocking Tokio runtime worker threads, then locking
/// the file inside `tokio::task::spawn_blocking`.
pub async fn acquire_file_lock(
    lock_path: &std::path::Path,
    in_process_mutex: std::sync::Arc<tokio::sync::Mutex<()>>,
) -> anyhow::Result<FileLockGuard> {
    use anyhow::Context;
    let in_process_guard = in_process_mutex.lock_owned().await;

    let path_clone = lock_path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
        if let Some(parent) = path_clone.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for lock file '{}'", parent.display())
            })?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&path_clone)
            .with_context(|| format!("Failed to open lock file '{}'", path_clone.display()))?;
        file.lock()
            .with_context(|| format!("Failed to acquire OS file lock on '{}'", path_clone.display()))?;
        Ok(file)
    })
    .await
    .map_err(|e| anyhow::anyhow!("File lock blocking task failed to join: {e}"))??;

    Ok(FileLockGuard {
        file: Some(file),
        _in_process: Some(in_process_guard),
    })
}

/// In-memory credential store. Writes are serialized per provider id.
#[derive(Debug, Default, Clone)]
pub struct InMemoryCredentialStore {
    credentials: std::sync::Arc<tokio::sync::RwLock<HashMap<String, StoredCredential>>>,
    provider_locks: std::sync::Arc<tokio::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_credentials(initial: HashMap<String, StoredCredential>) -> Self {
        Self {
            credentials: std::sync::Arc::new(tokio::sync::RwLock::new(initial)),
            provider_locks: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider_id: &str) -> anyhow::Result<Option<StoredCredential>> {
        let map = self.credentials.read().await;
        let cred = map.get(provider_id).or_else(|| {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(provider_id))
                .map(|(_, v)| v)
        });
        Ok(cred.cloned())
    }

    async fn list(&self) -> anyhow::Result<Vec<CredentialInfo>> {
        let map = self.credentials.read().await;
        let mut list: Vec<CredentialInfo> = map
            .iter()
            .map(|(k, v)| CredentialInfo::new(k, v.credential_type()))
            .collect();
        list.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        Ok(list)
    }

    async fn modify(
        &self,
        provider_id: &str,
        modify_fn: CredentialModifyFn,
    ) -> anyhow::Result<Option<StoredCredential>> {
        let lock_key = provider_id.to_ascii_lowercase();
        let provider_lock = {
            let mut locks = self.provider_locks.lock().await;
            locks
                .entry(lock_key)
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        let _guard = provider_lock.lock().await;

        let current = self.read(provider_id).await?;
        let next = modify_fn(current.clone()).await?;

        if let Some(ref new_cred) = next {
            let mut map = self.credentials.write().await;
            let existing_key = map
                .keys()
                .find(|k| k.eq_ignore_ascii_case(provider_id))
                .cloned();
            let key = existing_key.unwrap_or_else(|| provider_id.to_string());
            map.insert(key, new_cred.clone());
            Ok(Some(new_cred.clone()))
        } else {
            Ok(current)
        }
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        let lock_key = provider_id.to_ascii_lowercase();
        let provider_lock = {
            let mut locks = self.provider_locks.lock().await;
            locks
                .entry(lock_key)
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        let _guard = provider_lock.lock().await;
        let mut map = self.credentials.write().await;
        let existing_key = map
            .keys()
            .find(|k| k.eq_ignore_ascii_case(provider_id))
            .cloned();
        if let Some(key) = existing_key {
            map.remove(&key);
        } else {
            map.remove(provider_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_generation() {
        let pkce = generate_pkce();
        assert!(pkce.verifier.len() >= 43);
        assert!(!pkce.challenge.is_empty());

        let hash = Sha256::digest(pkce.verifier.as_bytes());
        let expected_challenge = BASE64_URL_SAFE_NO_PAD.encode(hash);
        assert_eq!(pkce.challenge, expected_challenge);
    }

    #[test]
    fn test_percent_encode() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
        assert_eq!(percent_encode("abc-123._~"), "abc-123._~");
    }

    #[test]
    fn test_build_authorize_url() {
        let pkce = generate_pkce();

        let anthropic_url = build_authorize_url(
            OAuthProviderKind::Anthropic,
            &pkce.challenge,
            "test_state",
            "http://localhost:53692/callback",
        )
        .unwrap();
        assert!(anthropic_url.starts_with("https://claude.ai/oauth/authorize"));
        assert!(anthropic_url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(anthropic_url.contains("code_challenge="));
        assert!(anthropic_url.contains("code_challenge_method=S256"));
        assert!(anthropic_url.contains("state=test_state"));

        let openai_url = build_authorize_url(
            OAuthProviderKind::OpenAiChatGpt,
            &pkce.challenge,
            "state_openai",
            "http://127.0.0.1:1455/auth/callback",
        )
        .unwrap();
        assert!(openai_url.starts_with("https://auth.openai.com/api/accounts/authorize"));
        assert!(openai_url.contains("client_id=dynamic_agent_client"));
        assert!(openai_url.contains("audience="));
        assert!(openai_url.contains("state=state_openai"));

        let copilot_err = build_authorize_url(
            OAuthProviderKind::GitHubCopilot,
            &pkce.challenge,
            "state_copilot",
            "http://localhost:1234",
        );
        assert!(copilot_err.is_err());
    }

    #[test]
    fn test_stored_credential_serde_roundtrip() {
        // ApiKey
        let mut env_map = HashMap::new();
        env_map.insert("AWS_PROFILE".to_string(), "default".to_string());
        let cred_api = StoredCredential::ApiKey {
            key: Some("secret-key-123".to_string()),
            env: Some(env_map),
        };
        let serialized_api = serde_json::to_string(&cred_api).unwrap();
        assert!(serialized_api.contains("\"type\":\"api_key\""));
        assert!(serialized_api.contains("\"key\":\"secret-key-123\""));

        let deserialized_api: StoredCredential = serde_json::from_str(&serialized_api).unwrap();
        assert_eq!(cred_api, deserialized_api);

        // OAuth
        let cred_oauth = StoredCredential::OAuth {
            access: "ey-access-token".to_string(),
            refresh: "rt-refresh-token".to_string(),
            expires: 1731512345678,
            client_id: Some("dynamic_agent_client".to_string()),
            scopes: Some(vec!["openid".to_string(), "profile".to_string()]),
            available_model_ids: Some(vec!["gpt-4o".to_string()]),
        };
        let serialized_oauth = serde_json::to_string(&cred_oauth).unwrap();
        assert!(serialized_oauth.contains("\"type\":\"oauth\""));
        assert!(serialized_oauth.contains("\"access\":\"ey-access-token\""));
        assert!(serialized_oauth.contains("\"clientId\":\"dynamic_agent_client\""));
        assert!(serialized_oauth.contains("\"availableModelIds\":[\"gpt-4o\"]"));

        let deserialized_oauth: StoredCredential = serde_json::from_str(&serialized_oauth).unwrap();
        assert_eq!(cred_oauth, deserialized_oauth);

        // Deserializing with snake_case aliases
        let raw_snake = r#"{
            "type": "oauth",
            "access": "acc",
            "refresh": "ref",
            "expires": 12345,
            "client_id": "test_client",
            "available_model_ids": ["m1"]
        }"#;
        let parsed: StoredCredential = serde_json::from_str(raw_snake).unwrap();
        match parsed {
            StoredCredential::OAuth { client_id, available_model_ids, .. } => {
                assert_eq!(client_id.as_deref(), Some("test_client"));
                assert_eq!(available_model_ids, Some(vec!["m1".to_string()]));
            }
            _ => panic!("Expected OAuth variant"),
        }
    }

    #[tokio::test]
    async fn in_memory_credential_store_serializes_modify_per_provider() {
        use std::sync::Arc;
        let store = Arc::new(InMemoryCredentialStore::new());
        let mut handles = Vec::new();

        for _ in 0..10 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                s.modify(
                    "openai",
                    Box::new(|current| {
                        Box::pin(async move {
                            let cur_num = match current {
                                Some(StoredCredential::ApiKey { key: Some(k), .. }) => {
                                    k.parse::<u32>().unwrap()
                                }
                                _ => 0,
                            };
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            Ok(Some(StoredCredential::ApiKey {
                                key: Some((cur_num + 1).to_string()),
                                env: None,
                            }))
                        })
                    }),
                )
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let final_cred = store.read("openai").await.unwrap();
        match final_cred {
            Some(StoredCredential::ApiKey { key: Some(k), .. }) => {
                assert_eq!(k, "10");
            }
            other => panic!("Unexpected final credential: {other:?}"),
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    // Env vars are process-global; serialize the tests that mutate them.
    static ENV_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn refresh_rejects_private_token_url() {
        let _env_lock = ENV_MUTEX.lock().await;
        let _guard = EnvVarGuard::set("ANTHROPIC_TOKEN_URL", "https://127.0.0.1:1/token");
        let cred = StoredCredential::OAuth {
            access: "access".to_string(),
            refresh: "refresh".to_string(),
            expires: 0,
            client_id: None,
            scopes: None,
            available_model_ids: None,
        };

        let err = refresh_oauth_credential(
            OAuthProviderKind::Anthropic,
            &cred,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("token URL"),
            "expected token URL rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn insecure_refresh_url_requires_explicit_opt_in() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _env_lock = ENV_MUTEX.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"access_token":"mock_access","refresh_token":"mock_refresh","expires_in":3600}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let _url_guard = EnvVarGuard::set("ANTHROPIC_TOKEN_URL", &format!("http://{addr}/token"));
        let _flag_guard = EnvVarGuard::set("MEHH_OAUTH_ALLOW_INSECURE_REFRESH", "");
        let cred = StoredCredential::OAuth {
            access: "access".to_string(),
            refresh: "refresh".to_string(),
            expires: 0,
            client_id: None,
            scopes: None,
            available_model_ids: None,
        };

        let err = refresh_oauth_credential(
            OAuthProviderKind::Anthropic,
            &cred,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("must use https"),
            "loopback http must be rejected by default, got: {err}"
        );

        let _enabled = EnvVarGuard::set("MEHH_OAUTH_ALLOW_INSECURE_REFRESH", "1");
        let refreshed =
            refresh_oauth_credential(OAuthProviderKind::Anthropic, &cred)
                .await
                .unwrap();
        match refreshed {
            StoredCredential::OAuth { access, .. } => assert_eq!(access, "mock_access"),
            other => panic!("Unexpected credential variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loopback_callback_accepts_request_split_inside_state_parameter() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_state = "state-abc-123";

        let waiter = tokio::spawn(async move {
            wait_for_loopback_callback(listener, expected_state, Duration::from_secs(5)).await
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Split mid-state so a single read() cannot see a complete state value.
        client
            .write_all(b"GET /callback?code=code-xyz&state=state-")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The server may have already closed on the old implementation; ignore
        // the write error so the assertion below is the failure signal.
        let _ = client
            .write_all(b"abc-123 HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await;

        let code = waiter.await.unwrap().unwrap();
        assert_eq!(code, "code-xyz");
    }

    #[tokio::test]
    async fn refresh_expiry_saturates_on_overflow() {
        let _env_lock = ENV_MUTEX.lock().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"access_token":"a","refresh_token":"r","expires_in":9223372036854775807}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let _url_guard = EnvVarGuard::set("ANTHROPIC_TOKEN_URL", &format!("http://{addr}/token"));
        let _flag_guard = EnvVarGuard::set("MEHH_OAUTH_ALLOW_INSECURE_REFRESH", "1");
        let cred = StoredCredential::OAuth {
            access: "access".to_string(),
            refresh: "refresh".to_string(),
            expires: 0,
            client_id: None,
            scopes: None,
            available_model_ids: None,
        };

        let refreshed = refresh_oauth_credential(OAuthProviderKind::Anthropic, &cred)
            .await
            .unwrap();
        match refreshed {
            StoredCredential::OAuth { expires, .. } => assert_eq!(expires, i64::MAX),
            other => panic!("Unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    fn expiry_helpers_saturate_on_extreme_values() {
        assert_eq!(expiry_from_ttl_secs(i64::MAX), i64::MAX);
        // Saturating multiplication clamps to i64::MIN; adding the current
        // timestamp stays a valid, far-past value without wrapping.
        let min_expiry = expiry_from_ttl_secs(i64::MIN);
        assert!(min_expiry < 0, "{min_expiry}");
        assert_eq!(copilot_expiry_from_expires_at(i64::MAX), i64::MAX - 300_000);
        assert_eq!(copilot_expiry_from_expires_at(i64::MIN), i64::MIN);
    }

    #[test]
    fn file_lock_guard_releases_os_lock_before_dropping_in_process_guard() {
        let dir = std::env::temp_dir().join(format!(
            "mehh-ai-file-lock-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cred.lock");
        let mutex = get_path_lock_mutex(&path);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let guard = acquire_file_lock(&path, mutex).await.unwrap();

            // Occupy the only blocking thread: a detached unlock task cannot run.
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            started_rx.recv().unwrap();

            drop(guard);

            // A plain thread must be able to take the OS lock right after Drop,
            // without waiting for the runtime's blocking pool to free up.
            let lock_path = path.clone();
            let (locked_tx, locked_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut opts = std::fs::OpenOptions::new();
                opts.create(true).read(true).write(true);
                if let Ok(file) = opts.open(&lock_path) {
                    if file.lock().is_ok() {
                        let _ = locked_tx.send(());
                        let _ = file.unlock();
                    }
                }
            });
            let locked = locked_rx.recv_timeout(Duration::from_millis(500)).is_ok();

            let _ = release_tx.send(());
            blocker.await.unwrap();
            assert!(locked, "OS lock still held after FileLockGuard was dropped");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oauth_error_body_redacts_secrets() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"error":"invalid_client","client_secret":"sk-ant-fake-DO-NOT-LEAK-12345","detail":"Bearer sk-ant-fake-DO-NOT-LEAK-12345"}"#;
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let err = send_oauth_request(reqwest::Client::new().post(format!("http://{addr}/token")), &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("sk-ant-fake-DO-NOT-LEAK-12345"),
            "secret leaked into error: {msg}"
        );
    }

    #[test]
    fn redact_error_body_covers_camel_case_and_dashed_secret_fields() {
        let body = r#"{"error":"bad","accessToken":"opaque-DO-NOT-LEAK-1","refresh-token":"opaque-DO-NOT-LEAK-2","clientSecret":"opaque-DO-NOT-LEAK-3"}"#;
        let redacted = redact_error_body(body, &[]);
        assert!(
            !redacted.contains("opaque-DO-NOT-LEAK-1"),
            "camelCase secret leaked: {redacted}"
        );
        assert!(
            !redacted.contains("opaque-DO-NOT-LEAK-2"),
            "dashed secret leaked: {redacted}"
        );
        assert!(
            !redacted.contains("opaque-DO-NOT-LEAK-3"),
            "camelCase secret leaked: {redacted}"
        );
        assert!(redacted.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn oauth_error_body_redacts_echoed_request_secret() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = "rt.abc.def.ghi-DO-NOT-LEAK";
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let body = format!(
                    r#"{{"error":"invalid_grant","error_description":"Invalid refresh_token: {secret}"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let err = send_oauth_request(
            reqwest::Client::new().post(format!("http://{addr}/token")),
            &[secret],
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(secret),
            "secret leaked into error: {msg}"
        );
        assert!(msg.contains("[REDACTED]"), "expected redaction marker: {msg}");
    }

    #[test]
    fn redact_error_body_redacts_before_truncation() {
        let secret = "S".repeat(600);
        let body = format!(r#"{{"error":"bad","client_secret":"{secret}"}}"#);
        let redacted = redact_error_body(&body, &[]);
        assert!(
            !redacted.contains(&secret[..64]),
            "secret prefix survived truncation: {redacted}"
        );
        assert!(redacted.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn oauth_error_body_redacts_secret_split_by_upstream_char_cap() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // The value starts early (so it survives the final 512-char message),
        // but its closing quote sits past the upstream 4096-char cap. Reading a
        // pre-truncated body hides that quote from the key-based regex.
        let secret = "LEAKTHIS".repeat(700);
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let body = format!(r#"{{"error":"bad","access_token":"{secret}"}}"#);
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let err = send_oauth_request(reqwest::Client::new().post(format!("http://{addr}/token")), &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("LEAKTHIS"),
            "credential leaked into error: {msg}"
        );
    }

    #[test]
    fn reject_unsafe_ip_blocks_cgnat_and_this_network() {
        for ip in ["100.64.0.1", "100.127.255.254", "0.1.2.3"] {
            let parsed: std::net::IpAddr = ip.parse().unwrap();
            assert!(
                reject_unsafe_ip(parsed, "example.test").is_err(),
                "{ip} must be rejected"
            );
        }
        let public: std::net::IpAddr = "93.184.216.34".parse().unwrap();
        assert!(reject_unsafe_ip(public, "example.test").is_ok());
    }

    #[tokio::test]
    async fn pinned_refresh_client_reaches_unresolvable_host_via_validated_address() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let body = "ok";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let url = format!("http://token.invalid:{port}/token");
        let validated = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

        // Sanity: an unpinned client cannot resolve the hostname at all.
        let unpinned = build_refresh_client(None).unwrap();
        assert!(unpinned.post(&url).send().await.is_err());

        let pinned = build_refresh_client(Some(("token.invalid", &[validated]))).unwrap();
        let resp = pinned.post(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }
}
