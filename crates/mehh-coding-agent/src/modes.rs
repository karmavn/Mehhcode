//! Non-interactive and interactive execution modes for CLI and script execution.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::Result;

use mehh_agent::AgentEvent;
use mehh_ai::{AssistantMessageEvent, Message};

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
    let streamed_any = Arc::new(AtomicBool::new(false));
    let streamed_flag = streamed_any.clone();

    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = match mode {
        OutputMode::Json => Arc::new(|event: AgentEvent| {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
        }),
        OutputMode::Text => Arc::new(move |event: AgentEvent| {
            if let AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } = event
            {
                print!("{delta}");
                let _ = std::io::stdout().flush();
                streamed_flag.store(true, Ordering::SeqCst);
            }
        }),
    };

    let new_messages = session.prompt(prompt, Some(on_event)).await?;

    let final_text = new_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(a.text_content()),
            _ => None,
        })
        .unwrap_or_default();

    if mode == OutputMode::Text {
        if streamed_any.load(Ordering::SeqCst) {
            println!();
        } else if !final_text.is_empty() {
            println!("{final_text}");
        }
    }

    Ok(final_text)
}
