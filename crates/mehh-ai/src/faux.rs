use std::sync::{Arc, Mutex};
use crate::stream::{create_event_stream, AssistantEventStream, LlmProvider};
use crate::types::{
    now_ms, AssistantContentBlock, AssistantMessage, AssistantMessageEvent, Context, Model,
    ModelCost, StopReason, StreamOptions, TextContent, ToolCall, Usage,
};

pub type FauxHandler =
    Box<dyn Fn(&Model, &Context, &StreamOptions) -> AssistantMessage + Send + Sync>;

#[allow(clippy::large_enum_variant)]
pub enum FauxResponse {
    Message(AssistantMessage),
    Handler(FauxHandler),
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
}

#[derive(Default)]
struct FauxState {
    responses: Vec<FauxResponse>,
    requests: Vec<RecordedRequest>,
}

#[derive(Clone, Default)]
pub struct FauxProvider {
    inner: Arc<Mutex<FauxState>>,
}

impl FauxProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_responses(responses: Vec<AssistantMessage>) -> Self {
        let provider = Self::new();
        for resp in responses {
            provider.add_response(resp);
        }
        provider
    }

    pub fn add_response(&self, message: AssistantMessage) {
        let mut state = self.inner.lock().unwrap();
        state.responses.push(FauxResponse::Message(message));
    }

    pub fn add_handler(
        &self,
        handler: impl Fn(&Model, &Context, &StreamOptions) -> AssistantMessage + Send + Sync + 'static,
    ) {
        let mut state = self.inner.lock().unwrap();
        state.responses.push(FauxResponse::Handler(Box::new(handler)));
    }

    pub fn recorded_requests(&self) -> Vec<RecordedRequest> {
        let state = self.inner.lock().unwrap();
        state.requests.clone()
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.responses.clear();
        state.requests.clear();
    }
}

#[async_trait::async_trait]
impl LlmProvider for FauxProvider {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> AssistantEventStream {
        let (sender, stream) = create_event_stream();

        let message = {
            let mut state = self.inner.lock().unwrap();
            state.requests.push(RecordedRequest {
                model: model.clone(),
                context: context.clone(),
                options: options.clone(),
            });
            if !state.responses.is_empty() {
                match state.responses.remove(0) {
                    FauxResponse::Message(msg) => msg,
                    FauxResponse::Handler(h) => h(model, context, options),
                }
            } else {
                faux_assistant_message("faux default response")
            }
        };

        let mut partial = AssistantMessage {
            content: Vec::new(),
            api: message.api.clone(),
            provider: message.provider.clone(),
            model: message.model.clone(),
            response_id: message.response_id.clone(),
            usage: message.usage.clone(),
            stop_reason: message.stop_reason,
            error_message: message.error_message.clone(),
            timestamp: message.timestamp,
        };

        sender.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        });

        for (index, block) in message.content.iter().enumerate() {
            match block {
                AssistantContentBlock::Text(t) => {
                    sender.push(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    partial
                        .content
                        .push(AssistantContentBlock::Text(t.clone()));
                    sender.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: t.text.clone(),
                        partial: partial.clone(),
                    });
                    sender.push(AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: t.text.clone(),
                        partial: partial.clone(),
                    });
                }
                AssistantContentBlock::Thinking(th) => {
                    sender.push(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    partial
                        .content
                        .push(AssistantContentBlock::Thinking(th.clone()));
                    sender.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: th.thinking.clone(),
                        partial: partial.clone(),
                    });
                    sender.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: th.thinking.clone(),
                        partial: partial.clone(),
                    });
                }
                AssistantContentBlock::ToolCall(tc) => {
                    sender.push(AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    partial
                        .content
                        .push(AssistantContentBlock::ToolCall(tc.clone()));
                    let delta = serde_json::to_string(&tc.arguments).unwrap_or_default();
                    sender.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta,
                        partial: partial.clone(),
                    });
                    sender.push(AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call: tc.clone(),
                        partial: partial.clone(),
                    });
                }
            }
        }

        if message.stop_reason == StopReason::Error || message.stop_reason == StopReason::Aborted {
            sender.push(AssistantMessageEvent::Error {
                reason: message.stop_reason,
                error: message,
            });
        } else {
            sender.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message,
            });
        }

        stream
    }
}

pub fn faux_assistant_message(text: impl Into<String>) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_id: Some("faux-resp".to_string()),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

pub fn faux_tool_call(
    id: impl Into<String>,
    name: impl Into<String>,
    args: serde_json::Value,
) -> ToolCall {
    let arguments = match args {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        arguments_error: None,
        thought_signature: None,
    }
}

pub fn faux_tool_call_message(calls: Vec<ToolCall>) -> AssistantMessage {
    AssistantMessage {
        content: calls
            .into_iter()
            .map(AssistantContentBlock::ToolCall)
            .collect(),
        api: "faux".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_id: Some("faux-resp-tool".to_string()),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: now_ms(),
    }
}

pub fn faux_model() -> Model {
    Model {
        id: "faux-1".to_string(),
        name: "Faux Model".to_string(),
        api: "faux".to_string(),
        provider: "faux".to_string(),
        base_url: "http://localhost:0".to_string(),
        reasoning: true,
        input: vec!["text".to_string(), "image".to_string()],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 128_000,
        max_tokens: 4096,
        headers: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_faux_streaming_text() {
        let provider = FauxProvider::new();
        provider.add_response(faux_assistant_message("Hello from faux"));

        let model = faux_model();
        let context = Context::default();
        let options = StreamOptions::default();
        let cancel = CancellationToken::new();

        let mut stream = provider.stream(&model, &context, &options, cancel);
        let final_message = stream.collect_final_message().await.unwrap();

        assert_eq!(final_message.text_content(), "Hello from faux");
        assert_eq!(final_message.stop_reason, StopReason::Stop);

        let recorded = provider.recorded_requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].model.id, "faux-1");
    }

    #[tokio::test]
    async fn test_faux_tool_call_stream() {
        let provider = FauxProvider::new();
        let call = faux_tool_call("c1", "bash", serde_json::json!({ "command": "ls" }));
        provider.add_response(faux_tool_call_message(vec![call]));

        let model = faux_model();
        let context = Context::default();
        let options = StreamOptions::default();
        let cancel = CancellationToken::new();

        let mut stream = provider.stream(&model, &context, &options, cancel);
        let final_message = stream.collect_final_message().await.unwrap();

        assert_eq!(final_message.stop_reason, StopReason::ToolUse);
        let calls = final_message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(
            calls[0].arguments.get("command"),
            Some(&serde_json::Value::String("ls".to_string()))
        );
    }

    #[tokio::test]
    async fn test_faux_handler() {
        let provider = FauxProvider::new();
        provider.add_handler(|model, _ctx, _opts| {
            faux_assistant_message(format!("Answer for model {}", model.id))
        });

        let model = faux_model();
        let context = Context::default();
        let options = StreamOptions::default();
        let cancel = CancellationToken::new();

        let mut stream = provider.stream(&model, &context, &options, cancel);
        let final_message = stream.collect_final_message().await.unwrap();

        assert_eq!(final_message.text_content(), "Answer for model faux-1");
    }
}
