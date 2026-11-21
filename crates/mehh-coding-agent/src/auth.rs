//! API key resolution across CLI flags, environment variables, and auth.json.

use anyhow::Context;
use mehh_ai::oauth::{
    acquire_file_lock, get_path_lock_mutex, refresh_oauth_credential, CredentialInfo,
    CredentialModifyFn, CredentialStore, FileLockGuard, OAuthProviderKind, StoredCredential,
};
use mehh_ai::{env_api_key_name, now_ms};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Returns default auth path `~/.mehh/agent/auth.json`.
pub fn default_auth_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default auth path: neither USERPROFILE nor HOME is set"
            )
        })?;
    Ok(home.join(".mehh").join("agent").join("auth.json"))
}

/// Maps a provider identifier to an OAuthProviderKind, if supported.
pub fn oauth_kind_for_provider(provider: &str) -> Option<OAuthProviderKind> {
    match provider.to_lowercase().as_str() {
        "anthropic" => Some(OAuthProviderKind::Anthropic),
        "openai" | "openai-codex" | "openai-chatgpt" | "openai-responses" => {
            Some(OAuthProviderKind::OpenAiChatGpt)
        }
        "github-copilot" => Some(OAuthProviderKind::GitHubCopilot),
        _ => None,
    }
}

/// Helper to resolve environment variable for a provider.
pub fn resolve_provider_env_var(provider: &str) -> Option<String> {
    let p = provider.to_lowercase();
    let primary_var = env_api_key_name(&p);
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
            path_buf = default_auth_path()?;
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
        let cred = parse_stored_credential_value(val).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid credential entry for provider '{k}' in '{}'",
                path.display()
            )
        })?;
        entries.insert(k, cred);
    }

    Ok(entries)
}

/// File-backed credential store for auth.json with process-safe advisory file locking.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock_path: PathBuf,
    process_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl FileCredentialStore {
    /// Creates a store targeting the default auth path (`~/.mehh/agent/auth.json`).
    pub fn try_new() -> anyhow::Result<Self> {
        Ok(Self::for_path(default_auth_path()?))
    }

    /// Creates a store for a specific auth file path.
    pub fn for_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let process_lock = get_path_lock_mutex(&lock_path);
        Self {
            path,
            lock_path,
            process_lock,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn lock(&self) -> anyhow::Result<FileLockGuard> {
        acquire_file_lock(&self.lock_path, self.process_lock.clone()).await
    }

    pub async fn modify_locked(
        &self,
        provider_id: &str,
        modify_fn: CredentialModifyFn,
    ) -> anyhow::Result<Option<StoredCredential>> {
        self.modify(provider_id, modify_fn).await
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn read(&self, provider_id: &str) -> anyhow::Result<Option<StoredCredential>> {
        let entries = load_auth_entries(Some(&self.path))?;
        let cred = entries.get(provider_id).or_else(|| {
            entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(provider_id))
                .map(|(_, v)| v)
        });
        Ok(cred.cloned())
    }

    async fn list(&self) -> anyhow::Result<Vec<CredentialInfo>> {
        let entries = load_auth_entries(Some(&self.path))?;
        let mut list: Vec<CredentialInfo> = entries
            .into_iter()
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
        let _lock = self.lock().await?;

        let mut entries = load_auth_entries(Some(&self.path))?;
        let existing_key = entries
            .keys()
            .find(|k| k.eq_ignore_ascii_case(provider_id))
            .cloned();
        let current = existing_key.as_ref().and_then(|k| entries.get(k)).cloned();

        let next = modify_fn(current.clone()).await?;

        if let Some(ref new_cred) = next {
            let key = existing_key.unwrap_or_else(|| provider_id.to_string());
            entries.insert(key, new_cred.clone());
            let json_str = serde_json::to_string_pretty(&entries)
                .context("Failed to serialize auth credentials to JSON")?;
            crate::settings::write_file_atomic(&self.path, json_str.as_bytes(), true)?;
            Ok(Some(new_cred.clone()))
        } else {
            Ok(current)
        }
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        let _lock = self.lock().await?;
        if !self.path.exists() {
            return Ok(());
        }

        let mut entries = load_auth_entries(Some(&self.path))?;
        let existing_key = entries
            .keys()
            .find(|k| k.eq_ignore_ascii_case(provider_id))
            .cloned();

        if let Some(k) = existing_key {
            entries.remove(&k);
            let json_str = serde_json::to_string_pretty(&entries)
                .context("Failed to serialize auth credentials to JSON")?;
            crate::settings::write_file_atomic(&self.path, json_str.as_bytes(), true)?;
        }
        Ok(())
    }
}

/// Acquires an advisory file lock on `<path>.lock`, creating parent directories if needed.
fn acquire_auth_file_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directories for: {}", path.display()))?;
    }

    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let lock_file = opts
        .open(&lock_path)
        .with_context(|| format!("Failed to open lock file: {}", lock_path.display()))?;
    lock_file
        .lock()
        .with_context(|| format!("Failed to acquire lock on: {}", lock_path.display()))?;
    Ok(lock_file)
}

/// Saves a credential to auth file, normalizing provider keys case-insensitively.
/// Ensures parent directories exist and writes UTF-8 without BOM under advisory file lock.
pub fn save_credential(
    provider: &str,
    cred: &StoredCredential,
    auth_file: Option<&Path>,
) -> anyhow::Result<()> {
    let path_buf;
    let path = match auth_file {
        Some(p) => p,
        None => {
            path_buf = default_auth_path()?;
            &path_buf
        }
    };

    let lock_file = acquire_auth_file_lock(path)?;

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
    crate::settings::write_file_atomic(path, json_str.as_bytes(), true)?;

    let _ = lock_file.unlock();
    Ok(())
}

/// Removes a credential for provider (case-insensitive) from auth file under advisory file lock.
/// Returns true if an entry was found and removed, false otherwise.
pub fn remove_credential(provider: &str, auth_file: Option<&Path>) -> anyhow::Result<bool> {
    let path_buf;
    let path = match auth_file {
        Some(p) => p,
        None => {
            path_buf = default_auth_path()?;
            &path_buf
        }
    };

    if !path.exists() {
        return Ok(false);
    }

    let lock_file = acquire_auth_file_lock(path)?;

    let mut entries = load_auth_entries(Some(path))?;
    let existing_key = entries
        .keys()
        .find(|k| k.eq_ignore_ascii_case(provider))
        .cloned();

    let removed = if let Some(k) = existing_key {
        entries.remove(&k);
        let json_str = serde_json::to_string_pretty(&entries)
            .context("Failed to serialize auth credentials to JSON")?;
        crate::settings::write_file_atomic(path, json_str.as_bytes(), true)?;
        true
    } else {
        false
    };

    let _ = lock_file.unlock();
    Ok(removed)
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

    let entries = match load_auth_entries(auth_file) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Warning: failed to read auth credentials: {e}");
            return None;
        }
    };
    let cred = entries.get(provider).or_else(|| {
        entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(provider))
            .map(|(_, v)| v)
    })?;

    match cred {
        StoredCredential::ApiKey { key, .. } => key.clone(),
        StoredCredential::OAuth { access, .. } => Some(access.clone()),
    }
}

/// Resolves an API key or OAuth access token asynchronously.
/// Automatically refreshes OAuth tokens if expired or expiring within 60 seconds
/// using double-checked locking under advisory file lock to serialize concurrent refresh callers.
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

    let auth_path = match auth_file {
        Some(p) => p.to_path_buf(),
        None => default_auth_path()?,
    };
    let store = FileCredentialStore::for_path(auth_path);

    let cred = store.read(provider).await?;
    let cred = match cred {
        Some(c) => c,
        None => return Ok(None),
    };

    match cred {
        StoredCredential::ApiKey { key, .. } => Ok(key),
        StoredCredential::OAuth {
            access, expires, ..
        } => {
            let now = now_ms();
            if now + 60_000 >= expires {
                if let Some(kind) = oauth_kind_for_provider(provider) {
                    let refreshed_cred = store
                        .modify(
                            provider,
                            Box::new(move |current_opt| {
                                Box::pin(async move {
                                    let current = match current_opt {
                                        Some(c) => c,
                                        None => return Ok(None),
                                    };
                                    let current_expires = match &current {
                                        StoredCredential::OAuth { expires, .. } => *expires,
                                        _ => return Ok(Some(current)),
                                    };
                                    // Double-check expiry under lock:
                                    if now_ms() + 60_000 < current_expires {
                                        // Another task or process refreshed while we waited for lock
                                        return Ok(Some(current));
                                    }
                                    let client = mehh_ai::default_http_client();
                                    let refreshed =
                                        refresh_oauth_credential(&client, kind, &current).await?;
                                    Ok(Some(refreshed))
                                })
                            }),
                        )
                        .await?;

                    if let Some(StoredCredential::OAuth { access, .. }) = refreshed_cred {
                        return Ok(Some(access));
                    }
                }
            }
            Ok(Some(access))
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
            StoredCredential::ApiKey { key, .. } => {
                assert_eq!(key.as_deref(), Some("sk-openai-key"))
            }
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
        assert_eq!(
            env_api_key_name("azure-openai-responses"),
            "AZURE_OPENAI_API_KEY"
        );
        assert_eq!(
            oauth_kind_for_provider("anthropic"),
            Some(OAuthProviderKind::Anthropic)
        );
        assert_eq!(
            oauth_kind_for_provider("OpenAI-ChatGPT"),
            Some(OAuthProviderKind::OpenAiChatGpt)
        );
        assert_eq!(
            oauth_kind_for_provider("github-copilot"),
            Some(OAuthProviderKind::GitHubCopilot)
        );
        assert_eq!(oauth_kind_for_provider("mistral"), None);
    }

    #[test]
    fn test_auth_corrupt_json_fails_closed_on_load_and_returns_none_on_resolve() {
        let dir = crate::TempTestDir::new("auth_corrupt");
        let auth_file = dir.path().join("auth.json");
        std::fs::write(&auth_file, b"{corrupt-auth-json").unwrap();

        assert!(load_auth_entries(Some(&auth_file)).is_err());
        assert_eq!(resolve_api_key("openai", None, Some(&auth_file)), None);

        // A single malformed entry inside valid JSON must also fail closed instead of being dropped.
        std::fs::write(
            &auth_file,
            r#"{"openai": {"type": "api_key", "key": "sk-ok"}, "broken": 123}"#,
        )
        .unwrap();
        assert!(load_auth_entries(Some(&auth_file)).is_err());
    }

    #[tokio::test]
    async fn file_credential_store_serializes_concurrent_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_clone = post_count.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) =
                    match tokio::time::timeout(Duration::from_secs(3), listener.accept()).await {
                        Ok(Ok(pair)) => pair,
                        _ => break,
                    };
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.starts_with("POST /token") || req_str.contains("/token") {
                    post_count_clone.fetch_add(1, Ordering::SeqCst);
                    // Artificially hold the response briefly so concurrent caller hits lock
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let resp_body = serde_json::json!({
                        "access_token": "refreshed_access_token_123",
                        "refresh_token": "refreshed_refresh_token_123",
                        "expires_in": 3600
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.flush().await;
                }
            }
        });

        struct EnvVarGuard {
            key: &'static str,
            prev: Option<String>,
        }

        impl EnvVarGuard {
            fn set(key: &'static str, value: &str) -> Self {
                let prev = std::env::var(key).ok();
                unsafe {
                    std::env::set_var(key, value);
                }
                Self { key, prev }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.prev {
                        Some(v) => std::env::set_var(self.key, v),
                        None => std::env::remove_var(self.key),
                    }
                }
            }
        }

        // Set ANTHROPIC_TOKEN_URL to point to local mock using RAII guard
        let token_url = format!("http://127.0.0.1:{port}/token");
        let _env_guard = EnvVarGuard::set("ANTHROPIC_TOKEN_URL", &token_url);

        let dir = crate::TempTestDir::new("auth_concurrent_refresh");
        let auth_path = dir.path().join("auth.json");

        // Seed with an expired OAuth token
        let expired_cred = StoredCredential::OAuth {
            access: "old_access_token".to_string(),
            refresh: "old_refresh_token".to_string(),
            expires: 1000, // Long expired
            client_id: Some("test_client".to_string()),
            scopes: None,
            available_model_ids: None,
        };
        save_credential("anthropic", &expired_cred, Some(&auth_path)).unwrap();

        // Spawn two concurrent resolve_api_key_async calls
        let auth_path_clone = auth_path.clone();
        let h1 = tokio::spawn(async move {
            resolve_api_key_async("anthropic", None, Some(&auth_path_clone)).await
        });
        let h2 = tokio::spawn(async move {
            resolve_api_key_async("anthropic", None, Some(&auth_path)).await
        });

        let (res1, res2) = tokio::join!(h1, h2);
        let key1 = res1.unwrap().unwrap();
        let key2 = res2.unwrap().unwrap();

        assert_eq!(key1, Some("refreshed_access_token_123".to_string()));
        assert_eq!(key2, Some("refreshed_access_token_123".to_string()));

        // Crucial invariant: only 1 HTTP POST to /token was performed
        assert_eq!(
            post_count.load(Ordering::SeqCst),
            1,
            "Expected exactly 1 refresh POST under file lock"
        );

        let _ = server_task.await;
    }
}
