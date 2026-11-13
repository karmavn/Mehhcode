//! Azure OpenAI Responses API client, URL builder, and SSE event processing.

use futures::StreamExt;

use crate::openai_responses::{build_openai_responses_request, OpenAiResponsesStreamState};
use crate::sse::SseParser;
use crate::stream::{create_event_stream, AssistantEventStream, LlmProvider};
use crate::types::*;

/// Provider for Azure OpenAI Responses API.
pub struct AzureOpenAiResponsesProvider {
    client: reqwest::Client,
}

impl AzureOpenAiResponsesProvider {
    /// Creates a new Azure OpenAI Responses provider with default HTTP client.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new Azure OpenAI Responses provider with the given HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for AzureOpenAiResponsesProvider {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

/// Builds the Azure OpenAI Responses endpoint URL.
/// Format: `https://{resource}.openai.azure.com/openai/deployments/{deployment}/responses?api-version={version}`
pub fn build_azure_openai_url(
    model: &Model,
    resource_override: Option<&str>,
    api_version_override: Option<&str>,
) -> String {
    let api_version = api_version_override
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AZURE_OPENAI_API_VERSION").ok())
        .unwrap_or_else(|| "2025-03-01-preview".to_string());

    // If model.base_url is already a full responses endpoint URL
    if model.base_url.contains("/responses")
        && (model.base_url.starts_with("http://") || model.base_url.starts_with("https://"))
    {
        if model.base_url.contains("api-version=") {
            return model.base_url.clone();
        } else {
            let sep = if model.base_url.contains('?') { "&" } else { "?" };
            return format!("{}{sep}api-version={api_version}", model.base_url);
        }
    }

    let deployment = &model.id;

    // Check AZURE_OPENAI_ENDPOINT
    if let Ok(endpoint) = std::env::var("AZURE_OPENAI_ENDPOINT") {
        let trimmed = endpoint.trim_end_matches('/');
        return format!("{trimmed}/openai/deployments/{deployment}/responses?api-version={api_version}");
    }

    // Determine resource name
    let resource = resource_override
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AZURE_OPENAI_RESOURCE_NAME").ok())
        .or_else(|| {
            let stripped = model
                .base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            if let Some((subdomain, _)) = stripped.split_once(".openai.azure.com") {
                if !subdomain.is_empty() {
                    return Some(subdomain.to_string());
                }
            }
            None
        })
        .unwrap_or_else(|| "default".to_string());

    format!("https://{resource}.openai.azure.com/openai/deployments/{deployment}/responses?api-version={api_version}")
}

#[async_trait::async_trait]
impl LlmProvider for AzureOpenAiResponsesProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AssistantEventStream {
        let (sender, stream) = create_event_stream();
        let client = self.client.clone();
        let model = model.clone();
        let context = context.clone();
        let options = options.clone();

        tokio::spawn(async move {
            let body = build_openai_responses_request(&model, &context, &options);
            let url = build_azure_openai_url(&model, None, None);

            let mut req = client.post(&url);
            req = req.header("content-type", "application/json");

            // Azure OpenAI uses the `api-key` header rather than Authorization Bearer
            if let Some(api_key) = &options.api_key {
                req = req.header("api-key", api_key);
            } else if let Ok(env_key) = std::env::var("AZURE_OPENAI_API_KEY") {
                req = req.header("api-key", env_key);
            }

            if let Some(headers) = &model.headers {
                for (k, v) in headers {
                    req = req.header(k, v);
                }
            }
            if let Some(headers) = &options.headers {
                for (k, v) in headers {
                    req = req.header(k, v);
                }
            }
            req = req.json(&body);

            let resp = tokio::select! {
                _ = cancel.cancelled() => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Aborted;
                    err_msg.error_message = Some("Request was cancelled".to_string());
                    sender.push(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: err_msg,
                    });
                    return;
                }
                res = req.send() => {
                    match res {
                        Ok(r) => r,
                        Err(e) => {
                            let mut err_msg = AssistantMessage::empty_for_model(&model);
                            err_msg.stop_reason = StopReason::Error;
                            err_msg.error_message = Some(e.to_string());
                            sender.push(AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error: err_msg,
                            });
                            return;
                        }
                    }
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let mut err_msg = AssistantMessage::empty_for_model(&model);
                err_msg.stop_reason = StopReason::Error;
                err_msg.error_message = Some(format!("HTTP {status}: {text}"));
                sender.push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: err_msg,
                });
                return;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = SseParser::new();
            let mut state = OpenAiResponsesStreamState::new(&model);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let mut err_msg = state.finish(&sender);
                        err_msg.stop_reason = StopReason::Aborted;
                        err_msg.error_message = Some("Request was cancelled".to_string());
                        sender.push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: err_msg,
                        });
                        return;
                    }
                    chunk_opt = byte_stream.next() => {
                        match chunk_opt {
                            Some(Ok(bytes)) => {
                                let chunk_str = String::from_utf8_lossy(&bytes);
                                let events = sse_parser.feed(&chunk_str);
                                for ev in events {
                                    state.process_chunk(&ev.data, &sender);
                                }
                            }
                            Some(Err(e)) => {
                                let mut err_msg = state.finish(&sender);
                                err_msg.stop_reason = StopReason::Error;
                                err_msg.error_message = Some(e.to_string());
                                sender.push(AssistantMessageEvent::Error {
                                    reason: StopReason::Error,
                                    error: err_msg,
                                });
                                return;
                            }
                            None => break,
                        }
                    }
                }
            }

            let remaining = sse_parser.finish();
            for ev in remaining {
                state.process_chunk(&ev.data, &sender);
            }
            state.finish(&sender);
        });

        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_azure_openai_url() {
        let model = Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o (Azure)".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: "".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
        };

        // Explicit resource and api_version override
        let url1 = build_azure_openai_url(&model, Some("my-res"), Some("2025-03-01-preview"));
        assert_eq!(
            url1,
            "https://my-res.openai.azure.com/openai/deployments/gpt-4o/responses?api-version=2025-03-01-preview"
        );

        // Full URL in base_url
        let model_with_full_url = Model {
            base_url: "https://custom.openai.azure.com/openai/deployments/my-deploy/responses".to_string(),
            ..model.clone()
        };
        let url2 = build_azure_openai_url(&model_with_full_url, None, Some("2025-03-01-preview"));
        assert_eq!(
            url2,
            "https://custom.openai.azure.com/openai/deployments/my-deploy/responses?api-version=2025-03-01-preview"
        );
    }

    #[test]
    fn test_azure_openai_request_payload() {
        let model = Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o (Azure)".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: "https://my-res.openai.azure.com".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
        };

        let context = Context {
            system_prompt: Some("You are Azure assistant".to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("Ping".to_string()),
                timestamp: 100,
            })],
            tools: None,
        };

        let options = StreamOptions {
            api_key: Some("azure-secret-key".to_string()),
            temperature: Some(0.2),
            max_tokens: Some(1024),
            reasoning: None,
            session_id: None,
            headers: None,
        };

        let body = build_openai_responses_request(&model, &context, &options);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are Azure assistant");
        assert_eq!(body["temperature"], 0.2);
    }
}
