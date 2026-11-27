use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, Model, StreamOptions,
};
use anyhow::{anyhow, Result};
use tokio::sync::mpsc::{channel, Receiver, Sender};

const EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone, Debug)]
pub struct AssistantEventSender {
    sender: Sender<AssistantMessageEvent>,
}

impl AssistantEventSender {
    pub fn push(&self, event: AssistantMessageEvent) -> bool {
        self.sender.try_send(event).is_ok()
    }

    pub fn try_push(
        &self,
        event: AssistantMessageEvent,
    ) -> Result<(), Box<tokio::sync::mpsc::error::TrySendError<AssistantMessageEvent>>> {
        self.sender.try_send(event).map_err(Box::new)
    }

    pub async fn send(&self, event: AssistantMessageEvent) -> bool {
        self.sender.send(event).await.is_ok()
    }

    pub async fn send_batch(&self, events: Vec<AssistantMessageEvent>) -> bool {
        for event in events {
            if self.sender.send(event).await.is_err() {
                return false;
            }
        }
        true
    }
}

pub struct AssistantEventStream {
    receiver: Receiver<AssistantMessageEvent>,
}

impl AssistantEventStream {
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        self.receiver.recv().await
    }

    pub async fn collect_final_message(&mut self) -> Result<AssistantMessage> {
        while let Some(event) = self.next().await {
            match event {
                AssistantMessageEvent::Done { message, .. } => {
                    return Ok(message);
                }
                AssistantMessageEvent::Error { error, .. } => {
                    return Ok(error);
                }
                AssistantMessageEvent::Start { .. }
                | AssistantMessageEvent::TextStart { .. }
                | AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::TextEnd { .. }
                | AssistantMessageEvent::ThinkingStart { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ThinkingEnd { .. }
                | AssistantMessageEvent::ToolCallStart { .. }
                | AssistantMessageEvent::ToolCallDelta { .. }
                | AssistantMessageEvent::ToolCallEnd { .. } => {}
            }
        }
        Err(anyhow!("Stream closed without final Done or Error event"))
    }
}

pub fn create_event_stream() -> (AssistantEventSender, AssistantEventStream) {
    let (sender, receiver) = channel(EVENT_CHANNEL_CAPACITY);
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

        assert!(sender.push(AssistantMessageEvent::Start {
            partial: message.clone(),
        }));
        assert!(sender.push(AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message: message.clone(),
        }));

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

        assert!(sender.push(AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: error_msg.clone(),
        }));

        let final_msg = stream.collect_final_message().await.unwrap();
        assert_eq!(final_msg.stop_reason, StopReason::Error);
        assert_eq!(final_msg.error_message.as_deref(), Some("rate limited"));
    }

    #[tokio::test]
    async fn test_stream_truncation_without_terminal_event_errors() {
        let (sender, mut stream) = create_event_stream();
        let partial = AssistantMessage::empty_for_model(&crate::faux::faux_model());
        assert!(sender.push(AssistantMessageEvent::Start { partial }));
        drop(sender);
        let err = stream.collect_final_message().await.unwrap_err();
        assert!(err.to_string().contains("without final Done or Error"));
    }

    #[tokio::test]
    async fn test_stream_try_push_reports_closed_and_full() {
        let (sender, stream) = create_event_stream();
        let partial = AssistantMessage::empty_for_model(&crate::faux::faux_model());

        assert!(sender
            .try_push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .is_ok());

        for _ in 1..EVENT_CHANNEL_CAPACITY {
            assert!(sender
                .try_push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                })
                .is_ok());
        }

        assert!(matches!(
            sender
                .try_push(AssistantMessageEvent::Start {
                    partial: partial.clone(),
                })
                .map_err(|e| *e),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        assert!(!sender.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        }));

        drop(stream);
        assert!(matches!(
            sender
                .try_push(AssistantMessageEvent::Start { partial })
                .map_err(|e| *e),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
        ));
    }
}
