use crate::types::{Model, ModelCost};

/// Builtin catalog of popular LLM models across 15 providers.
pub fn builtin_models() -> Vec<Model> {
    vec![
        // 1. Anthropic models (anthropic-messages)
        Model {
            id: "claude-sonnet-4-5".to_string(),
            name: "Claude Sonnet 4.5".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "claude-opus-4-5".to_string(),
            name: "Claude Opus 4.5".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
            context_window: 200_000,
            max_tokens: 32_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "claude-3-7-sonnet-latest".to_string(),
            name: "Claude 3.7 Sonnet Latest".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "claude-3-5-haiku-latest".to_string(),
            name: "Claude 3.5 Haiku Latest".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.8,
                output: 4.0,
                cache_read: 0.08,
                cache_write: 1.0,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 2. OpenAI models (openai-completions + gpt-5 / codex)
        Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.5,
                output: 10.0,
                cache_read: 1.25,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "gpt-4.1".to_string(),
            name: "GPT-4.1".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 1.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "gpt-4.1-mini".to_string(),
            name: "GPT-4.1 Mini".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.15,
                output: 0.6,
                cache_read: 0.075,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "o3".to_string(),
            name: "o3".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 20.0,
                output: 80.0,
                cache_read: 10.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 100_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "o4-mini".to_string(),
            name: "o4-mini".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 1.1,
                output: 4.4,
                cache_read: 0.55,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "gpt-5".to_string(),
            name: "GPT-5".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 10.0,
                output: 40.0,
                cache_read: 5.0,
                cache_write: 0.0,
            },
            context_window: 256_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "codex".to_string(),
            name: "Codex".to_string(),
            api: "openai-completions".to_string(),
            provider: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 1.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },

        // 3. OpenAI Responses models (openai-responses)
        Model {
            id: "gpt-4.1".to_string(),
            name: "GPT-4.1 (Responses)".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai-responses".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.0,
                output: 8.0,
                cache_read: 1.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "o3".to_string(),
            name: "o3 (Responses)".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai-responses".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 20.0,
                output: 80.0,
                cache_read: 10.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 100_000,
            headers: None,
            ..Default::default()
        },

        // 4. Google models (google-generative-ai)
        Model {
            id: "gemini-2.5-pro".to_string(),
            name: "Gemini 2.5 Pro".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 1.25,
                output: 5.0,
                cache_read: 0.3125,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "gemini-2.5-flash".to_string(),
            name: "Gemini 2.5 Flash".to_string(),
            api: "google-generative-ai".to_string(),
            provider: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.075,
                output: 0.3,
                cache_read: 0.01875,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },

        // 5. Google Vertex models (google-vertex)
        Model {
            id: "gemini-2.5-pro".to_string(),
            name: "Gemini 2.5 Pro (Vertex)".to_string(),
            api: "google-vertex".to_string(),
            provider: "google-vertex".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 1.25,
                output: 5.0,
                cache_read: 0.3125,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "gemini-2.5-flash".to_string(),
            name: "Gemini 2.5 Flash (Vertex)".to_string(),
            api: "google-vertex".to_string(),
            provider: "google-vertex".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.075,
                output: 0.3,
                cache_read: 0.01875,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },

        // 6. Amazon Bedrock models (bedrock-converse-stream)
        Model {
            id: "anthropic.claude-3-7-sonnet-20250219-v1:0".to_string(),
            name: "Claude 3.7 Sonnet (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "us.anthropic.claude-sonnet-4-20250514-v1:0".to_string(),
            name: "Claude Sonnet 4 (Bedrock)".to_string(),
            api: "bedrock-converse-stream".to_string(),
            provider: "amazon-bedrock".to_string(),
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        },

        // 7. Mistral models (mistral-conversations)
        Model {
            id: "mistral-large-latest".to_string(),
            name: "Mistral Large Latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.0,
                output: 6.0,
                cache_read: 0.2,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "codestral-latest".to_string(),
            name: "Codestral Latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.3,
                output: 0.9,
                cache_read: 0.03,
                cache_write: 0.0,
            },
            context_window: 256_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "magistral-medium-latest".to_string(),
            name: "Magistral Medium Latest".to_string(),
            api: "mistral-conversations".to_string(),
            provider: "mistral".to_string(),
            base_url: "https://api.mistral.ai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 1.5,
                output: 4.5,
                cache_read: 0.15,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },

        // 8. Azure OpenAI Responses models (azure-openai-responses)
        Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o (Azure Responses)".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.5,
                output: 10.0,
                cache_read: 1.25,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "o3".to_string(),
            name: "o3 (Azure Responses)".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: "".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 20.0,
                output: 80.0,
                cache_read: 10.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 100_000,
            headers: None,
            ..Default::default()
        },

        // 9. OpenRouter models (openai-completions)
        Model {
            id: "anthropic/claude-sonnet-4.5".to_string(),
            name: "Claude Sonnet 4.5 (OpenRouter)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "openai/gpt-4o".to_string(),
            name: "GPT-4o (OpenRouter)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 2.5,
                output: 10.0,
                cache_read: 1.25,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "deepseek/deepseek-r1".to_string(),
            name: "DeepSeek R1 (OpenRouter)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.55,
                output: 2.19,
                cache_read: 0.14,
                cache_write: 0.0,
            },
            context_window: 64_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 9b. OpenRouter free models (openai-completions, cost = 0)
        Model {
            id: "nvidia/nemotron-3-ultra-550b-a55b:free".to_string(),
            name: "Nemotron 3 Ultra 550B (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "nvidia/nemotron-3.5-lightning:free".to_string(),
            name: "Nemotron 3.5 Lightning (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "nvidia/nemotron-3.5-super-vl:free".to_string(),
            name: "Nemotron 3.5 Super VL (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 1_000_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free".to_string(),
            name: "Nemotron 3 Nano Omni 30B Reasoning (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 256_000,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "google/gemma-4-31b-it:free".to_string(),
            name: "Gemma 4 31B IT (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 262_144,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "inclusionai/ling-3.2-flash:free".to_string(),
            name: "Ling 3.2 Flash (OpenRouter, free)".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 1_048_576,
            max_tokens: 65_536,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "openrouter/free".to_string(),
            name: "OpenRouter Free Router".to_string(),
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },

        // 10. Groq models (openai-completions)
        Model {
            id: "llama-3.3-70b-versatile".to_string(),
            name: "Llama 3.3 70B Versatile (Groq)".to_string(),
            api: "openai-completions".to_string(),
            provider: "groq".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.59,
                output: 0.79,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "deepseek-r1-distill-llama-70b".to_string(),
            name: "DeepSeek R1 Distill Llama 70B (Groq)".to_string(),
            api: "openai-completions".to_string(),
            provider: "groq".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.59,
                output: 0.79,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 11. DeepSeek models (openai-completions)
        Model {
            id: "deepseek-chat".to_string(),
            name: "DeepSeek Chat".to_string(),
            api: "openai-completions".to_string(),
            provider: "deepseek".to_string(),
            base_url: "https://api.deepseek.com/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.14,
                output: 0.28,
                cache_read: 0.014,
                cache_write: 0.0,
            },
            context_window: 64_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "deepseek-reasoner".to_string(),
            name: "DeepSeek Reasoner".to_string(),
            api: "openai-completions".to_string(),
            provider: "deepseek".to_string(),
            base_url: "https://api.deepseek.com/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.55,
                output: 2.19,
                cache_read: 0.14,
                cache_write: 0.0,
            },
            context_window: 64_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 12. xAI models (openai-completions)
        Model {
            id: "grok-3".to_string(),
            name: "Grok 3".to_string(),
            api: "openai-completions".to_string(),
            provider: "xai".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.75,
                cache_write: 0.0,
            },
            context_window: 131_072,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "grok-3-mini".to_string(),
            name: "Grok 3 Mini".to_string(),
            api: "openai-completions".to_string(),
            provider: "xai".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost {
                input: 0.3,
                output: 1.5,
                cache_read: 0.075,
                cache_write: 0.0,
            },
            context_window: 131_072,
            max_tokens: 32_768,
            headers: None,
            ..Default::default()
        },

        // 13. Cerebras models (openai-completions)
        Model {
            id: "llama-3.3-70b".to_string(),
            name: "Llama 3.3 70B (Cerebras)".to_string(),
            api: "openai-completions".to_string(),
            provider: "cerebras".to_string(),
            base_url: "https://api.cerebras.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.6,
                output: 0.6,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "qwen-3-32b".to_string(),
            name: "Qwen 3 32B (Cerebras)".to_string(),
            api: "openai-completions".to_string(),
            provider: "cerebras".to_string(),
            base_url: "https://api.cerebras.ai/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.4,
                output: 0.4,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 14. Together models (openai-completions)
        Model {
            id: "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
            name: "Llama 3.3 70B Instruct Turbo (Together)".to_string(),
            api: "openai-completions".to_string(),
            provider: "together".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 0.88,
                output: 0.88,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 131_072,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "deepseek-ai/DeepSeek-R1".to_string(),
            name: "DeepSeek R1 (Together)".to_string(),
            api: "openai-completions".to_string(),
            provider: "together".to_string(),
            base_url: "https://api.together.xyz/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 7.0,
                output: 7.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 131_072,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 15. Fireworks models (openai-completions)
        Model {
            id: "accounts/fireworks/models/deepseek-r1".to_string(),
            name: "DeepSeek R1 (Fireworks)".to_string(),
            api: "openai-completions".to_string(),
            provider: "fireworks".to_string(),
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            reasoning: true,
            input: vec!["text".to_string()],
            cost: ModelCost {
                input: 7.0,
                output: 7.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 131_072,
            max_tokens: 8192,
            headers: None,
            ..Default::default()
        },

        // 16. GitHub Copilot models (openai-completions)
        Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o (GitHub Copilot)".to_string(),
            api: "openai-completions".to_string(),
            provider: "github-copilot".to_string(),
            base_url: "https://api.githubcopilot.com".to_string(),
            reasoning: false,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        },
        Model {
            id: "claude-sonnet-4.5".to_string(),
            name: "Claude Sonnet 4.5 (GitHub Copilot)".to_string(),
            api: "openai-completions".to_string(),
            provider: "github-copilot".to_string(),
            base_url: "https://api.githubcopilot.com".to_string(),
            reasoning: true,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        },
    ]
}

/// Returns the primary environment variable name storing the API key for a provider.
pub fn env_api_key_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" | "openai-responses" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        "google-vertex" => "GOOGLE_CLOUD_API_KEY",
        "amazon-bedrock" => "AWS_ACCESS_KEY_ID",
        "mistral" => "MISTRAL_API_KEY",
        "azure" | "azure-openai-responses" => "AZURE_OPENAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "groq" => "GROQ_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "xai" => "XAI_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "together" => "TOGETHER_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        "github-copilot" => "COPILOT_GITHUB_TOKEN",
        _ => "API_KEY",
    }
}

/// Returns the default base URL for a provider if standard/applicable.
pub fn default_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("https://api.anthropic.com/v1"),
        "openai" | "openai-responses" => Some("https://api.openai.com/v1"),
        "google" => Some("https://generativelanguage.googleapis.com/v1beta"),
        "google-vertex" => Some("https://us-central1-aiplatform.googleapis.com/v1"),
        "amazon-bedrock" => Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "deepseek" => Some("https://api.deepseek.com/v1"),
        "xai" => Some("https://api.x.ai/v1"),
        "cerebras" => Some("https://api.cerebras.ai/v1"),
        "together" => Some("https://api.together.xyz/v1"),
        "fireworks" => Some("https://api.fireworks.ai/inference/v1"),
        "github-copilot" => Some("https://api.githubcopilot.com"),
        _ => None,
    }
}

/// Resolves a model given either (provider, Some(model_id)) or a single identifier string
/// (e.g., "anthropic/claude-sonnet-4-5", "gpt-4o", or "google").
pub fn resolve_model(provider_or_id: &str, model_id: Option<&str>) -> Option<Model> {
    let models = builtin_models();

    if let Some(id) = model_id {
        return models.into_iter().find(|m| {
            m.provider.eq_ignore_ascii_case(provider_or_id) && m.id.eq_ignore_ascii_case(id)
        });
    }

    // 1. Check slash notation: "provider/model_id"
    if let Some((prov, id)) = provider_or_id.split_once('/') {
        if let Some(found) = models
            .iter()
            .find(|m| m.provider.eq_ignore_ascii_case(prov) && m.id.eq_ignore_ascii_case(id))
        {
            return Some(found.clone());
        }
    }

    // 2. Check exact model id match
    if let Some(found) = models
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(provider_or_id))
    {
        return Some(found.clone());
    }

    // 3. Check provider default fallback
    models
        .into_iter()
        .find(|m| m.provider.eq_ignore_ascii_case(provider_or_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_models_contains_keys() {
        let models = builtin_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();

        assert!(ids.contains(&"claude-sonnet-4-5"));
        assert!(ids.contains(&"claude-opus-4-5"));
        assert!(ids.contains(&"claude-3-7-sonnet-latest"));
        assert!(ids.contains(&"claude-3-5-haiku-latest"));
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"gpt-4.1"));
        assert!(ids.contains(&"gpt-4.1-mini"));
        assert!(ids.contains(&"o3"));
        assert!(ids.contains(&"o4-mini"));
        assert!(ids.contains(&"gpt-5"));
        assert!(ids.contains(&"codex"));
        assert!(ids.contains(&"gemini-2.5-pro"));
        assert!(ids.contains(&"gemini-2.5-flash"));
        assert!(ids.contains(&"anthropic.claude-3-7-sonnet-20250219-v1:0"));
        assert!(ids.contains(&"us.anthropic.claude-sonnet-4-20250514-v1:0"));
        assert!(ids.contains(&"mistral-large-latest"));
        assert!(ids.contains(&"codestral-latest"));
        assert!(ids.contains(&"magistral-medium-latest"));
        assert!(ids.contains(&"llama-3.3-70b-versatile"));
        assert!(ids.contains(&"deepseek-chat"));
        assert!(ids.contains(&"deepseek-reasoner"));
        assert!(ids.contains(&"grok-3"));
    }

    #[test]
    fn test_resolve_model() {
        let m1 = resolve_model("anthropic", Some("claude-sonnet-4-5")).unwrap();
        assert_eq!(m1.id, "claude-sonnet-4-5");
        assert_eq!(m1.provider, "anthropic");

        let m2 = resolve_model("openai/gpt-4o", None).unwrap();
        assert_eq!(m2.id, "gpt-4o");
        assert_eq!(m2.provider, "openai");

        let m3 = resolve_model("gemini-2.5-pro", None).unwrap();
        assert_eq!(m3.id, "gemini-2.5-pro");
        assert_eq!(m3.provider, "google");

        let m4 = resolve_model("anthropic", None).unwrap();
        assert_eq!(m4.provider, "anthropic");

        let m_mistral = resolve_model("mistral", Some("codestral-latest")).unwrap();
        assert_eq!(m_mistral.id, "codestral-latest");
        assert_eq!(m_mistral.provider, "mistral");

        let m_bedrock = resolve_model("amazon-bedrock", Some("anthropic.claude-3-7-sonnet-20250219-v1:0")).unwrap();
        assert_eq!(m_bedrock.id, "anthropic.claude-3-7-sonnet-20250219-v1:0");

        let m_openrouter = resolve_model("openrouter", Some("anthropic/claude-sonnet-4.5")).unwrap();
        assert_eq!(m_openrouter.id, "anthropic/claude-sonnet-4.5");

        let m5 = resolve_model("unknown-provider", Some("unknown-model"));
        assert!(m5.is_none());
    }

    #[test]
    fn test_env_api_key_name() {
        assert_eq!(env_api_key_name("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(env_api_key_name("openai"), "OPENAI_API_KEY");
        assert_eq!(env_api_key_name("google"), "GEMINI_API_KEY");
        assert_eq!(env_api_key_name("google-vertex"), "GOOGLE_CLOUD_API_KEY");
        assert_eq!(env_api_key_name("amazon-bedrock"), "AWS_ACCESS_KEY_ID");
        assert_eq!(env_api_key_name("mistral"), "MISTRAL_API_KEY");
        assert_eq!(env_api_key_name("azure-openai-responses"), "AZURE_OPENAI_API_KEY");
        assert_eq!(env_api_key_name("groq"), "GROQ_API_KEY");
        assert_eq!(env_api_key_name("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(env_api_key_name("github-copilot"), "COPILOT_GITHUB_TOKEN");
    }

    #[test]
    fn test_default_base_url() {
        assert_eq!(default_base_url("anthropic"), Some("https://api.anthropic.com/v1"));
        assert_eq!(default_base_url("openai"), Some("https://api.openai.com/v1"));
        assert_eq!(default_base_url("mistral"), Some("https://api.mistral.ai/v1"));
        assert_eq!(default_base_url("groq"), Some("https://api.groq.com/openai/v1"));
        assert_eq!(default_base_url("deepseek"), Some("https://api.deepseek.com/v1"));
        assert_eq!(default_base_url("unknown"), None);
    }

    #[test]
    fn test_openrouter_free_models_are_cataloged_and_cost_zero() {
        let free_ids = [
            "nvidia/nemotron-3-ultra-550b-a55b:free",
            "nvidia/nemotron-3.5-lightning:free",
            "nvidia/nemotron-3.5-super-vl:free",
            "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
            "google/gemma-4-31b-it:free",
            "inclusionai/ling-3.2-flash:free",
            "openrouter/free",
        ];
        for id in free_ids {
            let m = resolve_model("openrouter", Some(id))
                .unwrap_or_else(|| panic!("missing free OpenRouter model: {id}"));
            assert_eq!(m.provider, "openrouter");
            assert_eq!(m.api, "openai-completions");
            assert_eq!(m.cost.input, 0.0, "{id} must be free on input");
            assert_eq!(m.cost.output, 0.0, "{id} must be free on output");
        }
    }
}
