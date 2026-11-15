//! Context window overflow detection across LLM providers.

use std::sync::OnceLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

fn overflow_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            [
                r"(?i)prompt is too long",
                r"(?i)input is too long for requested model",
                r"(?i)exceeds the context window",
                r"(?i)input token count.*exceeds the maximum",
                r"(?i)maximum prompt length is \d+",
                r"(?i)reduce the length of the messages",
                r"(?i)maximum context length is \d+ tokens",
                r"(?i)exceeds the limit of \d+",
                r"(?i)exceeds the available context size",
                r"(?i)greater than the context length",
                r"(?i)context window exceeds limit",
                r"(?i)exceeded model token limit",
                r"(?i)too large for model with \d+ maximum context length",
                r"(?i)model_context_window_exceeded",
                r"(?i)context[_ ]length[_ ]exceeded",
                r"(?i)too many tokens",
                r"(?i)token limit exceeded",
                r"(?i)request_too_large",
                r"(?i)\b4(00|13)\b\s*(status code)?\s*\(no body\)",
            ]
            .into_iter()
            .map(|p| Regex::new(p).expect("overflow regex must compile"))
            .collect()
        })
        .as_slice()
}

/// Detects whether an `AssistantMessage` indicates the request exceeded the model's context window.
///
/// Covers explicit provider error payloads as well as silent token saturation (`StopReason::Stop` or
/// `StopReason::Length` when `usage` overflows `context_window`).
pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    if message.stop_reason == StopReason::Error {
        if let Some(err) = message.error_message.as_deref() {
            if overflow_patterns().iter().any(|re| re.is_match(err)) {
                return true;
            }
        }
    }

    if context_window > 0 && message.stop_reason == StopReason::Stop {
        let input_tokens = message.usage.input.saturating_add(message.usage.cache_read);
        if input_tokens > context_window {
            return true;
        }
    }

    if context_window > 0 && message.stop_reason == StopReason::Length {
        let total = message
            .usage
            .input
            .saturating_add(message.usage.cache_read)
            .saturating_add(message.usage.output);
        if total >= context_window {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, Usage};

    fn err_msg(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(text.to_string()),
            timestamp: now_ms(),
        }
    }

    #[test]
    fn test_provider_overflow_messages() {
        let cases = [
            "HTTP 400: prompt is too long: 205000 tokens > 200000 maximum",
            "HTTP 400: Your input exceeds the context window of this model",
            "HTTP 400: context_length_exceeded: maximum context length is 128000 tokens",
            "HTTP 400: Input token count of 1500000 exceeds the maximum number of tokens allowed",
            "HTTP 400: Input is too long for requested model",
            "HTTP 413: request_too_large",
            "HTTP 400: Prompt too many tokens for model",
            "HTTP 413: 413 status code (no body)",
            "HTTP 400: 400 (no body)",
            "HTTP 400: model_context_window_exceeded",
        ];

        for msg in cases {
            assert!(
                is_context_overflow(&err_msg(msg), 200_000),
                "Expected overflow match for: {msg}"
            );
        }
    }

    #[test]
    fn test_non_overflow_errors() {
        assert!(!is_context_overflow(
            &err_msg("HTTP 429: Rate limit exceeded"),
            200_000
        ));
        assert!(!is_context_overflow(
            &err_msg("HTTP 401: Invalid API key"),
            200_000
        ));
    }

    #[test]
    fn test_silent_usage_overflow() {
        let mut msg = AssistantMessage {
            content: Vec::new(),
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };
        msg.usage.input = 150_000;
        msg.usage.cache_read = 60_000;
        assert!(is_context_overflow(&msg, 200_000));

        msg.usage.input = 100_000;
        msg.usage.cache_read = 50_000;
        assert!(!is_context_overflow(&msg, 200_000));
    }
}
