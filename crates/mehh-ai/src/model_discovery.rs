//! Provider metadata specifications, thinking profiles, and live model discovery.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Deserialize;

use crate::anthropic::is_oauth_token;
use crate::catalog::builtin_models;
use crate::http_safety::{check_url_host_policy, normalize_and_validate_http_url, DnsPolicy};
use crate::oauth::redact_error_body;
use crate::types::{Model, ModelCost, ThinkingLevel};

const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MODEL_LIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_DISCOVERY_ERROR_BYTES: usize = 32 * 1024;
const MAX_DISCOVERY_ERROR_CHARS: usize = 512;

/// Authentication mechanisms supported by a known provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthMethod {
    ApiKey,
    OAuth,
}

/// Canonical metadata for a built-in LLM provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub api: &'static str,
    pub default_base_url: &'static str,
    pub env_var: &'static str,
    pub auth_methods: &'static [ProviderAuthMethod],
    pub supports_live_models: bool,
}

/// Ordered catalog of first-class providers shown in the connect UI.
pub const KNOWN_PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        display_name: "Anthropic (Claude)",
        api: "anthropic-messages",
        default_base_url: "https://api.anthropic.com/v1",
        env_var: "ANTHROPIC_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey, ProviderAuthMethod::OAuth],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "openai",
        display_name: "OpenAI",
        api: "openai-completions",
        default_base_url: "https://api.openai.com/v1",
        env_var: "OPENAI_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey, ProviderAuthMethod::OAuth],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "google",
        display_name: "Google (Gemini)",
        api: "google-generative-ai",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta",
        env_var: "GEMINI_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "openrouter",
        display_name: "OpenRouter",
        api: "openai-completions",
        default_base_url: "https://openrouter.ai/api/v1",
        env_var: "OPENROUTER_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "github-copilot",
        display_name: "GitHub Copilot",
        api: "openai-completions",
        default_base_url: "https://api.githubcopilot.com",
        env_var: "COPILOT_GITHUB_TOKEN",
        auth_methods: &[ProviderAuthMethod::OAuth, ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "deepseek",
        display_name: "DeepSeek",
        api: "openai-completions",
        default_base_url: "https://api.deepseek.com/v1",
        env_var: "DEEPSEEK_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "groq",
        display_name: "Groq",
        api: "openai-completions",
        default_base_url: "https://api.groq.com/openai/v1",
        env_var: "GROQ_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "xai",
        display_name: "xAI (Grok)",
        api: "openai-completions",
        default_base_url: "https://api.x.ai/v1",
        env_var: "XAI_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "mistral",
        display_name: "Mistral",
        api: "mistral-conversations",
        default_base_url: "https://api.mistral.ai/v1",
        env_var: "MISTRAL_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "together",
        display_name: "Together AI",
        api: "openai-completions",
        default_base_url: "https://api.together.xyz/v1",
        env_var: "TOGETHER_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "fireworks",
        display_name: "Fireworks AI",
        api: "openai-completions",
        default_base_url: "https://api.fireworks.ai/inference/v1",
        env_var: "FIREWORKS_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
    ProviderSpec {
        id: "cerebras",
        display_name: "Cerebras",
        api: "openai-completions",
        default_base_url: "https://api.cerebras.ai/v1",
        env_var: "CEREBRAS_API_KEY",
        auth_methods: &[ProviderAuthMethod::ApiKey],
        supports_live_models: true,
    },
];

/// Looks up a built-in provider specification by case-insensitive ID.
pub fn find_provider_spec(provider: &str) -> Option<&'static ProviderSpec> {
    let trimmed = provider.trim();
    KNOWN_PROVIDERS
        .iter()
        .find(|spec| spec.id.eq_ignore_ascii_case(trimmed))
}

/// Thinking capability profile derived from a model's provider, API, and ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelThinkingProfile {
    pub supported: bool,
    pub levels: Vec<ThinkingLevel>,
    pub default_level: ThinkingLevel,
    pub can_disable: bool,
}

/// Returns the valid thinking levels and default level for a model.
pub fn model_thinking_profile(model: &Model) -> ModelThinkingProfile {
    if model.provider.eq_ignore_ascii_case("faux") {
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            default_level: ThinkingLevel::Off,
            can_disable: true,
        };
    }

    if !model.reasoning {
        return ModelThinkingProfile {
            supported: false,
            levels: vec![ThinkingLevel::Off],
            default_level: ThinkingLevel::Off,
            can_disable: true,
        };
    }

    let id_lower = model.id.trim().to_ascii_lowercase();
    let bare_id = id_lower.rsplit('/').next().unwrap_or(&id_lower);

    if model.provider.eq_ignore_ascii_case("anthropic")
        || model.api.eq_ignore_ascii_case("anthropic-messages")
    {
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Max,
            ],
            default_level: ThinkingLevel::Medium,
            can_disable: true,
        };
    }

    if (model.provider.eq_ignore_ascii_case("openai")
        || model.provider.eq_ignore_ascii_case("openai-responses")
        || model.api.eq_ignore_ascii_case("openai-responses"))
        && is_openai_o_series(bare_id)
    {
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            default_level: ThinkingLevel::Medium,
            can_disable: false,
        };
    }

    if (model.provider.eq_ignore_ascii_case("openai")
        || model.provider.eq_ignore_ascii_case("openai-responses")
        || model.api.eq_ignore_ascii_case("openai-responses"))
        && bare_id.starts_with("gpt-5")
    {
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            default_level: ThinkingLevel::Medium,
            can_disable: true,
        };
    }

    if model.provider.eq_ignore_ascii_case("google")
        || model.provider.eq_ignore_ascii_case("google-vertex")
        || model.api.eq_ignore_ascii_case("google-generative-ai")
        || model.api.eq_ignore_ascii_case("google-vertex")
    {
        let pro = bare_id.starts_with("gemini-2.5-pro") || bare_id.starts_with("gemini-3-pro");
        if pro {
            return ModelThinkingProfile {
                supported: true,
                levels: vec![
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                ],
                default_level: ThinkingLevel::Medium,
                can_disable: false,
            };
        }
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            default_level: ThinkingLevel::Medium,
            can_disable: true,
        };
    }

    if !model.provider.eq_ignore_ascii_case("openrouter")
        && (bare_id == "deepseek-reasoner"
            || bare_id.starts_with("deepseek-reasoner-")
            || bare_id.starts_with("deepseek-r1"))
    {
        return ModelThinkingProfile {
            supported: true,
            levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            default_level: ThinkingLevel::Medium,
            can_disable: false,
        };
    }

    ModelThinkingProfile {
        supported: true,
        levels: vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
        default_level: ThinkingLevel::Medium,
        can_disable: true,
    }
}

fn is_openai_o_series(bare_lower: &str) -> bool {
    matches!(bare_lower, "o1" | "o3" | "o4")
        || bare_lower.starts_with("o1-")
        || bare_lower.starts_with("o3-")
        || bare_lower.starts_with("o4-")
}

/// Infers whether a model ID supports extended thinking / reasoning from naming conventions.
pub fn infer_reasoning_capability(_provider: &str, model_id: &str) -> bool {
    let id_lower = model_id.trim().to_ascii_lowercase();
    if id_lower.is_empty() {
        return false;
    }
    let bare = id_lower.rsplit('/').next().unwrap_or(&id_lower);

    const UTILITY_EXCLUDES: &[&str] = &[
        "embedding",
        "text-embedding",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "gpt-image",
        "gpt-audio",
        "gpt-realtime",
        "sora",
        "babbage",
        "davinci",
        "imagen",
        "veo",
    ];
    if UTILITY_EXCLUDES.iter().any(|kw| bare.contains(kw)) {
        return false;
    }

    if bare.contains("thinking") || bare.contains("reasoning") || bare.contains("reasoner") {
        return true;
    }

    const NON_REASONING_PREFIXES: &[&str] = &[
        "claude-3-5-",
        "claude-3.5-",
        "claude-3-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
        "gpt-4o",
        "gpt-4.1",
        "gpt-4-turbo",
        "gpt-4-",
        "gpt-3.5",
        "gemini-1.",
        "gemini-2.0-",
        "deepseek-chat",
        "deepseek-v3",
    ];
    if NON_REASONING_PREFIXES
        .iter()
        .any(|prefix| bare == prefix.trim_end_matches('-') || bare.starts_with(prefix))
    {
        return false;
    }

    if bare.starts_with("claude-3-7")
        || bare.starts_with("claude-3.7")
        || bare.starts_with("claude-sonnet-4")
        || bare.starts_with("claude-opus-4")
        || bare.starts_with("claude-haiku-4-5")
        || bare.starts_with("claude-haiku-4.5")
    {
        return true;
    }

    if is_openai_o_series(bare) || bare.starts_with("gpt-5") {
        return true;
    }

    if bare.starts_with("gemini-2.5") || bare.starts_with("gemini-3") {
        return true;
    }

    if bare.starts_with("deepseek-r1")
        || bare.contains("-r1")
        || bare.starts_with("grok-3")
        || bare.starts_with("grok-4")
        || bare.contains("magistral")
        || bare.starts_with("qwq")
        || bare.contains("-qwq")
        || bare.contains("qwen3")
        || bare.contains("qwen-3")
    {
        return true;
    }

    false
}

fn strip_trailing_date_suffix(id: &str) -> &str {
    let bytes = id.as_bytes();
    // Match trailing `-YYYY-MM-DD` (11 bytes)
    if bytes.len() > 11 && bytes[bytes.len() - 11] == b'-' {
        let tail = &bytes[bytes.len() - 10..];
        if tail[0..4].iter().all(u8::is_ascii_digit)
            && tail[4] == b'-'
            && tail[5..7].iter().all(u8::is_ascii_digit)
            && tail[7] == b'-'
            && tail[8..10].iter().all(u8::is_ascii_digit)
        {
            return &id[..id.len() - 11];
        }
    }
    // Match trailing `-YYYYMMDD` (9 bytes)
    if bytes.len() > 9 && bytes[bytes.len() - 9] == b'-' {
        let tail = &bytes[bytes.len() - 8..];
        if tail.iter().all(u8::is_ascii_digit) {
            return &id[..id.len() - 9];
        }
    }
    id
}

fn family_defaults(provider: &str, model_id: &str) -> (u64, u64, Vec<String>) {
    let lower = model_id.trim().to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(&lower);
    let vision = vec!["text".to_string(), "image".to_string()];
    let text_only = vec!["text".to_string()];

    if bare.starts_with("claude-sonnet-4")
        || bare.starts_with("claude-opus-4")
        || bare.starts_with("claude-haiku-4")
        || bare.starts_with("claude-3-7")
        || bare.starts_with("claude-3.7")
        || provider.eq_ignore_ascii_case("anthropic")
    {
        return (200_000, 64_000, vision);
    }

    if bare.starts_with("gemini-2.5")
        || bare.starts_with("gemini-3")
        || provider.eq_ignore_ascii_case("google")
    {
        return (1_000_000, 65_536, vision);
    }

    if is_openai_o_series(bare) || bare.starts_with("gpt-5") {
        return (200_000, 32_768, vision);
    }

    if bare.starts_with("gpt-4") || bare.starts_with("grok-3") || bare.starts_with("grok-4") {
        return (128_000, 16_384, vision);
    }

    (128_000, 16_384, text_only)
}

/// Enriches a freshly fetched provider model with verified catalog metadata or family defaults.
pub fn enrich_discovered_model(
    provider: &str,
    raw_model: Model,
    builtin_catalog: &[Model],
) -> Model {
    let provider_norm = if provider.trim().is_empty() {
        raw_model.provider.trim().to_ascii_lowercase()
    } else {
        provider.trim().to_ascii_lowercase()
    };

    let canonical_api = find_provider_spec(&provider_norm)
        .map(|spec| spec.api)
        .unwrap_or("openai-completions");

    let catalog_match = builtin_catalog
        .iter()
        .find(|m| {
            m.provider.eq_ignore_ascii_case(&provider_norm)
                && m.id.eq_ignore_ascii_case(&raw_model.id)
        })
        .or_else(|| {
            let stripped = strip_trailing_date_suffix(&raw_model.id);
            if stripped.len() == raw_model.id.len() {
                return None;
            }
            builtin_catalog.iter().find(|m| {
                m.provider.eq_ignore_ascii_case(&provider_norm)
                    && m.id.eq_ignore_ascii_case(stripped)
            })
        });

    if let Some(cat) = catalog_match {
        let mut enriched = cat.clone();
        enriched.id = raw_model.id;
        enriched.provider = provider_norm;
        if !raw_model.name.trim().is_empty() {
            enriched.name = raw_model.name;
        }
        if !raw_model.base_url.trim().is_empty() {
            enriched.base_url = raw_model.base_url;
        }
        if raw_model.reasoning {
            enriched.reasoning = true;
        }
        if raw_model.headers.is_some() {
            enriched.headers = raw_model.headers;
        }
        return enriched;
    }

    let (default_ctx, default_max, default_input) =
        family_defaults(&provider_norm, &raw_model.id);
    let mut enriched = raw_model;
    enriched.provider = provider_norm.clone();
    if enriched.name.trim().is_empty() {
        enriched.name = enriched.id.clone();
    }
    if enriched.api.trim().is_empty() {
        enriched.api = canonical_api.to_string();
    }
    if enriched.base_url.trim().is_empty() {
        if let Some(spec) = find_provider_spec(&provider_norm) {
            enriched.base_url = spec.default_base_url.to_string();
        }
    }
    if !enriched.reasoning {
        enriched.reasoning = infer_reasoning_capability(&provider_norm, &enriched.id);
    }
    if enriched.context_window == 0 {
        enriched.context_window = default_ctx;
    }
    if enriched.max_tokens == 0 {
        enriched.max_tokens = default_max;
    }
    if enriched.input.is_empty() {
        enriched.input = default_input;
    }
    enriched
}

async fn read_bounded_bytes(mut resp: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?
    {
        let remaining = max_bytes.saturating_sub(buf.len());
        if remaining == 0 {
            return Err(format!("Response body exceeded {max_bytes} byte limit"));
        }
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            return Err(format!("Response body exceeded {max_bytes} byte limit"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn read_error_snippet(resp: reqwest::Response, secrets: &[&str]) -> String {
    let mut buf = Vec::new();
    let mut resp = resp;
    while let Ok(Some(chunk)) = resp.chunk().await {
        let remaining = MAX_DISCOVERY_ERROR_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        buf.extend_from_slice(&chunk[..take]);
        if take < chunk.len() || buf.len() >= MAX_DISCOVERY_ERROR_BYTES {
            break;
        }
    }
    let raw = String::from_utf8_lossy(&buf);
    let redacted = redact_error_body(&raw, secrets);
    let mut chars = redacted.chars();
    let truncated: String = chars.by_ref().take(MAX_DISCOVERY_ERROR_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsResponse {
    #[serde(default)]
    data: Vec<AnthropicModelItem>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelItem {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleModelsResponse {
    #[serde(default)]
    models: Vec<GoogleModelItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleModelItem {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    output_token_limit: Option<u64>,
    #[serde(default)]
    thinking: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelsResponse {
    #[serde(default)]
    data: Vec<OpenRouterModelItem>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OpenAiModelsEnvelope {
    Wrapped { data: Vec<OpenAiModelItem> },
    Array(Vec<OpenAiModelItem>),
}

#[derive(Debug, Deserialize)]
struct OpenAiModelItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

fn is_non_chat_openai_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    const EXCLUDE_PREFIXES_OR_SUBSTRS: &[&str] = &[
        "whisper",
        "tts-",
        "dall-e",
        "text-embedding",
        "omni-moderation",
        "text-moderation",
        "babbage",
        "davinci",
        "gpt-image",
        "sora",
        "gpt-audio",
        "gpt-realtime",
    ];
    EXCLUDE_PREFIXES_OR_SUBSTRS
        .iter()
        .any(|kw| lower.contains(kw))
}

fn parse_per_million_price(raw: Option<&str>) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|per_tok| per_tok * 1_000_000.0)
        .unwrap_or(0.0)
}

fn parse_anthropic_models(
    bytes: &[u8],
    provider: &str,
    base_url: &str,
    api: &str,
) -> Result<Vec<Model>, String> {
    let items: Vec<AnthropicModelItem> =
        if let Ok(wrapped) = serde_json::from_slice::<AnthropicModelsResponse>(bytes) {
            wrapped.data
        } else {
            serde_json::from_slice::<Vec<AnthropicModelItem>>(bytes)
                .map_err(|e| format!("Failed to parse Anthropic models response: {e}"))?
        };
    Ok(items
        .into_iter()
        .filter(|item| !item.id.trim().is_empty())
        .map(|item| Model {
            id: item.id.clone(),
            name: item.display_name.unwrap_or(item.id),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            ..Default::default()
        })
        .collect())
}

fn parse_google_models(
    bytes: &[u8],
    provider: &str,
    base_url: &str,
    api: &str,
) -> Result<Vec<Model>, String> {
    let parsed: GoogleModelsResponse = serde_json::from_slice(bytes)
        .map_err(|e| format!("Failed to parse Google models response: {e}"))?;

    let mut models = Vec::new();
    for item in parsed.models {
        let supports_generate = item
            .supported_generation_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case("generateContent"));
        if !supports_generate {
            continue;
        }
        let id = item
            .name
            .strip_prefix("models/")
            .unwrap_or(&item.name)
            .trim()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let lower = id.to_ascii_lowercase();
        if ["embedding", "imagen", "aqa", "veo", "tts"]
            .iter()
            .any(|kw| lower.contains(kw))
        {
            continue;
        }
        let explicit_thinking = match &item.thinking {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::Object(_)) => true,
            _ => false,
        };
        models.push(Model {
            id: id.clone(),
            name: item
                .display_name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: explicit_thinking,
            context_window: item.input_token_limit.unwrap_or(0),
            max_tokens: item.output_token_limit.unwrap_or(0),
            ..Default::default()
        });
    }
    Ok(models)
}

fn parse_openrouter_models(
    bytes: &[u8],
    provider: &str,
    base_url: &str,
    api: &str,
) -> Result<Vec<Model>, String> {
    let parsed: OpenRouterModelsResponse = serde_json::from_slice(bytes)
        .map_err(|e| format!("Failed to parse OpenRouter models response: {e}"))?;

    Ok(parsed
        .data
        .into_iter()
        .filter(|item| !item.id.trim().is_empty())
        .map(|item| {
            let reasoning = item.supported_parameters.iter().any(|p| {
                p.eq_ignore_ascii_case("reasoning") || p.eq_ignore_ascii_case("include_reasoning")
            });
            let cost = if let Some(pricing) = &item.pricing {
                ModelCost {
                    input: parse_per_million_price(pricing.prompt.as_deref()),
                    output: parse_per_million_price(pricing.completion.as_deref()),
                    cache_read: 0.0,
                    cache_write: 0.0,
                }
            } else {
                ModelCost::default()
            };
            let input: Vec<String> = item
                .architecture
                .map(|arch| {
                    arch.input_modalities
                        .into_iter()
                        .filter(|m| matches!(m.as_str(), "text" | "image"))
                        .collect()
                })
                .unwrap_or_default();
            Model {
                id: item.id.clone(),
                name: item
                    .name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(item.id),
                api: api.to_string(),
                provider: provider.to_string(),
                base_url: base_url.to_string(),
                reasoning,
                input,
                cost,
                context_window: item.context_length.unwrap_or(0),
                max_tokens: item
                    .top_provider
                    .and_then(|tp| tp.max_completion_tokens)
                    .unwrap_or(0),
                ..Default::default()
            }
        })
        .collect())
}

fn parse_openai_models(
    bytes: &[u8],
    provider: &str,
    base_url: &str,
    api: &str,
) -> Result<Vec<Model>, String> {
    let items = match serde_json::from_slice::<OpenAiModelsEnvelope>(bytes)
        .map_err(|e| format!("Failed to parse OpenAI-compatible models response: {e}"))?
    {
        OpenAiModelsEnvelope::Wrapped { data } => data,
        OpenAiModelsEnvelope::Array(arr) => arr,
    };
    let is_openai = provider.eq_ignore_ascii_case("openai")
        || provider.eq_ignore_ascii_case("openai-responses");

    Ok(items
        .into_iter()
        .filter(|item| !item.id.trim().is_empty())
        .filter(|item| !is_openai || !is_non_chat_openai_id(&item.id))
        .map(|item| Model {
            id: item.id.clone(),
            name: item
                .name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(item.id),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            context_window: item.context_length.unwrap_or(0),
            max_tokens: item.max_tokens.unwrap_or(0),
            ..Default::default()
        })
        .collect())
}

/// Fetches and enriches the live model list from a provider API endpoint.
pub async fn fetch_provider_models(
    client: &reqwest::Client,
    provider: &str,
    base_url: &str,
    api_key: Option<&str>,
    api_override: Option<&str>,
) -> Result<Vec<Model>, String> {
    let provider_norm = provider.trim().to_ascii_lowercase();
    if provider_norm.is_empty() {
        return Err("Provider ID must not be empty".to_string());
    }

    let parsed_url = normalize_and_validate_http_url(base_url)?;
    check_url_host_policy(DnsPolicy::AllowPrivate, &parsed_url)?;

    let clean_base = parsed_url.as_str().trim_end_matches('/').to_string();
    let trimmed_key = api_key.map(str::trim).filter(|k| !k.is_empty());

    let canonical_api = api_override
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .or_else(|| find_provider_spec(&provider_norm).map(|s| s.api))
        .unwrap_or("openai-completions");

    let (request_url, mut req) = match provider_norm.as_str() {
        "anthropic" => {
            let needs_v1 = !clean_base.ends_with("/v1")
                && parsed_url
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case("api.anthropic.com"))
                && (parsed_url.path().is_empty() || parsed_url.path() == "/");
            let url = if needs_v1 {
                format!("{clean_base}/v1/models?limit=100")
            } else {
                format!("{clean_base}/models?limit=100")
            };
            let mut rb = client
                .get(&url)
                .header("anthropic-version", "2023-06-01");
            if let Some(key) = trimmed_key {
                if is_oauth_token(key) {
                    rb = rb
                        .bearer_auth(key)
                        .header("anthropic-beta", "oauth-2025-04-20");
                } else {
                    rb = rb.header("x-api-key", key);
                }
            }
            (url, rb)
        }
        "google" => {
            let url = format!("{clean_base}/models?pageSize=200");
            let mut rb = client.get(&url);
            if let Some(key) = trimmed_key {
                rb = rb.header("x-goog-api-key", key);
            }
            (url, rb)
        }
        "openrouter" => {
            let url = format!("{clean_base}/models");
            let mut rb = client.get(&url);
            if let Some(key) = trimmed_key {
                rb = rb.bearer_auth(key);
            }
            (url, rb)
        }
        _ => {
            let url = format!("{clean_base}/models");
            let mut rb = client.get(&url);
            if let Some(key) = trimmed_key {
                rb = rb.bearer_auth(key);
            }
            if provider_norm == "github-copilot" {
                rb = rb
                    .header("Copilot-Integration-Id", "vscode-chat")
                    .header("Editor-Version", "mehh/0.1.0");
            }
            (url, rb)
        }
    };

    req = req.timeout(MODEL_DISCOVERY_TIMEOUT);

    let resp = tokio::time::timeout(MODEL_DISCOVERY_TIMEOUT, req.send())
        .await
        .map_err(|_| format!("Timed out fetching models from {provider_norm} ({request_url})"))?
        .map_err(|e| format!("Failed to fetch models from {provider_norm}: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let secret_slice: [&str; 1] = [trimmed_key.unwrap_or_default()];
        let secrets = if trimmed_key.is_some() {
            &secret_slice[..]
        } else {
            &[]
        };
        let snippet = read_error_snippet(resp, secrets).await;
        return Err(format!(
            "Provider {provider_norm} returned HTTP {status}: {snippet}"
        ));
    }

    let bytes = tokio::time::timeout(
        MODEL_DISCOVERY_TIMEOUT,
        read_bounded_bytes(resp, MAX_MODEL_LIST_BYTES),
    )
    .await
    .map_err(|_| format!("Timed out reading models from {provider_norm}"))??;

    let raw_models = match provider_norm.as_str() {
        "anthropic" => parse_anthropic_models(&bytes, &provider_norm, &clean_base, canonical_api)?,
        "google" => parse_google_models(&bytes, &provider_norm, &clean_base, canonical_api)?,
        "openrouter" => parse_openrouter_models(&bytes, &provider_norm, &clean_base, canonical_api)?,
        _ => parse_openai_models(&bytes, &provider_norm, &clean_base, canonical_api)?,
    };

    let catalog = builtin_models();
    let mut seen: HashSet<String> = HashSet::new();
    let mut enriched = Vec::with_capacity(raw_models.len());

    // Keep featured catalog order ahead of extra discovered models? Wait: preserve response order
    // while deduplicating case-insensitively and preferring earlier occurrence cleanly.
    let mut catalog_index: HashMap<String, usize> = HashMap::new();
    for (idx, m) in catalog.iter().enumerate() {
        if m.provider.eq_ignore_ascii_case(&provider_norm) {
            catalog_index.entry(m.id.to_ascii_lowercase()).or_insert(idx);
        }
    }

    for raw in raw_models {
        let key = raw.id.trim().to_ascii_lowercase();
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        let mut model = enrich_discovered_model(&provider_norm, raw, &catalog);
        if let Some(api) = api_override.map(str::trim).filter(|a| !a.is_empty()) {
            model.api = api.to_string();
        }
        enriched.push(model);
    }

    // Place built-in featured catalog matches first (preserving catalog order), followed by the
    // remaining discovered models in API order.
    enriched.sort_by_key(|m| {
        let key = strip_trailing_date_suffix(&m.id).to_ascii_lowercase();
        catalog_index
            .get(&m.id.to_ascii_lowercase())
            .or_else(|| catalog_index.get(&key))
            .copied()
            .unwrap_or(usize::MAX)
    });

    Ok(enriched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_http_server(
        status: &str,
        body: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut req_buf = vec![0u8; 4096];
            let n = socket.read(&mut req_buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&req_buf[..n]).to_string();
            let _ = tx.send(req_str);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn test_fetch_anthropic_models_uses_expected_headers_and_enriches() {
        let body = serde_json::json!({
            "data": [
                { "id": "claude-sonnet-4-5-20250929", "display_name": "Claude Sonnet 4.5" },
                { "id": "claude-opus-4-6", "display_name": "Claude Opus 4.6" },
                { "id": "claude-3-5-haiku-20241022", "display_name": "Claude 3.5 Haiku" }
            ]
        })
        .to_string();
        let (base_url, rx) = spawn_mock_http_server("200 OK", &body).await;
        let client = reqwest::Client::new();

        let models = fetch_provider_models(
            &client,
            "anthropic",
            &base_url,
            Some("sk-ant-api03-secret"),
            None,
        )
        .await
        .expect("anthropic fetch should succeed");

        let req_raw = rx.await.unwrap();
        assert!(req_raw.starts_with("GET /models?limit=100 "));
        let req_lower = req_raw.to_ascii_lowercase();
        assert!(req_lower.contains("x-api-key: sk-ant-api03-secret"));
        assert!(req_lower.contains("anthropic-version: 2023-06-01"));

        let sonnet = models
            .iter()
            .find(|m| m.id == "claude-sonnet-4-5-20250929")
            .unwrap();
        assert!(sonnet.reasoning);
        assert_eq!(sonnet.context_window, 200_000);
        assert_eq!(sonnet.api, "anthropic-messages");

        let opus = models.iter().find(|m| m.id == "claude-opus-4-6").unwrap();
        assert!(opus.reasoning);
        let haiku = models
            .iter()
            .find(|m| m.id == "claude-3-5-haiku-20241022")
            .unwrap();
        assert!(!haiku.reasoning);
    }

    #[tokio::test]
    async fn test_fetch_anthropic_oauth_token_uses_bearer_and_beta_header() {
        let body = serde_json::json!({
            "data": [{ "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5" }]
        })
        .to_string();
        let (base_url, rx) = spawn_mock_http_server("200 OK", &body).await;
        let client = reqwest::Client::new();

        let _ = fetch_provider_models(
            &client,
            "anthropic",
            &base_url,
            Some("sk-ant-oat01-oauth-token"),
            None,
        )
        .await
        .expect("anthropic oauth fetch should succeed");

        let req_lower = rx.await.unwrap().to_ascii_lowercase();
        assert!(req_lower.contains("authorization: bearer sk-ant-oat01-oauth-token"));
        assert!(req_lower.contains("anthropic-beta: oauth-2025-04-20"));
    }

    #[tokio::test]
    async fn test_fetch_google_models_strips_prefix_filters_non_chat_and_reads_thinking() {
        let body = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-2.5-pro",
                    "displayName": "Gemini 2.5 Pro",
                    "supportedGenerationMethods": ["generateContent", "countTokens"],
                    "inputTokenLimit": 1048576,
                    "outputTokenLimit": 65536,
                    "thinking": true
                },
                {
                    "name": "models/text-embedding-004",
                    "displayName": "Text Embedding 004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/imagen-3.0-generate-002",
                    "displayName": "Imagen 3",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        })
        .to_string();
        let (base_url, rx) = spawn_mock_http_server("200 OK", &body).await;
        let client = reqwest::Client::new();

        let models =
            fetch_provider_models(&client, "google", &base_url, Some("AIzaTestKey"), None)
                .await
                .expect("google fetch should succeed");

        let req_raw = rx.await.unwrap();
        assert!(req_raw.starts_with("GET /models?pageSize=200 "));
        assert!(!req_raw.contains("AIzaTestKey\r\n") || req_raw.to_ascii_lowercase().contains("x-goog-api-key: aizatestkey"));
        assert!(!req_raw.contains("?key=") && !req_raw.contains("&key="));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-2.5-pro");
        assert!(models[0].reasoning);
        assert_eq!(models[0].api, "google-generative-ai");
    }

    #[tokio::test]
    async fn test_fetch_openrouter_models_parses_reasoning_and_pricing() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-opus-4.6",
                    "name": "Claude Opus 4.6",
                    "context_length": 200000,
                    "top_provider": { "max_completion_tokens": 32768 },
                    "pricing": { "prompt": "0.000005", "completion": "0.000025" },
                    "supported_parameters": ["temperature", "reasoning", "tools"],
                    "architecture": { "input_modalities": ["text", "image"] }
                },
                {
                    "id": "meta-llama/llama-3.3-70b-instruct",
                    "name": "Llama 3.3 70B",
                    "context_length": 131072,
                    "pricing": { "prompt": "0", "completion": "0" },
                    "supported_parameters": ["temperature"],
                    "architecture": { "input_modalities": ["text"] }
                }
            ]
        })
        .to_string();
        let (base_url, _rx) = spawn_mock_http_server("200 OK", &body).await;
        let client = reqwest::Client::new();

        let models =
            fetch_provider_models(&client, "openrouter", &base_url, Some("sk-or-test"), None)
                .await
                .expect("openrouter fetch should succeed");

        assert_eq!(models.len(), 2);
        let opus = models
            .iter()
            .find(|m| m.id == "anthropic/claude-opus-4.6")
            .unwrap();
        assert!(opus.reasoning);
        assert!((opus.cost.input - 5.0).abs() < 1e-6);
        assert!((opus.cost.output - 25.0).abs() < 1e-6);
        assert_eq!(opus.max_tokens, 32768);
        assert_eq!(opus.input, vec!["text", "image"]);

        let llama = models
            .iter()
            .find(|m| m.id == "meta-llama/llama-3.3-70b-instruct")
            .unwrap();
        assert!(!llama.reasoning);
    }

    #[tokio::test]
    async fn test_fetch_openai_and_raw_array_models_filter_non_chat_and_dedup() {
        let body = serde_json::json!({
            "data": [
                { "id": "gpt-4o" },
                { "id": "whisper-1" },
                { "id": "text-embedding-3-large" },
                { "id": "o3" },
                { "id": "gpt-5" },
                { "id": "O3" }
            ]
        })
        .to_string();
        let (base_url, _rx) = spawn_mock_http_server("200 OK", &body).await;
        let client = reqwest::Client::new();

        let models =
            fetch_provider_models(&client, "openai", &base_url, Some("sk-openai"), None)
                .await
                .expect("openai fetch should succeed");

        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(!ids.contains(&"whisper-1"));
        assert!(!ids.contains(&"text-embedding-3-large"));
        assert_eq!(ids.iter().filter(|id| id.eq_ignore_ascii_case("o3")).count(), 1);
        assert!(models.iter().find(|m| m.id == "o3").unwrap().reasoning);
        assert!(models.iter().find(|m| m.id == "gpt-5").unwrap().reasoning);
        assert!(!models.iter().find(|m| m.id == "gpt-4o").unwrap().reasoning);

        // Top-level array format (e.g., Together AI)
        let together_body = serde_json::json!([
            { "id": "deepseek-ai/DeepSeek-R1" },
            { "id": "meta-llama/Llama-3.3-70B-Instruct-Turbo" }
        ])
        .to_string();
        let (together_url, _rx2) = spawn_mock_http_server("200 OK", &together_body).await;
        let together_models =
            fetch_provider_models(&client, "together", &together_url, Some("tog-key"), None)
                .await
                .expect("together array fetch should succeed");
        assert_eq!(together_models.len(), 2);
        assert!(together_models
            .iter()
            .find(|m| m.id == "deepseek-ai/DeepSeek-R1")
            .unwrap()
            .reasoning);
    }

    #[tokio::test]
    async fn test_fetch_provider_models_rejects_metadata_ip_and_handles_http_errors() {
        let client = reqwest::Client::new();
        let ssrf = fetch_provider_models(
            &client,
            "openai",
            "http://169.254.169.254/v1",
            Some("sk-test"),
            None,
        )
        .await;
        assert!(ssrf.is_err());

        let (base_url, _rx) = spawn_mock_http_server(
            "401 Unauthorized",
            r#"{"error":{"message":"invalid bearer sk-secret123"}}"#,
        )
        .await;
        let err = fetch_provider_models(&client, "openai", &base_url, Some("sk-secret123"), None)
            .await
            .expect_err("401 should fail cleanly");
        assert!(err.contains("401"));
        assert!(!err.contains("sk-secret123"));
    }

    #[test]
    fn test_enrich_and_thinking_profiles() {
        let catalog = builtin_models();

        let claude_opus = enrich_discovered_model(
            "anthropic",
            Model {
                id: "claude-opus-4-6".to_string(),
                ..Default::default()
            },
            &catalog,
        );
        assert!(claude_opus.reasoning);
        assert_eq!(claude_opus.context_window, 200_000);
        let p_claude = model_thinking_profile(&claude_opus);
        assert!(p_claude.supported);
        assert!(p_claude.can_disable);
        assert_eq!(p_claude.default_level, ThinkingLevel::Medium);
        assert!(p_claude.levels.contains(&ThinkingLevel::Max));

        let o3 = enrich_discovered_model(
            "openai",
            Model {
                id: "o3".to_string(),
                ..Default::default()
            },
            &catalog,
        );
        let p_o3 = model_thinking_profile(&o3);
        assert!(p_o3.supported);
        assert!(!p_o3.can_disable);
        assert_eq!(p_o3.default_level, ThinkingLevel::Medium);
        assert!(!p_o3.levels.contains(&ThinkingLevel::Off));

        let gpt41 = enrich_discovered_model(
            "openai",
            Model {
                id: "gpt-4.1".to_string(),
                ..Default::default()
            },
            &catalog,
        );
        assert!(!gpt41.reasoning);
        let p_gpt41 = model_thinking_profile(&gpt41);
        assert!(!p_gpt41.supported);
        assert_eq!(p_gpt41.levels, vec![ThinkingLevel::Off]);

        let gem_pro = enrich_discovered_model(
            "google",
            Model {
                id: "gemini-2.5-pro".to_string(),
                ..Default::default()
            },
            &catalog,
        );
        let p_gem = model_thinking_profile(&gem_pro);
        assert!(p_gem.supported);
        assert!(!p_gem.can_disable);
        assert_eq!(p_gem.default_level, ThinkingLevel::Medium);

        assert!(find_provider_spec("OpenRouter").is_some());
    }
}
