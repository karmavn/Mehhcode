use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, Model, StreamOptions,
};
use anyhow::{anyhow, Result};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug)]
pub struct AssistantEventSender {
    sender: UnboundedSender<AssistantMessageEvent>,
}

impl AssistantEventSender {
    pub fn push(&self, event: AssistantMessageEvent) {
        let _ = self.sender.send(event);
    }
}

pub struct AssistantEventStream {
    receiver: UnboundedReceiver<AssistantMessageEvent>,
}

impl AssistantEventStream {
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        self.receiver.recv().await
    }

    pub async fn collect_final_message(&mut self) -> Result<AssistantMessage> {
        let mut last_partial: Option<AssistantMessage> = None;
        while let Some(event) = self.next().await {
            match event {
                AssistantMessageEvent::Done { message, .. } => {
                    return Ok(message);
                }
                AssistantMessageEvent::Error { error, .. } => {
                    return Ok(error);
                }
                AssistantMessageEvent::Start { partial }
                | AssistantMessageEvent::TextStart { partial, .. }
                | AssistantMessageEvent::TextDelta { partial, .. }
                | AssistantMessageEvent::TextEnd { partial, .. }
                | AssistantMessageEvent::ThinkingStart { partial, .. }
                | AssistantMessageEvent::ThinkingDelta { partial, .. }
                | AssistantMessageEvent::ThinkingEnd { partial, .. }
                | AssistantMessageEvent::ToolCallStart { partial, .. }
                | AssistantMessageEvent::ToolCallDelta { partial, .. }
                | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                    last_partial = Some(partial);
                }
            }
        }
        last_partial.ok_or_else(|| anyhow!("Stream closed without final message or error"))
    }
}

pub fn create_event_stream() -> (AssistantEventSender, AssistantEventStream) {
    let (sender, receiver) = unbounded_channel();
    (
        AssistantEventSender { sender },
        AssistantEventStream { receiver },
    )
}

#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> AssistantEventStream;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, StopReason, Usage};

    #[tokio::test]
    async fn test_stream_push_and_collect_done() {
        let (sender, mut stream) = create_event_stream();

        let message = AssistantMessage {
            content: Vec::new(),
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };

        sender.push(AssistantMessageEvent::Start {
            partial: message.clone(),
        });
        sender.push(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: message.clone(),
        });

        let final_msg = stream.collect_final_message().await.unwrap();
        assert_eq!(final_msg.model, "test-model");
        assert_eq!(final_msg.stop_reason, StopReason::Stop);
    }

    #[tokio::test]
    async fn test_stream_collect_error() {
        let (sender, mut stream) = create_event_stream();

        let error_msg = AssistantMessage {
            content: Vec::new(),
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("rate limited".to_string()),
            timestamp: now_ms(),
        };

        sender.push(AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: error_msg.clone(),
        });

        let final_msg = stream.collect_final_message().await.unwrap();
        assert_eq!(final_msg.stop_reason, StopReason::Error);
        assert_eq!(final_msg.error_message.as_deref(), Some("rate limited"));
    }
}
