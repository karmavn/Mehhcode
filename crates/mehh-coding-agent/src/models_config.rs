use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use mehh_ai::catalog::{builtin_models, default_base_url};
use mehh_ai::types::{Model, ModelCompat, ModelCost, ThinkingLevelMap};
use serde::{Deserialize, Serialize};

static CONFIG_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn config_lock_mutex(path: &Path) -> Arc<Mutex<()>> {
    let locks = CONFIG_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = mehh_ai::normalize_lock_path(path);
    let mut guard = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn with_config_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory for {}", path.display()))?;
    }
    let mut lock_os_str = path.as_os_str().to_os_string();
    lock_os_str.push(".lock");
    let lock_path = PathBuf::from(lock_os_str);

    let mutex = config_lock_mutex(&lock_path);
    let _in_proc_guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

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

    let result = f();
    let _ = lock_file.unlock();
    result
}

fn same_http_endpoint(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        mehh_ai::normalize_and_validate_http_url(s)
            .map(|u| u.as_str().trim_end_matches('/').to_string())
            .unwrap_or_else(|_| s.trim().trim_end_matches('/').to_string())
    };
    norm(a) == norm(b)
}

/// Returns the default user models configuration path (`~/.mehh/agent/models.json`).
pub fn default_models_path() -> Result<PathBuf> {
    let home = crate::user_home_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot determine default models path: neither USERPROFILE nor HOME is set"
        )
    })?;
    Ok(home.join(".mehh").join("agent").join("models.json"))
}

/// Returns the default cache file for live-discovered models (`~/.mehh/agent/discovered-models.json`).
pub fn default_discovered_models_path() -> Result<PathBuf> {
    let home = crate::user_home_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot determine discovered models path: neither USERPROFILE nor HOME is set"
        )
    })?;
    Ok(home.join(".mehh").join("agent").join("discovered-models.json"))
}

/// Per-provider entry in `discovered-models.json`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredProviderModels {
    pub fetched_at_ms: i64,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredModelsFile {
    #[serde(default)]
    providers: HashMap<String, DiscoveredProviderModels>,
}

fn sanitize_model_for_disk(model: &Model) -> Model {
    let mut sanitized = model.clone();
    if let Some(ref mut headers) = sanitized.headers {
        headers.retain(|k, _| {
            let lower = k.trim().to_ascii_lowercase();
            !matches!(
                lower.as_str(),
                "authorization" | "x-api-key" | "x-goog-api-key" | "api-key" | "proxy-authorization"
            )
        });
        if headers.is_empty() {
            sanitized.headers = None;
        }
    }
    sanitized
}

/// Loads live-discovered provider model lists from disk (`discovered-models.json`).
/// Returns an empty map if the file does not exist; fails closed on corrupt JSON.
pub fn load_discovered_models(
    path: Option<&Path>,
) -> Result<HashMap<String, DiscoveredProviderModels>> {
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_discovered_models_path()?,
    };
    let content = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Failed to read discovered models cache: {}",
                    resolved.display()
                )
            });
        }
    };
    let clean = content.strip_prefix('\u{feff}').unwrap_or(&content);
    if clean.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let parsed: DiscoveredModelsFile = serde_json::from_str(clean).with_context(|| {
        format!(
            "Failed to parse discovered models JSON: {}",
            resolved.display()
        )
    })?;
    let mut out = HashMap::new();
    for (prov, mut entry) in parsed.providers {
        let key = prov.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        for model in &mut entry.models {
            model.provider = key.clone();
            model.id = model.id.trim().to_string();
        }
        out.insert(key, entry);
    }
    Ok(out)
}

fn read_models_file(resolved: &Path) -> Result<ModelsFile> {
    match std::fs::read_to_string(resolved) {
        Ok(content) => {
            let clean = content.strip_prefix('\u{feff}').unwrap_or(&content);
            if clean.trim().is_empty() {
                Ok(ModelsFile::default())
            } else {
                serde_json::from_str(clean).with_context(|| {
                    format!("Failed to parse models JSON: {}", resolved.display())
                })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ModelsFile::default()),
        Err(e) => {
            Err(e).with_context(|| format!("Failed to read models JSON: {}", resolved.display()))
        }
    }
}

/// Saves discovered models for `provider` atomically to `discovered-models.json` under file lock.
/// Strips any sensitive credential headers before writing.
pub fn save_discovered_provider_models(
    provider: &str,
    models: &[Model],
    path: Option<&Path>,
) -> Result<()> {
    let _ = save_discovered_provider_models_if_active(provider, models, path, None)?;
    Ok(())
}

fn save_discovered_provider_models_if_active(
    provider: &str,
    models: &[Model],
    path: Option<&Path>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<bool> {
    let key = provider.trim().to_ascii_lowercase();
    if key.is_empty() {
        bail!("Provider name must not be empty when caching discovered models");
    }
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_discovered_models_path()?,
    };
    with_config_file_lock(&resolved, || {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Ok(false);
        }
        let mut providers = load_discovered_models(Some(&resolved))?;
        let sanitized: Vec<Model> = models
            .iter()
            .map(|m| {
                let mut copy = sanitize_model_for_disk(m);
                copy.provider = key.clone();
                copy.id = copy.id.trim().to_string();
                copy
            })
            .collect();
        providers.insert(
            key.clone(),
            DiscoveredProviderModels {
                fetched_at_ms: mehh_ai::now_ms(),
                models: sanitized,
            },
        );
        let file_data = DiscoveredModelsFile { providers };
        let json = serde_json::to_string_pretty(&file_data)
            .context("Failed to serialize discovered models cache")?;
        crate::settings::write_file_atomic(&resolved, json.as_bytes(), true)?;
        Ok(true)
    })
}

/// Async cancellable save of discovered models safe to call from Tokio worker threads.
/// Returns `Ok(false)` without touching disk if `cancel` is cancelled before acquiring the file lock.
pub async fn save_discovered_provider_models_cancellable_async(
    provider: &str,
    models: &[Model],
    path: Option<&Path>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<bool> {
    let provider_owned = provider.to_string();
    let models_owned = models.to_vec();
    let path_owned = path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        save_discovered_provider_models_if_active(
            &provider_owned,
            &models_owned,
            path_owned.as_deref(),
            Some(&cancel),
        )
    })
    .await
    .context("Save discovered models task panicked")?
}

/// Removes cached discovered models for `provider` when disconnecting or logging out.
pub fn remove_discovered_provider_models(provider: &str, path: Option<&Path>) -> Result<()> {
    let key = provider.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Ok(());
    }
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_discovered_models_path()?,
    };
    with_config_file_lock(&resolved, || {
        let mut providers = load_discovered_models(Some(&resolved))?;
        let before = providers.len();
        providers.retain(|k, _| !k.trim().eq_ignore_ascii_case(&key));
        if providers.len() == before {
            return Ok(());
        }
        let file_data = DiscoveredModelsFile { providers };
        let json = serde_json::to_string_pretty(&file_data)
            .context("Failed to serialize discovered models cache")?;
        crate::settings::write_file_atomic(&resolved, json.as_bytes(), true)
    })
}

/// Async version of [`remove_discovered_provider_models`] safe to call from Tokio worker threads.
pub async fn remove_discovered_provider_models_async(
    provider: &str,
    path: Option<&Path>,
) -> Result<()> {
    let provider_owned = provider.to_string();
    let path_owned = path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        remove_discovered_provider_models(&provider_owned, path_owned.as_deref())
    })
    .await
    .context("Remove discovered models task panicked")?
}

/// Adds or updates a custom OpenAI-compatible provider (e.g., Ollama / vLLM / proxy) in `models.json`.
pub fn upsert_custom_provider(
    provider: &str,
    base_url: &str,
    api: Option<&str>,
    path: Option<&Path>,
) -> Result<()> {
    let provider_key = provider.trim().to_ascii_lowercase();
    if provider_key.is_empty() {
        bail!("Custom provider ID must not be empty");
    }
    let parsed_url =
        mehh_ai::normalize_and_validate_http_url(base_url).map_err(|e| anyhow::anyhow!(e))?;
    mehh_ai::check_url_host_policy(mehh_ai::DnsPolicy::AllowPrivate, &parsed_url)
        .map_err(|e| anyhow::anyhow!(e))?;
    let normalized_url = parsed_url.as_str().trim_end_matches('/').to_string();

    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_models_path()?,
    };

    with_config_file_lock(&resolved, || {
        let mut file_data = read_models_file(&resolved)?;

        let existing_keys: Vec<String> = file_data
            .providers
            .keys()
            .filter(|k| k.trim().eq_ignore_ascii_case(&provider_key))
            .cloned()
            .collect();
        let mut cfg = ProviderConfig {
            base_url: None,
            api_key: None,
            api: None,
            headers: None,
            models: Vec::new(),
        };
        for old_k in existing_keys {
            if let Some(prev) = file_data.providers.remove(&old_k) {
                cfg = prev;
            }
        }
        cfg.base_url = Some(normalized_url);
        cfg.api = api
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .or(cfg.api)
            .or_else(|| Some(default_api_for_provider(&provider_key).to_string()));
        file_data.providers.insert(provider_key, cfg);

        let json = serde_json::to_string_pretty(&file_data)
            .context("Failed to serialize models configuration")?;
        crate::settings::write_file_atomic(&resolved, json.as_bytes(), true)
    })
}

/// Async version of [`upsert_custom_provider`] safe to call from Tokio worker threads.
pub async fn upsert_custom_provider_async(
    provider: &str,
    base_url: &str,
    api: Option<&str>,
    path: Option<&Path>,
) -> Result<()> {
    let provider_owned = provider.to_string();
    let base_url_owned = base_url.to_string();
    let api_owned = api.map(str::to_string);
    let path_owned = path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        upsert_custom_provider(
            &provider_owned,
            &base_url_owned,
            api_owned.as_deref(),
            path_owned.as_deref(),
        )
    })
    .await
    .context("Upsert custom provider task panicked")?
}

/// Removes a custom provider entry (case-insensitive) from `models.json` under file lock.
/// Returns `Ok(true)` if an entry was removed, `Ok(false)` if absent or no file exists.
pub fn remove_custom_provider(provider: &str, path: Option<&Path>) -> Result<bool> {
    let provider_key = provider.trim().to_ascii_lowercase();
    if provider_key.is_empty() {
        return Ok(false);
    }
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_models_path()?,
    };
    with_config_file_lock(&resolved, || {
        if !resolved.exists() {
            return Ok(false);
        }
        let mut file_data = read_models_file(&resolved)?;
        let keys_to_remove: Vec<String> = file_data
            .providers
            .keys()
            .filter(|k| k.trim().eq_ignore_ascii_case(&provider_key))
            .cloned()
            .collect();
        if keys_to_remove.is_empty() {
            return Ok(false);
        }
        for k in keys_to_remove {
            file_data.providers.remove(&k);
        }
        let json = serde_json::to_string_pretty(&file_data)
            .context("Failed to serialize models configuration")?;
        crate::settings::write_file_atomic(&resolved, json.as_bytes(), true)?;
        Ok(true)
    })
}

/// Disconnects `provider` in `models.json` safely under file lock:
/// - For non-builtin custom providers, removes the provider block entirely.
/// - For built-in providers, clears only `api_key` and preserves user-authored `baseUrl`, `api`, `headers`, and custom `models`
///   (removing the entry only if nothing remains).
pub fn disconnect_provider_in_models_file(provider: &str, path: Option<&Path>) -> Result<bool> {
    let provider_key = provider.trim().to_ascii_lowercase();
    if provider_key.is_empty() {
        return Ok(false);
    }
    let resolved = match path {
        Some(p) => p.to_path_buf(),
        None => default_models_path()?,
    };
    let is_builtin =
        mehh_ai::find_provider_spec(&provider_key).is_some() || default_base_url(&provider_key).is_some();

    with_config_file_lock(&resolved, || {
        if !resolved.exists() {
            return Ok(false);
        }
        let mut file_data = read_models_file(&resolved)?;
        let matching_keys: Vec<String> = file_data
            .providers
            .keys()
            .filter(|k| k.trim().eq_ignore_ascii_case(&provider_key))
            .cloned()
            .collect();
        if matching_keys.is_empty() {
            return Ok(false);
        }

        let mut changed = false;
        for k in matching_keys {
            if !is_builtin {
                file_data.providers.remove(&k);
                changed = true;
            } else if let Some(mut cfg) = file_data.providers.remove(&k) {
                if cfg.api_key.take().is_some() {
                    changed = true;
                }
                let is_empty = cfg.base_url.as_ref().is_none_or(|s| s.trim().is_empty())
                    && cfg.api.as_ref().is_none_or(|s| s.trim().is_empty())
                    && cfg.headers.as_ref().is_none_or(HashMap::is_empty)
                    && cfg.models.is_empty();
                if is_empty {
                    changed = true;
                } else {
                    file_data.providers.insert(k, cfg);
                }
            }
        }

        if !changed {
            return Ok(false);
        }
        let json = serde_json::to_string_pretty(&file_data)
            .context("Failed to serialize models configuration")?;
        crate::settings::write_file_atomic(&resolved, json.as_bytes(), true)?;
        Ok(true)
    })
}

/// Async version of [`disconnect_provider_in_models_file`] safe to call from Tokio worker threads.
pub async fn disconnect_provider_in_models_file_async(
    provider: &str,
    path: Option<&Path>,
) -> Result<bool> {
    let provider_owned = provider.to_string();
    let path_owned = path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        disconnect_provider_in_models_file(&provider_owned, path_owned.as_deref())
    })
    .await
    .context("Disconnect provider in models config task panicked")?
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsFile {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub models: Vec<CustomModelEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelEntry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub input: Option<Vec<String>>,
    #[serde(default)]
    pub cost: Option<ModelCost>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub compat: Option<ModelCompat>,
    #[serde(default)]
    pub thinking_level_map: Option<ThinkingLevelMap>,
}

/// Registry combining built-in catalog models with validated entries from `~/.mehh/agent/models.json`.
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: Vec<Model>,
    provider_api_keys: HashMap<String, String>,
    provider_configs: HashMap<String, ProviderConfig>,
    discovered_by_provider: HashMap<String, Vec<Model>>,
    explicit_base_url_providers: HashSet<String>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::builtin_only()
    }
}

/// Looks up a configured provider API key from `models.json` without loading the full built-in catalog.
pub fn lookup_provider_api_key(provider: &str, models_file: Option<&Path>) -> Result<Option<String>> {
    let resolved = match models_file {
        Some(p) => p.to_path_buf(),
        None => default_models_path()?,
    };
    let raw = match std::fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read models config '{}'", resolved.display()));
        }
    };
    let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let parsed: ModelsFile = serde_json::from_str(clean)
        .with_context(|| format!("Failed to parse models config '{}'", resolved.display()))?;
    for (provider_name, cfg) in parsed.providers {
        if provider_name.trim().eq_ignore_ascii_case(provider.trim()) {
            let key = cfg
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            return Ok(key);
        }
    }
    Ok(None)
}

impl ModelRegistry {
    /// Creates a registry containing only the built-in catalog models.
    pub fn builtin_only() -> Self {
        Self {
            models: builtin_models(),
            provider_api_keys: HashMap::new(),
            provider_configs: HashMap::new(),
            discovered_by_provider: HashMap::new(),
            explicit_base_url_providers: HashSet::new(),
        }
    }

    /// Loads models from `~/.mehh/agent/models.json` if present, failing closed on invalid JSON or missing HOME.
    pub fn load_default() -> Result<Self> {
        let path = default_models_path()?;
        Self::load_with_path(&path)
    }

    /// Loads models from `path` merged onto the built-in catalog. Missing file yields built-ins only.
    pub fn load_with_path(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::builtin_only()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read models config '{}'", path.display()));
            }
        };

        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let parsed: ModelsFile = serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse models config '{}'", path.display()))?;

        let mut models = builtin_models();
        let mut provider_api_keys = HashMap::new();
        let mut provider_configs = HashMap::new();
        let mut explicit_base_url_providers = HashSet::new();

        for (provider_name, cfg) in parsed.providers {
            let provider_trimmed = provider_name.trim().to_string();
            if provider_trimmed.is_empty() {
                bail!(
                    "Provider key in models config '{}' cannot be empty",
                    path.display()
                );
            }

            let provider_key = provider_trimmed.to_ascii_lowercase();
            if provider_configs.contains_key(&provider_key) {
                bail!(
                    "Duplicate provider entry '{}' in models config '{}'",
                    provider_trimmed,
                    path.display()
                );
            }
            if let Some(key) = cfg
                .api_key
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                provider_api_keys.insert(provider_key.clone(), key.to_string());
            }

            let explicit_base = cfg
                .base_url
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());

            if cfg.base_url.as_ref().is_some_and(|s| s.trim().is_empty()) {
                bail!(
                    "Provider '{}' in models config '{}' has an empty baseUrl",
                    provider_trimmed,
                    path.display()
                );
            }

            let validated_explicit_base = match explicit_base {
                Some(raw_url) => {
                    let parsed = mehh_ai::http_safety::normalize_and_validate_http_url(raw_url)
                        .map_err(|err| {
                            anyhow::anyhow!(
                                "Invalid baseUrl '{}' for provider '{}' in '{}': {err}",
                                raw_url,
                                provider_trimmed,
                                path.display()
                            )
                        })?;
                    mehh_ai::http_safety::check_url_host_policy(
                        mehh_ai::http_safety::DnsPolicy::AllowPrivate,
                        &parsed,
                    )
                    .map_err(|err| {
                        anyhow::anyhow!(
                            "Unsafe baseUrl '{}' for provider '{}' in '{}': {err}",
                            raw_url,
                            provider_trimmed,
                            path.display()
                        )
                    })?;
                    Some(parsed.as_str().trim_end_matches('/').to_string())
                }
                None => None,
            };

            let default_base = default_base_url(&provider_key);
            if validated_explicit_base.as_deref().is_some_and(|explicit| {
                default_base.is_none_or(|default| !same_http_endpoint(explicit, default))
            }) {
                explicit_base_url_providers.insert(provider_key.clone());
            }

            let resolved_provider_base = validated_explicit_base
                .or_else(|| default_base.map(str::to_string));

            let normalized_models: Vec<CustomModelEntry> = cfg
                .models
                .iter()
                .map(|entry| {
                    let mut cloned = entry.clone();
                    cloned.id = cloned.id.trim().to_string();
                    cloned
                })
                .collect();

            provider_configs.insert(
                provider_key,
                ProviderConfig {
                    base_url: resolved_provider_base.clone(),
                    api_key: cfg.api_key.clone(),
                    api: cfg.api.clone(),
                    headers: cfg.headers.clone(),
                    models: normalized_models.clone(),
                },
            );

            // Override existing built-in models for this provider if baseUrl or headers are given.
            if cfg.base_url.is_some() || cfg.headers.is_some() {
                for existing in &mut models {
                    if existing.provider.eq_ignore_ascii_case(&provider_trimmed) {
                        if let Some(ref url) = resolved_provider_base {
                            existing.base_url = url.clone();
                        }
                        if let Some(ref hdrs) = cfg.headers {
                            let mut merged = existing.headers.take().unwrap_or_default();
                            for (k, v) in hdrs {
                                merged.insert(k.clone(), v.clone());
                            }
                            existing.headers = Some(merged);
                        }
                    }
                }
            }

            for entry in normalized_models {
                let id = entry.id.trim().to_string();
                if id.is_empty() {
                    bail!(
                        "Model entry for provider '{}' in '{}' has empty id",
                        provider_trimmed,
                        path.display()
                    );
                }

                let existing_idx = models.iter().position(|m| {
                    m.provider.eq_ignore_ascii_case(&provider_trimmed)
                        && m.id.eq_ignore_ascii_case(&id)
                });

                let base_url = resolved_provider_base
                    .clone()
                    .or_else(|| existing_idx.map(|i| models[i].base_url.clone()))
                    .unwrap_or_default();
                if base_url.trim().is_empty() {
                    bail!(
                        "Custom model '{}/{}' in '{}' requires a non-empty baseUrl",
                        provider_trimmed,
                        id,
                        path.display()
                    );
                }

                let api = entry
                    .api
                    .clone()
                    .or_else(|| cfg.api.clone())
                    .or_else(|| existing_idx.map(|i| models[i].api.clone()))
                    .unwrap_or_else(|| default_api_for_provider(&provider_trimmed).to_string());

                let mut headers = existing_idx
                    .and_then(|i| models[i].headers.clone())
                    .unwrap_or_default();
                if let Some(ref p_hdrs) = cfg.headers {
                    for (k, v) in p_hdrs {
                        headers.insert(k.clone(), v.clone());
                    }
                }
                if let Some(ref m_hdrs) = entry.headers {
                    for (k, v) in m_hdrs {
                        headers.insert(k.clone(), v.clone());
                    }
                }

                let base_existing = existing_idx.map(|i| &models[i]);
                let model = Model {
                    id: id.clone(),
                    name: entry
                        .name
                        .or_else(|| base_existing.map(|m| m.name.clone()))
                        .unwrap_or(id),
                    api,
                    provider: provider_trimmed.clone(),
                    base_url,
                    reasoning: entry
                        .reasoning
                        .or_else(|| base_existing.map(|m| m.reasoning))
                        .unwrap_or(false),
                    input: entry
                        .input
                        .or_else(|| base_existing.map(|m| m.input.clone()))
                        .unwrap_or_else(|| vec!["text".to_string()]),
                    cost: entry
                        .cost
                        .or_else(|| base_existing.map(|m| m.cost.clone()))
                        .unwrap_or_default(),
                    context_window: entry
                        .context_window
                        .or_else(|| base_existing.map(|m| m.context_window))
                        .unwrap_or(128_000),
                    max_tokens: entry
                        .max_tokens
                        .or_else(|| base_existing.map(|m| m.max_tokens))
                        .unwrap_or(16_384),
                    headers: if headers.is_empty() {
                        None
                    } else {
                        Some(headers)
                    },
                    compat: entry
                        .compat
                        .or_else(|| base_existing.and_then(|m| m.compat.clone())),
                    thinking_level_map: entry
                        .thinking_level_map
                        .or_else(|| base_existing.and_then(|m| m.thinking_level_map.clone())),
                };

                if let Some(idx) = existing_idx {
                    models[idx] = model;
                } else {
                    models.push(model);
                }
            }
        }

        Ok(Self {
            models,
            provider_api_keys,
            provider_configs,
            discovered_by_provider: HashMap::new(),
            explicit_base_url_providers,
        })
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn provider_api_key(&self, provider: &str) -> Option<&str> {
        self.provider_api_keys
            .get(&provider.trim().to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn provider_config(&self, provider: &str) -> Option<&ProviderConfig> {
        self.provider_configs
            .get(&provider.trim().to_ascii_lowercase())
    }

    /// Returns true when `models.json` declares a non-default explicit `baseUrl` for `provider`.
    pub fn provider_has_explicit_endpoint(&self, provider: &str) -> bool {
        self.explicit_base_url_providers
            .contains(&provider.trim().to_ascii_lowercase())
    }

    pub fn provider_configs(&self) -> &HashMap<String, ProviderConfig> {
        &self.provider_configs
    }

    pub fn discovered_models_for(&self, provider: &str) -> Option<&[Model]> {
        self.discovered_by_provider
            .get(&provider.trim().to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    /// Loads `models.json` merged with cached live-discovered models from `discovered-models.json`.
    pub fn load_with_discovered(
        models_path: Option<&Path>,
        discovered_path: Option<&Path>,
    ) -> Result<Self> {
        let mut reg = match models_path {
            Some(path) => Self::load_with_path(path)?,
            None => Self::load_default()?,
        };
        let discovered = load_discovered_models(discovered_path)?;
        let mut model_index: HashMap<(String, String), usize> =
            HashMap::with_capacity(reg.models.len() + discovered.values().map(|e| e.models.len()).sum::<usize>());
        for (idx, existing) in reg.models.iter().enumerate() {
            model_index
                .entry((
                    existing.provider.trim().to_ascii_lowercase(),
                    existing.id.trim().to_ascii_lowercase(),
                ))
                .or_insert(idx);
        }

        for (provider_key, entry) in discovered {
            let mut merged_for_provider = Vec::with_capacity(entry.models.len());
            let mut seen_ids = std::collections::HashSet::with_capacity(entry.models.len());
            let cfg_opt = reg.provider_configs.get(&provider_key).cloned();
            let explicit_overrides: HashSet<String> = cfg_opt
                .as_ref()
                .map(|cfg| {
                    cfg.models
                        .iter()
                        .map(|c| c.id.trim().to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();

            for mut model in entry.models {
                model.provider = provider_key.clone();
                model.id = model.id.trim().to_string();
                let id_lower = model.id.to_ascii_lowercase();
                if id_lower.is_empty() || !seen_ids.insert(id_lower.clone()) {
                    continue;
                }

                let has_explicit_override = explicit_overrides.contains(&id_lower);
                if has_explicit_override {
                    if let Some(&idx) = model_index.get(&(provider_key.clone(), id_lower.clone())) {
                        merged_for_provider.push(reg.models[idx].clone());
                        continue;
                    }
                }

                if let Some(base) = cfg_opt
                    .as_ref()
                    .and_then(|c| c.base_url.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    model.base_url = base.to_string();
                } else if let Some(builtin_base) = default_base_url(&provider_key) {
                    model.base_url = builtin_base.to_string();
                } else {
                    if model.base_url.trim().is_empty() {
                        continue;
                    }
                    let valid_cached_url =
                        mehh_ai::http_safety::normalize_and_validate_http_url(&model.base_url)
                            .and_then(|u| {
                                mehh_ai::http_safety::check_url_host_policy(
                                    mehh_ai::http_safety::DnsPolicy::AllowPrivate,
                                    &u,
                                )
                                .map(|()| u.as_str().trim_end_matches('/').to_string())
                            });
                    match valid_cached_url {
                        Ok(normalized) => model.base_url = normalized,
                        Err(_) => continue,
                    }
                }

                if let Some(ref cfg) = cfg_opt {
                    if let Some(ref api) = cfg.api {
                        let trimmed = api.trim();
                        if !trimmed.is_empty() {
                            model.api = trimmed.to_string();
                        }
                    }
                    if let Some(ref extra_headers) = cfg.headers {
                        let mut headers = model.headers.take().unwrap_or_default();
                        for (k, v) in extra_headers {
                            headers.insert(k.clone(), v.clone());
                        }
                        if !headers.is_empty() {
                            model.headers = Some(headers);
                        }
                    }
                }

                merged_for_provider.push(model.clone());
                let key_pair = (provider_key.clone(), id_lower);
                if let Some(&idx) = model_index.get(&key_pair) {
                    if !has_explicit_override {
                        reg.models[idx] = model;
                    }
                } else {
                    let idx = reg.models.len();
                    reg.models.push(model);
                    model_index.insert(key_pair, idx);
                }
            }

            if !merged_for_provider.is_empty() {
                reg.discovered_by_provider
                    .insert(provider_key, merged_for_provider);
            }
        }
        Ok(reg)
    }

    /// Returns models belonging exclusively to the given connected providers.
    /// Prefers live-discovered models when cached for a provider, plus any explicit custom models.
    pub fn models_for_connected_providers(
        &self,
        connected: &[crate::auth::ConnectedProvider],
    ) -> Vec<Model> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for cp in connected {
            let p_lower = cp.provider_id.trim().to_ascii_lowercase();
            if p_lower.is_empty() {
                continue;
            }
            if let Some(discovered) = self.discovered_by_provider.get(&p_lower) {
                for m in discovered {
                    if seen.insert((p_lower.clone(), m.id.to_ascii_lowercase())) {
                        out.push(m.clone());
                    }
                }
                if let Some(cfg) = self.provider_configs.get(&p_lower) {
                    for custom in &cfg.models {
                        if let Some(m) = self.resolve_exact(&p_lower, &custom.id) {
                            if seen.insert((p_lower.clone(), m.id.to_ascii_lowercase())) {
                                out.push(m);
                            }
                        }
                    }
                }
            } else {
                for m in &self.models {
                    if m.provider.eq_ignore_ascii_case(&p_lower)
                        && seen.insert((p_lower.clone(), m.id.to_ascii_lowercase()))
                    {
                        out.push(m.clone());
                    }
                }
            }
        }

        out
    }

    /// Resolves an exact `(provider, model_id)` pair without inventing an empty `base_url`.
    pub fn resolve_exact(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.models
            .iter()
            .find(|m| {
                m.provider.eq_ignore_ascii_case(provider.trim())
                    && m.id.eq_ignore_ascii_case(model_id.trim())
            })
            .cloned()
    }

    /// Resolves `(provider_or_id, Some(model_id))` or a single specifier (`provider/id`, `id`, or `provider`).
    pub fn resolve(&self, provider_or_id: &str, model_id: Option<&str>) -> Option<Model> {
        if let Some(id) = model_id {
            return self.resolve_exact(provider_or_id, id);
        }

        let needle = provider_or_id.trim();
        if needle.is_empty() {
            return None;
        }

        if let Some((prov, id)) = needle.split_once('/') {
            if let Some(found) = self.resolve_exact(prov, id) {
                return Some(found);
            }
        }

        if let Some(found) = self
            .models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(needle))
        {
            return Some(found.clone());
        }

        self.models
            .iter()
            .find(|m| m.provider.eq_ignore_ascii_case(needle))
            .cloned()
    }
}

fn default_api_for_provider(provider: &str) -> &'static str {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" => "anthropic-messages",
        "openai-responses" => "openai-responses",
        "google" | "google-vertex" => "google-generative-ai",
        "mistral" => "mistral-conversations",
        "amazon-bedrock" => "bedrock-converse-stream",
        "azure-openai-responses" => "azure-openai-responses",
        _ => "openai-completions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_registry_resolves_standard_and_rejects_unknown() {
        let reg = ModelRegistry::builtin_only();
        let sonnet = reg.resolve_exact("anthropic", "claude-sonnet-4-5").unwrap();
        assert!(!sonnet.base_url.is_empty());
        assert!(reg
            .resolve_exact("openai", "nonexistent-model-xyz")
            .is_none());
    }

    #[test]
    fn test_custom_models_json_loads_and_fails_closed_on_invalid_json() {
        let dir = crate::TempTestDir::new("models_cfg");
        let path = dir.path().join("models.json");

        // BOM + valid custom provider
        std::fs::write(
            &path,
            "\u{feff}{\"providers\":{\"local-vllm\":{\"baseUrl\":\"http://127.0.0.1:8000/v1\",\"models\":[{\"id\":\"qwen-coder\",\"contextWindow\":65536}]}}}",
        )
        .unwrap();

        let reg = ModelRegistry::load_with_path(&path).unwrap();
        let custom = reg.resolve_exact("local-vllm", "qwen-coder").unwrap();
        assert_eq!(custom.base_url, "http://127.0.0.1:8000/v1");
        assert_eq!(custom.api, "openai-completions");
        assert_eq!(custom.context_window, 65536);

        // Missing baseUrl for unknown custom provider must fail closed
        std::fs::write(
            &path,
            "{\"providers\":{\"broken\":{\"models\":[{\"id\":\"x\"}]}}}",
        )
        .unwrap();
        assert!(ModelRegistry::load_with_path(&path).is_err());

        // Corrupt JSON must fail closed
        std::fs::write(&path, "{not-valid-json").unwrap();
        assert!(ModelRegistry::load_with_path(&path).is_err());
    }

    #[test]
    fn test_custom_models_json_propagates_compat_and_thinking_level_map() {
        let dir = crate::TempTestDir::new("models_compat_cfg");
        let path = dir.path().join("models.json");

        let json = r#"{
            "providers": {
                "custom-ai": {
                    "baseUrl": "https://api.custom.ai/v1",
                    "models": [{
                        "id": "smart-model",
                        "compat": {
                            "supportsLongCacheRetention": true,
                            "forceAdaptiveThinking": true
                        },
                        "thinkingLevelMap": {
                            "low": "min",
                            "high": "max",
                            "off": null
                        }
                    }]
                }
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let reg = ModelRegistry::load_with_path(&path).unwrap();
        let model = reg.resolve_exact("custom-ai", "smart-model").unwrap();
        let compat = model.compat.as_ref().expect("compat should be present");
        assert_eq!(compat.supports_long_cache_retention, Some(true));
        assert_eq!(compat.force_adaptive_thinking, Some(true));
        assert_eq!(compat.supports_temperature, None);

        let map = model
            .thinking_level_map
            .as_ref()
            .expect("thinking_level_map present");
        assert_eq!(map.get("low"), Some(&Some("min".to_string())));
        assert_eq!(map.get("high"), Some(&Some("max".to_string())));
        assert_eq!(map.get("off"), Some(&None));
    }

    #[test]
    fn test_discovered_models_roundtrip_strips_secret_headers_and_filters_connected() {
        let dir = crate::TempTestDir::new("discovered_models");
        let models_path = dir.path().join("models.json");
        let disc_path = dir.path().join("discovered-models.json");

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer topsecret".to_string());
        headers.insert("x-api-key".to_string(), "sk-secret".to_string());
        headers.insert("X-Custom-Header".to_string(), "safe".to_string());

        let m1 = Model {
            id: "claude-opus-4-6".to_string(),
            name: "Claude Opus 4.6".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: true,
            headers: Some(headers),
            ..Default::default()
        };
        save_discovered_provider_models("Anthropic", &[m1], Some(&disc_path)).unwrap();

        let loaded = load_discovered_models(Some(&disc_path)).unwrap();
        let entry = loaded.get("anthropic").unwrap();
        assert_eq!(entry.models.len(), 1);
        let h = entry.models[0].headers.as_ref().unwrap();
        assert!(!h.contains_key("Authorization"));
        assert!(!h.contains_key("x-api-key"));
        assert_eq!(h.get("X-Custom-Header").map(String::as_str), Some("safe"));

        let reg = ModelRegistry::load_with_discovered(Some(&models_path), Some(&disc_path)).unwrap();
        assert!(reg.resolve_exact("anthropic", "claude-opus-4-6").is_some());

        let connected = vec![crate::auth::ConnectedProvider {
            provider_id: "anthropic".to_string(),
            source: crate::auth::CredentialSource::AuthFile,
            has_oauth: false,
        }];
        let visible = reg.models_for_connected_providers(&connected);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "claude-opus-4-6");
        assert!(visible.iter().all(|m| m.provider == "anthropic"));

        remove_discovered_provider_models("anthropic", Some(&disc_path)).unwrap();
        assert!(load_discovered_models(Some(&disc_path)).unwrap().is_empty());
    }

    #[test]
    fn test_upsert_custom_provider_validates_urls() {
        let dir = crate::TempTestDir::new("custom_provider");
        let models_path = dir.path().join("models.json");

        upsert_custom_provider("ollama", "http://localhost:11434/v1/", None, Some(&models_path))
            .unwrap();
        let reg = ModelRegistry::load_with_path(&models_path).unwrap();
        assert_eq!(
            reg.provider_config("ollama")
                .and_then(|c| c.base_url.as_deref()),
            Some("http://localhost:11434/v1")
        );

        assert!(
            upsert_custom_provider("bad1", "http://169.254.169.254/v1", None, Some(&models_path))
                .is_err()
        );
        assert!(
            upsert_custom_provider("bad2", "file:///etc/passwd", None, Some(&models_path)).is_err()
        );
    }

    #[tokio::test]
    async fn test_concurrent_save_discovered_models_preserves_all_providers() {
        let dir = crate::TempTestDir::new("concurrent_disc");
        let disc_path = dir.path().join("discovered-models.json");

        let mut handles = Vec::new();
        for i in 0..8 {
            let p = disc_path.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let prov = format!("provider_{i}");
                let model = Model {
                    id: format!("model-{i}"),
                    name: format!("Model {i}"),
                    api: "openai-completions".to_string(),
                    provider: prov.clone(),
                    base_url: "https://api.example.com/v1".to_string(),
                    ..Default::default()
                };
                save_discovered_provider_models(&prov, &[model], Some(&p))
            }));
        }

        for h in handles {
            h.await.unwrap().expect("concurrent save should succeed without losing lock");
        }

        let loaded = load_discovered_models(Some(&disc_path)).unwrap();
        for i in 0..8 {
            let prov = format!("provider_{i}");
            assert!(
                loaded.contains_key(&prov),
                "lost concurrent write for {prov}; remaining keys: {:?}",
                loaded.keys().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn test_provider_has_explicit_endpoint_distinguishes_headers_default_proxy_and_custom() {
        let dir = crate::TempTestDir::new("explicit_endpoint");
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{
  "providers": {
    "anthropic": { "headers": { "X-Custom": "1" } },
    "openai": { "baseUrl": "https://api.openai.com/v1/" },
    "groq": { "baseUrl": "https://proxy.corp.local/groq/v1" },
    "ollama": { "baseUrl": "http://127.0.0.1:11434/v1" }
  }
}"#,
        )
        .unwrap();

        let reg = ModelRegistry::load_with_path(&models_path).unwrap();
        assert!(
            !reg.provider_has_explicit_endpoint("anthropic"),
            "header-only override must not count as an explicit endpoint"
        );
        assert!(
            !reg.provider_has_explicit_endpoint("openai"),
            "redundant built-in default baseUrl must not count as explicit custom endpoint"
        );
        assert!(
            reg.provider_has_explicit_endpoint("groq"),
            "non-default proxy URL for built-in provider should count as explicit endpoint"
        );
        assert!(
            reg.provider_has_explicit_endpoint("ollama"),
            "custom provider with baseUrl should count as explicit endpoint"
        );
    }

    #[test]
    fn test_remove_custom_provider_removes_entry_and_fails_closed_on_corrupt_json() {
        let dir = crate::TempTestDir::new("remove_custom_prov");
        let models_path = dir.path().join("models.json");

        assert!(
            !remove_custom_provider("ollama", Some(&models_path)).unwrap(),
            "missing models.json should return false without creating file"
        );
        assert!(!models_path.exists());

        upsert_custom_provider("Ollama", "http://127.0.0.1:11434/v1", None, Some(&models_path))
            .unwrap();
        upsert_custom_provider(
            "lmstudio",
            "http://127.0.0.1:1234/v1",
            None,
            Some(&models_path),
        )
        .unwrap();

        assert!(remove_custom_provider("OLLAMA", Some(&models_path)).unwrap());
        assert!(!remove_custom_provider("ollama", Some(&models_path)).unwrap());

        let reg = ModelRegistry::load_with_path(&models_path).unwrap();
        assert!(!reg.provider_has_explicit_endpoint("ollama"));
        assert!(reg.provider_has_explicit_endpoint("lmstudio"));

        std::fs::write(&models_path, "{ not valid json").unwrap();
        let err = remove_custom_provider("lmstudio", Some(&models_path)).unwrap_err();
        assert!(
            err.to_string().contains("parse"),
            "corrupt JSON must fail closed, got: {err}"
        );
    }

    #[test]
    fn test_with_config_file_lock_bare_relative_path_and_discovered_replaces_builtin() {
        struct LockCleanup(PathBuf);
        impl Drop for LockCleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let rel_path = Path::new("mehh_unit9_bare_rel_models.json");
        let _cleanup = LockCleanup(PathBuf::from("mehh_unit9_bare_rel_models.json.lock"));
        with_config_file_lock(rel_path, || Ok(()))
            .expect("bare relative filename must not fail create_dir_all(\"\")");

        let dir = crate::TempTestDir::new("disc_override_builtin");
        let models_path = dir.path().join("models.json");
        let disc_path = dir.path().join("discovered-models.json");

        let mut custom_sonnet = ModelRegistry::builtin_only()
            .resolve_exact("anthropic", "claude-sonnet-4-5")
            .unwrap();
        custom_sonnet.reasoning = false;
        save_discovered_provider_models("anthropic", &[custom_sonnet], Some(&disc_path)).unwrap();

        let reg = ModelRegistry::load_with_discovered(Some(&models_path), Some(&disc_path)).unwrap();
        let resolved = reg.resolve_exact("anthropic", "claude-sonnet-4-5").unwrap();
        assert!(
            !resolved.reasoning,
            "discovered override without models.json explicit model must update reg.resolve() entry"
        );
    }

    #[tokio::test]
    async fn test_cancellable_save_and_disconnect_provider_in_models_file() {
        let dir = crate::TempTestDir::new("disc_cancel_disconnect");
        let disc_path = dir.path().join("discovered-models.json");
        let models_path = dir.path().join("models.json");

        // Cancelled token prevents stale save_blocking task from writing.
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        let model = Model {
            id: "stale-m".into(),
            name: "Stale".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        let wrote = save_discovered_provider_models_cancellable_async(
            "openai",
            &[model],
            Some(&disc_path),
            cancelled,
        )
        .await
        .unwrap();
        assert!(!wrote);
        assert!(!disc_path.exists());

        // Built-in anthropic keeps headers, models, and custom proxy baseUrl, clearing only apiKey.
        std::fs::write(
            &models_path,
            r#"{
  "providers": {
    "anthropic": {
      "baseUrl": "https://proxy.example.com/anthropic",
      "apiKey": "sk-ant-cfg",
      "headers": { "X-Team": "core" },
      "models": [{ "id": "custom-claude" }]
    },
    "ollama": {
      "baseUrl": "http://127.0.0.1:11434/v1"
    }
  }
}"#,
        )
        .unwrap();

        assert!(disconnect_provider_in_models_file("anthropic", Some(&models_path)).unwrap());
        assert!(disconnect_provider_in_models_file("ollama", Some(&models_path)).unwrap());

        let reg = ModelRegistry::load_with_path(&models_path).unwrap();
        assert!(reg.provider_api_key("anthropic").is_none());
        let ant_cfg = reg.provider_config("anthropic").expect("built-in config must remain");
        assert_eq!(
            ant_cfg.base_url.as_deref(),
            Some("https://proxy.example.com/anthropic")
        );
        assert_eq!(
            ant_cfg
                .headers
                .as_ref()
                .and_then(|h| h.get("X-Team"))
                .map(String::as_str),
            Some("core")
        );
        assert_eq!(ant_cfg.models.len(), 1);
        assert!(reg.provider_config("ollama").is_none());
    }

    #[test]
    fn load_with_path_rejects_unsafe_urls_and_case_duplicate_providers() {
        let dir = crate::TempTestDir::new("models_url_dup");
        let path = dir.path().join("models.json");

        for bad_url in [
            "http://169.254.169.254/latest/meta-data",
            "http://metadata.google.internal/v1",
            "file:///etc/passwd",
            "https://user:pass@api.example.com/v1",
            "not-a-url",
        ] {
            std::fs::write(
                &path,
                serde_json::json!({
                    "providers": {
                        "evil": { "baseUrl": bad_url, "models": [{"id": "m1"}] }
                    }
                })
                .to_string(),
            )
            .unwrap();
            assert!(
                ModelRegistry::load_with_path(&path).is_err(),
                "must reject unsafe/malformed baseUrl '{bad_url}'"
            );
        }

        std::fs::write(
            &path,
            r#"{
                "providers": {
                    "OpenAI": { "baseUrl": "https://api.openai.com/v1" },
                    "openai": { "baseUrl": "https://evil.example.com/v1" }
                }
            }"#,
        )
        .unwrap();
        let err = ModelRegistry::load_with_path(&path).unwrap_err();
        assert!(
            err.to_string().to_ascii_lowercase().contains("duplicate"),
            "case-duplicate provider must fail closed, got: {err}"
        );
    }

    #[test]
    fn load_with_discovered_restores_builtin_base_url_and_trims_model_ids() {
        let dir = crate::TempTestDir::new("models_restore_builtin");
        let models_path = dir.path().join("models.json");
        let disc_path = dir.path().join("discovered-models.json");

        // Cached discovered model carries a stale proxy URL and padded model id.
        let stale_model = Model {
            id: "  claude-sonnet-4-5  ".to_string(),
            name: "Claude Sonnet 4.5".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://stale-proxy.example.com/v1".to_string(),
            ..Default::default()
        };
        save_discovered_provider_models("anthropic", &[stale_model], Some(&disc_path)).unwrap();

        // Reload with no anthropic entry in models.json (cfg_opt is None).
        std::fs::write(&models_path, r#"{"providers":{}}"#).unwrap();
        let reg = ModelRegistry::load_with_discovered(Some(&models_path), Some(&disc_path)).unwrap();
        let resolved = reg.resolve_exact("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(resolved.id, "claude-sonnet-4-5");
        assert_eq!(
            resolved.base_url,
            default_base_url("anthropic").unwrap(),
            "builtin base_url must be restored when cfg_opt is None"
        );
    }

    #[test]
    fn load_with_discovered_skips_empty_or_blocked_cached_urls_under_custom_provider_without_base_url() {
        let dir = crate::TempTestDir::new("models_disc_custom_validate");
        let models_path = dir.path().join("models.json");
        let disc_path = dir.path().join("discovered-models.json");

        // Custom provider in models.json declares headers but omits baseUrl.
        std::fs::write(
            &models_path,
            r#"{
                "providers": {
                    "custom-gw": {
                        "headers": { "X-Custom": "1" }
                    }
                }
            }"#,
        )
        .unwrap();

        let empty_url_model = Model {
            id: "empty-url-model".to_string(),
            name: "Empty URL".to_string(),
            api: "openai-completions".to_string(),
            provider: "custom-gw".to_string(),
            base_url: "   ".to_string(),
            ..Default::default()
        };
        let blocked_url_model = Model {
            id: "metadata-url-model".to_string(),
            name: "Blocked URL".to_string(),
            api: "openai-completions".to_string(),
            provider: "custom-gw".to_string(),
            base_url: "http://169.254.169.254/latest".to_string(),
            ..Default::default()
        };
        let valid_url_model = Model {
            id: "valid-url-model".to_string(),
            name: "Valid URL".to_string(),
            api: "openai-completions".to_string(),
            provider: "custom-gw".to_string(),
            base_url: "https://gw.example.com/v1/".to_string(),
            ..Default::default()
        };

        save_discovered_provider_models(
            "custom-gw",
            &[empty_url_model, blocked_url_model, valid_url_model],
            Some(&disc_path),
        )
        .unwrap();

        let reg = ModelRegistry::load_with_discovered(Some(&models_path), Some(&disc_path)).unwrap();
        assert!(
            reg.resolve_exact("custom-gw", "empty-url-model").is_none(),
            "empty cached base_url under custom provider without baseUrl must be skipped"
        );
        assert!(
            reg.resolve_exact("custom-gw", "metadata-url-model").is_none(),
            "blocked metadata cached base_url under custom provider without baseUrl must be skipped"
        );
        let ok = reg
            .resolve_exact("custom-gw", "valid-url-model")
            .expect("valid cached model should be loaded");
        assert_eq!(ok.base_url, "https://gw.example.com/v1");
        assert_eq!(
            ok.headers.as_ref().and_then(|h| h.get("X-Custom")).map(String::as_str),
            Some("1")
        );
    }
}
