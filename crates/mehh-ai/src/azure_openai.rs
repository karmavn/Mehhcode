//! Azure OpenAI Responses API client, URL builder, and SSE event processing.

use std::collections::HashMap;
use futures::StreamExt;

use crate::oauth::redact_error_body_with_limit;
use crate::openai_responses::{build_openai_responses_request, OpenAiResponsesStreamState};
use crate::retry::{
    default_http_client, retry_failure_event, send_with_retry_with_secrets, RetryPolicy,
    DEFAULT_STREAM_CHUNK_TIMEOUT,
};
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
            client: default_http_client(),
        }
    }
}

fn non_empty_trimmed(val: Option<&str>) -> Option<String> {
    val.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn insert_custom_headers(
    header_map: &mut reqwest::header::HeaderMap,
    headers: Option<&HashMap<String, String>>,
) -> Result<(), String> {
    if let Some(map) = headers {
        for (k, v) in map {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("Invalid header name '{k}': {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| format!("Invalid header value for '{k}': {e}"))?;
            header_map.insert(name, value);
        }
    }
    Ok(())
}

fn append_api_version(url: &mut reqwest::Url, encoded_api_version: &str) {
    if !url.query_pairs().any(|(k, _)| k == "api-version") {
        let new_query = match url.query() {
            Some(existing) if !existing.is_empty() => {
                format!("{existing}&api-version={encoded_api_version}")
            }
            _ => format!("api-version={encoded_api_version}"),
        };
        url.set_query(Some(&new_query));
    }
}

fn build_resource_responses_url(
    resource: &str,
    encoded_deployment: &str,
    encoded_api_version: &str,
) -> anyhow::Result<String> {
    if resource.contains(['/', '\\', '?', '#', '@', ':'])
        || resource.chars().any(char::is_whitespace)
    {
        anyhow::bail!("Invalid Azure OpenAI resource name '{resource}'");
    }
    let raw = format!(
        "https://{resource}.openai.azure.com/openai/deployments/{encoded_deployment}/responses?api-version={encoded_api_version}"
    );
    let url = reqwest::Url::parse(&raw)
        .map_err(|e| anyhow::anyhow!("Invalid Azure OpenAI resource URL: {e}"))?;
    Ok(url.to_string())
}

/// Builds the Azure OpenAI Responses endpoint URL.
/// Format: `https://{resource}.openai.azure.com/openai/deployments/{deployment}/responses?api-version={version}`
pub fn build_azure_openai_url(
    model: &Model,
    resource_override: Option<&str>,
    api_version_override: Option<&str>,
) -> anyhow::Result<String> {
    let api_version = non_empty_trimmed(api_version_override)
        .or_else(|| non_empty_env("AZURE_OPENAI_API_VERSION"))
        .unwrap_or_else(|| "2025-03-01-preview".to_string());
    let encoded_api_version = crate::oauth::percent_encode(&api_version);

    let trimmed_base = model.base_url.trim();
    let parsed_base = if !trimmed_base.is_empty() {
        let u = reqwest::Url::parse(trimmed_base)
            .map_err(|e| anyhow::anyhow!("Invalid Azure OpenAI base_url '{trimmed_base}': {e}"))?;
        if u.scheme() != "http" && u.scheme() != "https" {
            anyhow::bail!("Unsupported scheme '{}' in Azure OpenAI base_url", u.scheme());
        }
        Some(u)
    } else {
        None
    };

    // If model.base_url already terminates at the `/responses` segment, preserve it.
    if let Some(mut url) = parsed_base.clone() {
        let clean_path = url.path().trim_end_matches('/');
        if clean_path.rsplit('/').next() == Some("responses") {
            let clean_path_owned = clean_path.to_string();
            if url.path() != clean_path_owned {
                url.set_path(&clean_path_owned);
            }
            append_api_version(&mut url, &encoded_api_version);
            return Ok(url.to_string());
        }
    }

    let deployment = model.id.trim();
    if deployment.is_empty() {
        anyhow::bail!("Azure OpenAI deployment (model.id) is empty");
    }
    let encoded_deployment = crate::oauth::percent_encode(deployment);

    // Per-model configuration wins over global environment variables.
    let explicit_resource = non_empty_trimmed(resource_override).or_else(|| {
        parsed_base.as_ref().and_then(|u| {
            u.host_str()
                .and_then(|h| h.strip_suffix(".openai.azure.com"))
                .filter(|sub| !sub.is_empty())
                .map(str::to_string)
        })
    });

    if let Some(resource) = explicit_resource {
        return build_resource_responses_url(&resource, &encoded_deployment, &encoded_api_version);
    }

    // Any deliberately configured base URL (proxy, gateway, etc.) is
    // authoritative over ambient env vars; the generic OpenAI fallback is not a real configuration.
    if let Some(mut url) = parsed_base {
        if url.host_str() != Some("api.openai.com") {
            let base_path = url.path().trim_end_matches('/').to_string();
            url.set_path(&format!(
                "{base_path}/openai/deployments/{encoded_deployment}/responses"
            ));
            append_api_version(&mut url, &encoded_api_version);
            return Ok(url.to_string());
        }
    }

    if let Some(resource) = non_empty_env("AZURE_OPENAI_RESOURCE_NAME") {
        return build_resource_responses_url(&resource, &encoded_deployment, &encoded_api_version);
    }

    // Global endpoint is only a fallback when the model has no configuration.
    if let Some(endpoint) = non_empty_env("AZURE_OPENAI_ENDPOINT") {
        let mut url = reqwest::Url::parse(&endpoint)
            .map_err(|e| anyhow::anyhow!("Invalid AZURE_OPENAI_ENDPOINT '{endpoint}': {e}"))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            anyhow::bail!(
                "Unsupported scheme '{}' in AZURE_OPENAI_ENDPOINT",
                url.scheme()
            );
        }
        let base_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!(
            "{base_path}/openai/deployments/{encoded_deployment}/responses"
        ));
        append_api_version(&mut url, &encoded_api_version);
        return Ok(url.to_string());
    }

    anyhow::bail!(
        "Azure OpenAI endpoint is not configured: set model.base_url, AZURE_OPENAI_RESOURCE_NAME, or AZURE_OPENAI_ENDPOINT"
    )
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
        let options = options.clone();
        let body_res = serde_json::to_vec(&build_openai_responses_request(&model, context, &options))
            .map_err(|e| format!("Failed to serialize request body: {e}"));

        tokio::spawn(async move {
            let body_bytes = match body_res {
                Ok(b) => b,
                Err(msg) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(msg);
                    let _ = sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };
            let url = match build_azure_openai_url(&model, None, None) {
                Ok(u) => u,
                Err(e) => {
                    let mut err_msg = AssistantMessage::empty_for_model(&model);
                    err_msg.stop_reason = StopReason::Error;
                    err_msg.error_message = Some(e.to_string());
                    let _ = sender
                        .send(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: err_msg,
                        })
                        .await;
                    return;
                }
            };

            let api_key = non_empty_trimmed(options.api_key.as_deref())
                .or_else(|| non_empty_env("AZURE_OPENAI_API_KEY"));
            let secrets_owned: Vec<String> = api_key.clone().into_iter().collect();
            let secret_refs: Vec<&str> = secrets_owned.iter().map(String::as_str).collect();

            let mut header_map = reqwest::header::HeaderMap::new();
            header_map.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            );
            // Azure OpenAI uses the `api-key` header rather than Authorization Bearer
            if let Some(ref key) = api_key {
                match reqwest::header::HeaderValue::from_str(key) {
                    Ok(val) => {
                        header_map.insert(reqwest::header::HeaderName::from_static("api-key"), val);
                    }
                    Err(e) => {
                        let mut err_msg = AssistantMessage::empty_for_model(&model);
                        err_msg.stop_reason = StopReason::Error;
                        err_msg.error_message = Some(redact_error_body_with_limit(
                            &format!("Invalid api-key header: {e}"),
                            &secret_refs,
                            4096,
                        ));
                        let _ = sender
                            .send(AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error: err_msg,
                            })
                            .await;
                        return;
                    }
                }
            }
            if let Err(e) = insert_custom_headers(&mut header_map, model.headers.as_ref())
                .and_then(|_| insert_custom_headers(&mut header_map, options.headers.as_ref()))
            {
                let mut err_msg = AssistantMessage::empty_for_model(&model);
                err_msg.stop_reason = StopReason::Error;
                err_msg.error_message =
                    Some(redact_error_body_with_limit(&e, &secret_refs, 4096));
                let _ = sender
                    .send(AssistantMessageEvent::Error {
                        reason: StopReason::Error,
                        error: err_msg,
                    })
                    .await;
                return;
            }

            let resp = match send_with_retry_with_secrets(
                || {
                    client
                        .post(&url)
                        .headers(header_map.clone())
                        .body(body_bytes.clone())
                },
                &RetryPolicy::default(),
                &cancel,
                &secret_refs,
            )
            .await
            {
                Ok(r) => r,
                Err(failure) => {
                    sender.send(retry_failure_event(failure, &model)).await;
                    return;
                }
            };

            let mut byte_stream = resp.bytes_stream();
            let mut sse_parser = SseParser::new();
            let mut state = OpenAiResponsesStreamState::new(&model);
            state.set_secrets(secrets_owned.clone());

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        state.mark_aborted("Request was cancelled");
                        let mut batch = Vec::new();
                        state.finish_into(&mut batch);
                        sender.send_batch(batch).await;
                        return;
                    }
                    chunk_opt = tokio::time::timeout(DEFAULT_STREAM_CHUNK_TIMEOUT, byte_stream.next()) => {
                        match chunk_opt {
                            Ok(Some(Ok(bytes))) => {
                                let events = sse_parser.feed_bytes(&bytes);
                                if sse_parser.has_overflowed() {
                                    state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
                                    let mut batch = Vec::new();
                                    state.finish_into(&mut batch);
                                    sender.send_batch(batch).await;
                                    return;
                                }
                                let mut batch = Vec::new();
                                for ev in events {
                                    state.process_chunk_into(&ev.data, &mut batch);
                                }
                                if !sender.send_batch(batch).await {
                                    return;
                                }
                                if state.done {
                                    return;
                                }
                            }
                            Ok(Some(Err(e))) => {
                                state.mark_error(redact_error_body_with_limit(
                                    &e.without_url().to_string(),
                                    &secret_refs,
                                    4096,
                                ));
                                let mut batch = Vec::new();
                                state.finish_into(&mut batch);
                                sender.send_batch(batch).await;
                                return;
                            }
                            Err(_) => {
                                state.mark_error(format!(
                                    "Stream read timed out after {} seconds",
                                    DEFAULT_STREAM_CHUNK_TIMEOUT.as_secs()
                                ));
                                let mut batch = Vec::new();
                                state.finish_into(&mut batch);
                                sender.send_batch(batch).await;
                                return;
                            }
                            Ok(None) => break,
                        }
                    }
                }
            }

            let remaining = sse_parser.finish();
            let mut batch = Vec::new();
            for ev in remaining {
                state.process_chunk_into(&ev.data, &mut batch);
            }
            if sse_parser.has_overflowed() {
                state.mark_error("SSE event exceeded maximum buffer size (16 MiB)");
            } else if sse_parser.has_pending_bytes() || !state.received_terminal {
                state.mark_error("Stream ended prematurely before Azure OpenAI Responses completion");
            }
            state.finish_into(&mut batch);
            sender.send_batch(batch).await;
        });

        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static AZURE_ENV_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(key: &'static str, val: Option<&str>) -> Self {
            let prev = std::env::var_os(key);
            unsafe {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self(key, prev)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
    }

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
            ..Default::default()
        };

        // Explicit resource and api_version override
        let url1 = build_azure_openai_url(&model, Some("my-res"), Some("2025-03-01-preview"))
            .expect("explicit resource URL");
        assert_eq!(
            url1,
            "https://my-res.openai.azure.com/openai/deployments/gpt-4o/responses?api-version=2025-03-01-preview"
        );

        // Full URL in base_url
        let model_with_full_url = Model {
            base_url: "https://custom.openai.azure.com/openai/deployments/my-deploy/responses"
                .to_string(),
            ..model.clone()
        };
        let url2 = build_azure_openai_url(&model_with_full_url, None, Some("2025-03-01-preview"))
            .expect("full responses URL");
        assert_eq!(
            url2,
            "https://custom.openai.azure.com/openai/deployments/my-deploy/responses?api-version=2025-03-01-preview"
        );
    }

    #[test]
    fn build_azure_openai_url_fails_closed_when_unconfigured() {
        let _lock = AZURE_ENV_MUTEX.blocking_lock();
        let model = Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o (Azure)".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: String::new(),
            ..Default::default()
        };

        // Only assert fail-closed when ambient AZURE_OPENAI_* env vars are absent.
        let has_env = std::env::var("AZURE_OPENAI_RESOURCE_NAME")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
            || std::env::var("AZURE_OPENAI_ENDPOINT")
                .ok()
                .is_some_and(|v| !v.trim().is_empty());
        if !has_env {
            let err = build_azure_openai_url(&model, Some(""), Some(""))
                .expect_err("unconfigured Azure endpoint must fail closed");
            assert!(err.to_string().contains("not configured"));
        }
    }

    #[test]
    fn build_azure_openai_url_percent_encodes_and_checks_terminal_responses() {
        // `/responses-proxy/v1` must NOT be treated as a terminal `/responses` endpoint.
        let proxy_model = Model {
            id: "team/deploy 1".to_string(),
            base_url: "https://proxy.example.com/responses-proxy/v1".to_string(),
            ..Default::default()
        };
        let url = build_azure_openai_url(&proxy_model, None, Some("2025-03-01&extra=1"))
            .expect("proxy URL");
        assert_eq!(
            url,
            "https://proxy.example.com/responses-proxy/v1/openai/deployments/team%2Fdeploy%201/responses?api-version=2025-03-01%26extra%3D1"
        );

        // Host confusion attempt (`evil.com/foo.openai.azure.com`) must NOT extract `evil.com/foo` as resource.
        let spoof_model = Model {
            id: "gpt-4o".to_string(),
            base_url: "https://evil.com/foo.openai.azure.com/base".to_string(),
            ..Default::default()
        };
        let spoof_url = build_azure_openai_url(&spoof_model, None, Some("2025-03-01-preview"))
            .expect("proxy base url");
        assert_eq!(
            spoof_url,
            "https://evil.com/foo.openai.azure.com/base/openai/deployments/gpt-4o/responses?api-version=2025-03-01-preview"
        );
    }

    #[tokio::test]
    async fn azure_provider_stream_emits_error_when_unconfigured() {
        let _lock = AZURE_ENV_MUTEX.lock().await;
        let has_env = std::env::var("AZURE_OPENAI_RESOURCE_NAME")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
            || std::env::var("AZURE_OPENAI_ENDPOINT")
                .ok()
                .is_some_and(|v| !v.trim().is_empty());
        if has_env {
            return;
        }
        let model = Model {
            id: "gpt-4o".to_string(),
            api: "azure-openai-responses".to_string(),
            provider: "azure-openai-responses".to_string(),
            base_url: String::new(),
            ..Default::default()
        };
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 1,
            })],
            tools: None,
        };
        let provider = AzureOpenAiResponsesProvider::new();
        let mut stream = provider.stream(
            &model,
            &context,
            &StreamOptions::default(),
            tokio_util::sync::CancellationToken::new(),
        );
        let msg = stream.collect_final_message().await.unwrap();
        assert_eq!(msg.stop_reason, StopReason::Error);
        assert!(msg
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("not configured"));
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
            ..Default::default()
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
            ..Default::default()
        };

        let body = build_openai_responses_request(&model, &context, &options);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "You are Azure assistant");
        assert_eq!(body["temperature"], 0.2);
    }

    #[test]
    fn explicit_custom_proxy_base_url_wins_over_ambient_resource_name() {
        let _lock = AZURE_ENV_MUTEX.blocking_lock();
        let _res_guard = EnvGuard::set("AZURE_OPENAI_RESOURCE_NAME", Some("ambient-res"));
        let _ep_guard = EnvGuard::set("AZURE_OPENAI_ENDPOINT", None);

        let proxy_model = Model {
            id: "gpt-4o".to_string(),
            base_url: "https://proxy.example.com/gateway".to_string(),
            ..Default::default()
        };
        let url = build_azure_openai_url(&proxy_model, None, Some("2025-03-01-preview"))
            .expect("custom proxy base_url should succeed");
        assert_eq!(
            url,
            "https://proxy.example.com/gateway/openai/deployments/gpt-4o/responses?api-version=2025-03-01-preview"
        );

        // Generic OpenAI default base_url is not an Azure proxy config, so env resource still applies.
        let generic_model = Model {
            id: "gpt-4o".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            ..Default::default()
        };
        let env_url = build_azure_openai_url(&generic_model, None, Some("2025-03-01-preview"))
            .expect("ambient resource used when base_url is generic OpenAI");
        assert_eq!(
            env_url,
            "https://ambient-res.openai.azure.com/openai/deployments/gpt-4o/responses?api-version=2025-03-01-preview"
        );
    }
}
