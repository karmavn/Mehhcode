use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mehh_ai::catalog::{builtin_models, default_base_url};
use mehh_ai::types::{Model, ModelCompat, ModelCost, ThinkingLevelMap};
use serde::{Deserialize, Serialize};

/// Returns the default user models configuration path (`~/.mehh/agent/models.json`).
pub fn default_models_path() -> Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default models path: neither USERPROFILE nor HOME is set"
            )
        })?;
    Ok(home.join(".mehh").join("agent").join("models.json"))
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
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::builtin_only()
    }
}

impl ModelRegistry {
    /// Creates a registry containing only the built-in catalog models.
    pub fn builtin_only() -> Self {
        Self {
            models: builtin_models(),
            provider_api_keys: HashMap::new(),
            provider_configs: HashMap::new(),
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

        for (provider_name, cfg) in parsed.providers {
            let provider_trimmed = provider_name.trim().to_string();
            if provider_trimmed.is_empty() {
                bail!("Provider key in models config '{}' cannot be empty", path.display());
            }

            let provider_key = provider_trimmed.to_ascii_lowercase();
            if let Some(key) = cfg.api_key.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                provider_api_keys.insert(provider_key.clone(), key.to_string());
            }

            let resolved_provider_base = cfg
                .base_url
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| default_base_url(&provider_trimmed).map(|s| s.to_string()));

            if cfg.base_url.as_ref().is_some_and(|s| s.trim().is_empty()) {
                bail!(
                    "Provider '{}' in models config '{}' has an empty baseUrl",
                    provider_trimmed,
                    path.display()
                );
            }

            provider_configs.insert(
                provider_key,
                ProviderConfig {
                    base_url: resolved_provider_base.clone(),
                    api_key: cfg.api_key.clone(),
                    api: cfg.api.clone(),
                    headers: cfg.headers.clone(),
                    models: cfg.models.clone(),
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

            for entry in cfg.models {
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
                    headers: if headers.is_empty() { None } else { Some(headers) },
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
        assert!(reg.resolve_exact("openai", "nonexistent-model-xyz").is_none());
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

        let map = model.thinking_level_map.as_ref().expect("thinking_level_map present");
        assert_eq!(map.get("low"), Some(&Some("min".to_string())));
        assert_eq!(map.get("high"), Some(&Some("max".to_string())));
        assert_eq!(map.get("off"), Some(&None));
    }
}
