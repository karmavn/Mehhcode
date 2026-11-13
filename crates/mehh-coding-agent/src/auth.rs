//! API key resolution across CLI flags, environment variables, and auth.json.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use anyhow::Context;
use mehh_ai::oauth::{refresh_oauth_credential, OAuthProviderKind, StoredCredential};
use mehh_ai::{env_api_key_name, now_ms};

/// Returns default auth path `~/.mehh/agent/auth.json`.
pub fn default_auth_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".mehh").join("agent").join("auth.json")
}

/// Maps a provider identifier to an OAuthProviderKind, if supported.
pub fn oauth_kind_for_provider(provider: &str) -> Option<OAuthProviderKind> {
    match provider.to_lowercase().as_str() {
        "anthropic" => Some(OAuthProviderKind::Anthropic),
        "openai" | "openai-codex" | "openai-chatgpt" => Some(OAuthProviderKind::OpenAiChatGpt),
        "github-copilot" => Some(OAuthProviderKind::GitHubCopilot),
        _ => None,
    }
}

/// Helper to resolve environment variable for a provider.
pub fn resolve_provider_env_var(provider: &str) -> Option<String> {
    let p = provider.to_lowercase();
    let primary_var = env_api_key_name(provider);
    if let Ok(val) = std::env::var(primary_var) {
        let trimmed = val.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    // Provider aliases and fallbacks
    if p == "google" || p == "gemini" {
        if let Ok(val) = std::env::var("GOOGLE_API_KEY") {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    } else if p == "bedrock" || p == "amazon-bedrock" {
        if let Ok(val) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }

    // Dynamic fallback e.g. FOO_API_KEY if primary_var was default "API_KEY"
    if primary_var == "API_KEY" {
        let var_name = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
        if let Ok(val) = std::env::var(var_name) {
            let trimmed = val.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }

    None
}

/// Parses a JSON value into a `StoredCredential`, supporting standard format,
/// legacy string token, or legacy `{ apiKey: "..." }` shapes.
fn parse_stored_credential_value(val: serde_json::Value) -> Option<StoredCredential> {
    if let Ok(cred) = serde_json::from_value::<StoredCredential>(val.clone()) {
        return Some(cred);
    }

    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(StoredCredential::ApiKey {
                key: Some(trimmed.to_string()),
                env: None,
            });
        }
    } else if let Some(obj) = val.as_object() {
        for field in &["apiKey", "api_key", "token", "accessToken"] {
            if let Some(s) = obj.get(*field).and_then(|v| v.as_str()) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(StoredCredential::ApiKey {
                        key: Some(trimmed.to_string()),
                        env: None,
                    });
                }
            }
        }
    }

    None
}

/// Loads auth entries from the given file (or `default_auth_path()`).
/// Strips UTF-8 BOM if present. Returns empty map on `NotFound`.
/// Returns Err on syntax / malformed JSON (fail closed!).
pub fn load_auth_entries(
    auth_file: Option<&Path>,
) -> anyhow::Result<HashMap<String, StoredCredential>> {
    let path_buf;
    let path = match auth_file {
        Some(p) => p,
        None => {
            path_buf = default_auth_path();
            &path_buf
        }
    };

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to read auth file: {}", path.display()))
        }
    };

    let clean = content.strip_prefix('\u{feff}').unwrap_or(&content);
    if clean.trim().is_empty() {
        return Ok(HashMap::new());
    }

    let raw_map: HashMap<String, serde_json::Value> = serde_json::from_str(clean)
        .with_context(|| format!("Failed to parse auth file as JSON: {}", path.display()))?;

    let mut entries = HashMap::new();
    for (k, val) in raw_map {
        if let Some(cred) = parse_stored_credential_value(val) {
            entries.insert(k, cred);
        }
    }

    Ok(entries)
}

/// Saves a credential to auth file, normalizing provider keys case-insensitively.
/// Ensures parent directories exist and writes UTF-8 without BOM.
pub fn save_credential(
    provider: &str,
    cred: &StoredCredential,
    auth_file: Option<&Path>,
) -> anyhow::Result<()> {
    let path_buf;
    let path = match auth_file {
        Some(p) => p,
        None => {
            path_buf = default_auth_path();
            &path_buf
        }
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directories for: {}", path.display()))?;
    }

    let mut entries = load_auth_entries(Some(path))?;

    // Find existing key matching provider case-insensitively
    let existing_key = entries
        .keys()
        .find(|k| k.eq_ignore_ascii_case(provider))
        .cloned();

    let target_key = existing_key.unwrap_or_else(|| provider.to_string());
    entries.insert(target_key, cred.clone());

    let json_str = serde_json::to_string_pretty(&entries)
        .context("Failed to serialize auth credentials to JSON")?;
    std::fs::write(path, json_str.as_bytes())
        .with_context(|| format!("Failed to write auth file: {}", path.display()))?;

    Ok(())
}

/// Removes a credential for provider (case-insensitive) from auth file.
/// Returns true if an entry was found and removed, false otherwise.
pub fn remove_credential(
    provider: &str,
    auth_file: Option<&Path>,
) -> anyhow::Result<bool> {
    let path_buf;
    let path = match auth_file {
        Some(p) => p,
        None => {
            path_buf = default_auth_path();
            &path_buf
        }
    };

    if !path.exists() {
        return Ok(false);
    }

    let mut entries = load_auth_entries(Some(path))?;
    let existing_key = entries
        .keys()
        .find(|k| k.eq_ignore_ascii_case(provider))
        .cloned();

    if let Some(k) = existing_key {
        entries.remove(&k);
        let json_str = serde_json::to_string_pretty(&entries)
            .context("Failed to serialize auth credentials to JSON")?;
        std::fs::write(path, json_str.as_bytes())
            .with_context(|| format!("Failed to write auth file: {}", path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Resolves an API key for the requested provider following priority:
/// 1. Explicit key argument
/// 2. Provider environment variables
/// 3. Auth credential file (`~/.mehh/agent/auth.json` or custom path)
pub fn resolve_api_key(
    provider: &str,
    explicit_key: Option<&str>,
    auth_file: Option<&Path>,
) -> Option<String> {
    if let Some(key) = explicit_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Some(val) = resolve_provider_env_var(provider) {
        return Some(val);
    }

    let entries = load_auth_entries(auth_file).ok()?;
    let cred = entries
        .get(provider)
        .or_else(|| entries.iter().find(|(k, _)| k.eq_ignore_ascii_case(provider)).map(|(_, v)| v))?;

    match cred {
        StoredCredential::ApiKey { key, .. } => key.clone(),
        StoredCredential::OAuth { access, .. } => Some(access.clone()),
    }
}

/// Resolves an API key or OAuth access token asynchronously.
/// Automatically refreshes OAuth tokens if expired or expiring within 60 seconds.
pub async fn resolve_api_key_async(
    provider: &str,
    explicit_key: Option<&str>,
    auth_file: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    if let Some(key) = explicit_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

    if let Some(val) = resolve_provider_env_var(provider) {
        return Ok(Some(val));
    }

    let entries = load_auth_entries(auth_file)?;
    let cred = entries
        .get(provider)
        .or_else(|| entries.iter().find(|(k, _)| k.eq_ignore_ascii_case(provider)).map(|(_, v)| v));

    let cred = match cred {
        Some(c) => c,
        None => return Ok(None),
    };

    match cred {
        StoredCredential::ApiKey { key, .. } => Ok(key.clone()),
        StoredCredential::OAuth {
            access, expires, ..
        } => {
            let now = now_ms();
            if now + 60_000 >= *expires {
                if let Some(kind) = oauth_kind_for_provider(provider) {
                    let client = reqwest::Client::new();
                    let refreshed = refresh_oauth_credential(&client, kind, cred).await?;
                    let new_access = match &refreshed {
                        StoredCredential::OAuth { access, .. } => access.clone(),
                        _ => anyhow::bail!("Refreshed credential is not OAuth"),
                    };
                    save_credential(provider, &refreshed, auth_file)?;
                    return Ok(Some(new_access));
                }
            }
            Ok(Some(access.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_save_remove_auth_entries() {
        let temp_dir = std::env::temp_dir().join(format!("auth_test_{}", uuid::Uuid::now_v7()));
        let auth_file = temp_dir.join("auth.json");

        // Non-existent file returns empty map
        let entries = load_auth_entries(Some(&auth_file)).unwrap();
        assert!(entries.is_empty());

        // Save ApiKey credential
        let cred = StoredCredential::ApiKey {
            key: Some("sk-test-12345".to_string()),
            env: None,
        };
        save_credential("anthropic", &cred, Some(&auth_file)).unwrap();

        // Load again
        let loaded = load_auth_entries(Some(&auth_file)).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("anthropic"), Some(&cred));

        // Save OAuth credential case-insensitively
        let oauth_cred = StoredCredential::OAuth {
            access: "access-token-1".to_string(),
            refresh: "refresh-token-1".to_string(),
            expires: 1893456000000,
            client_id: Some("cid".to_string()),
            scopes: None,
            available_model_ids: None,
        };
        save_credential("Anthropic", &oauth_cred, Some(&auth_file)).unwrap();

        let updated = load_auth_entries(Some(&auth_file)).unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated.get("anthropic"), Some(&oauth_cred));

        // Remove credential
        assert!(remove_credential("ANTHROPIC", Some(&auth_file)).unwrap());
        assert!(!remove_credential("ANTHROPIC", Some(&auth_file)).unwrap());
        let empty = load_auth_entries(Some(&auth_file)).unwrap();
        assert!(empty.is_empty());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_bom_stripping_and_malformed_json() {
        let temp_dir = std::env::temp_dir().join(format!("auth_test_bom_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let auth_file = temp_dir.join("auth.json");

        // UTF-8 BOM with valid JSON
        let bom_json = "\u{feff}{\"openai\": \"sk-openai-key\"}";
        std::fs::write(&auth_file, bom_json.as_bytes()).unwrap();

        let entries = load_auth_entries(Some(&auth_file)).unwrap();
        assert_eq!(entries.len(), 1);
        match entries.get("openai").unwrap() {
            StoredCredential::ApiKey { key, .. } => assert_eq!(key.as_deref(), Some("sk-openai-key")),
            _ => panic!("Expected ApiKey"),
        }

        // Malformed JSON fails closed
        std::fs::write(&auth_file, b"NOT_VALID_JSON{{{").unwrap();
        assert!(load_auth_entries(Some(&auth_file)).is_err());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_legacy_entries_parsing() {
        let temp_dir = std::env::temp_dir().join(format!("auth_legacy_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let auth_file = temp_dir.join("auth.json");

        let legacy_json = serde_json::json!({
            "plain_string": "sk-plain-123",
            "api_key_obj": { "apiKey": "sk-obj-123" },
            "token_obj": { "token": "tok-123" },
            "access_token_obj": { "accessToken": "acc-123" }
        });
        std::fs::write(&auth_file, legacy_json.to_string()).unwrap();

        let entries = load_auth_entries(Some(&auth_file)).unwrap();
        assert_eq!(entries.len(), 4);

        let k1 = resolve_api_key("plain_string", None, Some(&auth_file));
        assert_eq!(k1.as_deref(), Some("sk-plain-123"));

        let k2 = resolve_api_key("api_key_obj", None, Some(&auth_file));
        assert_eq!(k2.as_deref(), Some("sk-obj-123"));

        let k3 = resolve_api_key("token_obj", None, Some(&auth_file));
        assert_eq!(k3.as_deref(), Some("tok-123"));

        let k4 = resolve_api_key("access_token_obj", None, Some(&auth_file));
        assert_eq!(k4.as_deref(), Some("acc-123"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_env_api_key_name_resolution() {
        assert_eq!(env_api_key_name("mistral"), "MISTRAL_API_KEY");
        assert_eq!(env_api_key_name("azure-openai-responses"), "AZURE_OPENAI_API_KEY");
        assert_eq!(oauth_kind_for_provider("anthropic"), Some(OAuthProviderKind::Anthropic));
        assert_eq!(oauth_kind_for_provider("OpenAI-ChatGPT"), Some(OAuthProviderKind::OpenAiChatGpt));
        assert_eq!(oauth_kind_for_provider("github-copilot"), Some(OAuthProviderKind::GitHubCopilot));
        assert_eq!(oauth_kind_for_provider("mistral"), None);
    }
}
