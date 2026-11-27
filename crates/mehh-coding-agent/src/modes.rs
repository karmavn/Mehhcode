//! Non-interactive and interactive execution modes for CLI and script execution.

use anyhow::Result;
use std::io::Write;
use std::sync::{Arc, Mutex};

use mehh_agent::AgentEvent;
use mehh_ai::{AssistantMessageEvent, Message, StopReason};

use crate::agent_session::AgentSession;

pub use crate::interactive::run_interactive_mode;
pub use crate::rpc::run_rpc_mode;

/// Output rendering mode for print executions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

/// Executes a single prompt in print mode, streaming events according to `mode`.
/// Returns the text content of the final assistant message.
pub async fn run_print_mode(
    session: &mut AgentSession,
    prompt: &str,
    mode: OutputMode,
) -> Result<String> {
    run_print_mode_content(
        session,
        mehh_ai::UserContent::Text(prompt.to_string()),
        mode,
    )
    .await
}

/// Streams `OutputMode::Text` deltas to an arbitrary `Write` sink, inserting a
/// newline separator between consecutive assistant messages so multi-step replies
/// do not concatenate mid-word.
struct TextModeStreamPrinter<W: Write> {
    writer: W,
    streamed_any: bool,
    need_separator: bool,
    last_ended_with_newline: bool,
}

impl<W: Write> TextModeStreamPrinter<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            streamed_any: false,
            need_separator: false,
            last_ended_with_newline: false,
        }
    }

    fn handle_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                if delta.is_empty() {
                    return;
                }
                if self.need_separator {
                    if !self.last_ended_with_newline {
                        let _ = writeln!(self.writer);
                    }
                    self.need_separator = false;
                }
                let _ = write!(self.writer, "{delta}");
                let _ = self.writer.flush();
                self.streamed_any = true;
                self.last_ended_with_newline = delta.ends_with('\n');
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(_),
            } if self.streamed_any => {
                self.need_separator = true;
            }
            _ => {}
        }
    }
}

/// Executes arbitrary user content in print mode, streaming events according to `mode`.
/// Returns the text content of the final assistant message.
pub async fn run_print_mode_content(
    session: &mut AgentSession,
    content: mehh_ai::UserContent,
    mode: OutputMode,
) -> Result<String> {
    let text_printer = Arc::new(Mutex::new(TextModeStreamPrinter::new(std::io::stdout())));
    let printer_cb = text_printer.clone();

    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = match mode {
        OutputMode::Json => Arc::new(|event: AgentEvent| {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
        }),
        OutputMode::Text => Arc::new(move |event: AgentEvent| {
            mehh_ai::lock_recover(&printer_cb).handle_event(&event);
        }),
    };

    let new_messages = session.prompt_content(content, Some(on_event)).await?;

    let final_assistant = new_messages.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => Some(a),
        _ => None,
    });

    if let Some(ast) = final_assistant {
        if matches!(ast.stop_reason, StopReason::Error | StopReason::Aborted) {
            anyhow::bail!(
                "{}",
                ast.error_message
                    .clone()
                    .unwrap_or_else(|| format!("Agent stopped with {:?}", ast.stop_reason))
            );
        }
    }

    let final_text = final_assistant
        .map(|a| a.text_content())
        .unwrap_or_default();

    if mode == OutputMode::Text {
        let streamed = mehh_ai::lock_recover(&text_printer).streamed_any;
        if streamed {
            println!();
        } else if !final_text.is_empty() {
            println!("{final_text}");
        }
    }

    Ok(final_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use crate::settings::Settings;
    use mehh_ai::{faux_model, now_ms, AssistantMessage, FauxProvider, TextContent, Usage};

    #[tokio::test]
    async fn test_run_print_mode_propagates_provider_error() {
        let dir = std::env::temp_dir().join(format!("mehh_print_err_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let faux = Arc::new(FauxProvider::with_responses(vec![AssistantMessage {
            content: vec![mehh_ai::AssistantContentBlock::Text(TextContent {
                text: String::new(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("upstream 500 failure".to_string()),
            timestamp: now_ms(),
        }]));
        let mut session = AgentSession::new(
            dir.clone(),
            faux_model(),
            faux,
            SessionManager::in_memory(&dir),
            Settings::default(),
        );
        let err = run_print_mode(&mut session, "hello", OutputMode::Json)
            .await
            .expect_err("print mode must fail when assistant stops with Error");
        assert!(format!("{err:#}").contains("upstream 500 failure"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn print_mode_text_separates_consecutive_assistant_messages_with_newline() {
        let dir = std::env::temp_dir().join(format!("mehh_print_sep_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let ast = |stop_reason| AssistantMessage {
            content: vec![],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: now_ms(),
        };
        let mut buf = Vec::<u8>::new();
        {
            let mut printer = TextModeStreamPrinter::new(&mut buf);
            printer.handle_event(&AgentEvent::MessageUpdate {
                message: ast(StopReason::Stop),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "Step one.".to_string(),
                    partial: ast(StopReason::Stop),
                },
            });
            printer.handle_event(&AgentEvent::MessageEnd {
                message: Message::Assistant(ast(StopReason::ToolUse)),
            });
            printer.handle_event(&AgentEvent::MessageUpdate {
                message: ast(StopReason::Stop),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "Step two.".to_string(),
                    partial: ast(StopReason::Stop),
                },
            });
        }
        let rendered = String::from_utf8(buf).unwrap();
        assert_eq!(rendered, "Step one.\nStep two.");
        let _ = std::fs::remove_dir_all(dir);
    }
}
