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

            let res = client
                .post(token_url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;

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
                expires: now_ms() + (expires_in * 1000),
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

            let res = client
                .post(token_url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;

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
                expires: now_ms() + (expires_in * 1000),
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
    client: &reqwest::Client,
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
            let token_url = "https://platform.claude.com/v1/oauth/token";
            let mut params = HashMap::new();
            params.insert("grant_type", "refresh_token");
            params.insert("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
            params.insert("refresh_token", &refresh_token);

            let res = client
                .post(token_url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;

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
                expires: now_ms() + (expires_in * 1000),
                client_id: client_id_opt.or_else(|| Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string())),
                scopes,
                available_model_ids: models,
            })
        }
        OAuthProviderKind::OpenAiChatGpt => {
            let token_url = "https://auth.openai.com/api/accounts/oauth/token";
            let c_id = client_id_opt
                .as_deref()
                .unwrap_or("dynamic_agent_client");
            let mut params = HashMap::new();
            params.insert("grant_type", "refresh_token");
            params.insert("client_id", c_id);
            params.insert("refresh_token", &refresh_token);

            let res = client
                .post(token_url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;

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
                expires: now_ms() + (expires_in * 1000),
                client_id: client_id_opt.or_else(|| Some("dynamic_agent_client".to_string())),
                scopes,
                available_model_ids: models,
            })
        }
        OAuthProviderKind::GitHubCopilot => {
            let res = client
                .get("https://api.github.com/copilot_internal/v2/token")
                .header("Authorization", format!("Bearer {refresh_token}"))
                .header("User-Agent", "GitHubCopilotChat/0.35.0")
                .header("Editor-Version", "vscode/1.107.0")
                .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
                .header("Copilot-Integration-Id", "vscode-chat")
                .header("Accept", "application/json")
                .send()
                .await?
                .error_for_status()?;

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
                expires: (expires_at * 1000).saturating_sub(300_000),
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

    let res = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .form(&params)
        .send()
        .await?
        .error_for_status()?;

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

/// Polls GitHub for token completion during Copilot Device Code Flow.
/// Returns Ok(None) while user authorization is pending.
pub async fn poll_copilot_device_token(
    client: &reqwest::Client,
    device_code: &str,
) -> anyhow::Result<Option<StoredCredential>> {
    let mut params = HashMap::new();
    params.insert("client_id", "Iv1.b507a08c87ecfe98");
    params.insert("device_code", device_code);
    params.insert("grant_type", "urn:ietf:params:oauth:grant-type:device_code");

    let res = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .form(&params)
        .send()
        .await?
        .error_for_status()?;

    let json: serde_json::Value = res.json().await?;

    if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
        match err {
            "authorization_pending" | "slow_down" => return Ok(None),
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
    let copilot_res = client
        .get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", format!("Bearer {github_token}"))
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;

    let copilot_json: serde_json::Value = copilot_res.json().await?;
    let token = copilot_json["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing token in Copilot token exchange"))?
        .to_string();
    let expires_at = copilot_json["expires_at"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing expires_at in Copilot token exchange"))?;

    Ok(Some(StoredCredential::OAuth {
        access: token,
        refresh: github_token,
        expires: (expires_at * 1000).saturating_sub(300_000),
        client_id: Some("Iv1.b507a08c87ecfe98".to_string()),
        scopes: None,
        available_model_ids: None,
    }))
}

/// Listens on loopback `127.0.0.1:{port}` for OAuth redirect callback with code and state.
pub async fn wait_for_loopback_callback(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    tokio::time::timeout(timeout, async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let (mut socket, _) = listener.accept().await?;

        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).await?;
        let request_str = String::from_utf8_lossy(&buf[..n]);

        let req_line = request_str
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty HTTP request"))?;

        let path = req_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("Invalid HTTP request line"))?;

        let query = path
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("");

        let mut code_opt = None;
        let mut state_opt = None;

        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "code" {
                    code_opt = Some(v.to_string());
                } else if k == "state" {
                    state_opt = Some(v.to_string());
                }
            }
        }

        let state = state_opt.ok_or_else(|| anyhow::anyhow!("Missing state in callback URL"))?;
        if state != expected_state {
            anyhow::bail!("State mismatch in callback URL");
        }

        let code = code_opt.ok_or_else(|| anyhow::anyhow!("Missing code in callback URL"))?;

        let html = "<!DOCTYPE html><html><body><h1>Authentication Successful</h1><p>You can close this tab and return to the application.</p></body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;

        Ok(code)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Timed out waiting for loopback callback on port {port}"))?
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
}
