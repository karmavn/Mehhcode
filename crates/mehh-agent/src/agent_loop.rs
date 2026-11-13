use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use mehh_ai::{
    now_ms, AssistantMessage, AssistantMessageEvent, Context, LlmProvider,
    Message, Model, StopReason, StreamOptions, ToolCall, ToolDefinition,
    ToolResultMessage,
};
use crate::types::{
    tool_to_definition, AfterToolCallContext, AfterToolCallHook, AgentEvent,
    AgentTool, AgentToolResult, AgentToolUpdateCallback,
    BeforeToolCallContext, BeforeToolCallHook, QueueMode, ToolExecutionMode,
};

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub stream_options: StreamOptions,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
    pub cancel: CancellationToken,
}

impl AgentLoopConfig {
    pub fn new(model: Model, provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            model,
            provider,
            stream_options: StreamOptions::default(),
            system_prompt: None,
            tools: Vec::new(),
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            before_tool_call: None,
            after_tool_call: None,
            cancel: CancellationToken::new(),
        }
    }
}

fn drain_queue(queue: &Mutex<VecDeque<Message>>, mode: QueueMode) -> Vec<Message> {
    let mut q = queue.lock().unwrap();
    match mode {
        QueueMode::All => q.drain(..).collect(),
        QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
    }
}

pub async fn run_agent_loop(
    prompts: Vec<Message>,
    history: Vec<Message>,
    config: AgentLoopConfig,
    on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> Vec<Message> {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let mut current_messages = history;
    let mut new_messages: Vec<Message> = Vec::new();

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);

    for prompt in prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        });
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        });
        current_messages.push(prompt.clone());
        new_messages.push(prompt);
    }

    if config.cancel.is_cancelled() {
        emit(AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        });
        return new_messages;
    }

    let mut pending_messages: Vec<Message> = drain_queue(&config.steering_queue, config.steering_mode);

    'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if config.cancel.is_cancelled() {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return new_messages;
            }

            for msg in pending_messages.drain(..) {
                emit(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                emit(AgentEvent::MessageEnd {
                    message: msg.clone(),
                });
                current_messages.push(msg.clone());
                new_messages.push(msg);
            }

            let assistant_message =
                stream_assistant_response(&current_messages, &config, &on_event).await;

            current_messages.push(Message::Assistant(assistant_message.clone()));
            new_messages.push(Message::Assistant(assistant_message.clone()));

            if assistant_message.stop_reason == StopReason::Error
                || assistant_message.stop_reason == StopReason::Aborted
            {
                emit(AgentEvent::TurnEnd {
                    message: assistant_message,
                    tool_results: Vec::new(),
                });
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return new_messages;
            }

            let tool_calls = assistant_message.tool_calls();
            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                if assistant_message.stop_reason == StopReason::Length {
                    let batch = fail_truncated_tool_calls(&tool_calls, &on_event).await;
                    for tr in &batch {
                        current_messages.push(Message::ToolResult(tr.clone()));
                        new_messages.push(Message::ToolResult(tr.clone()));
                    }
                    tool_results.extend(batch);
                    has_more_tool_calls = true;
                } else {
                    let (batch, terminate) = execute_tool_calls(
                        &assistant_message,
                        &tool_calls,
                        &config,
                        &on_event,
                    )
                    .await;
                    for tr in &batch {
                        current_messages.push(Message::ToolResult(tr.clone()));
                        new_messages.push(Message::ToolResult(tr.clone()));
                    }
                    tool_results.extend(batch);
                    has_more_tool_calls = !terminate;
                }
            }

            emit(AgentEvent::TurnEnd {
                message: assistant_message,
                tool_results,
            });

            pending_messages = drain_queue(&config.steering_queue, config.steering_mode);

            if has_more_tool_calls || !pending_messages.is_empty() {
                emit(AgentEvent::TurnStart);
            }
        }

        let follow_up_messages: Vec<Message> = drain_queue(&config.follow_up_queue, config.follow_up_mode);

        if !follow_up_messages.is_empty() {
            pending_messages = follow_up_messages;
            emit(AgentEvent::TurnStart);
            continue 'outer;
        }

        break 'outer;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });

    new_messages
}

async fn stream_assistant_response(
    messages: &[Message],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> AssistantMessage {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let tool_definitions: Vec<ToolDefinition> = config
        .tools
        .iter()
        .map(|t| tool_to_definition(t.as_ref()))
        .collect();

    let context = Context {
        system_prompt: config.system_prompt.clone(),
        messages: messages.to_vec(),
        tools: if tool_definitions.is_empty() {
            None
        } else {
            Some(tool_definitions)
        },
    };

    let mut stream = config.provider.stream(
        &config.model,
        &context,
        &config.stream_options,
        config.cancel.clone(),
    );

    let mut started = false;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        let partial_update = match &event {
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial.clone()),
            _ => None,
        };

        if let Some(partial) = partial_update {
            if !started {
                started = true;
                emit(AgentEvent::MessageStart {
                    message: Message::Assistant(partial.clone()),
                });
            }
            emit(AgentEvent::MessageUpdate {
                message: partial,
                assistant_message_event: event,
            });
            continue;
        }

        match event {
            AssistantMessageEvent::Start { partial } => {
                started = true;
                emit(AgentEvent::MessageStart {
                    message: Message::Assistant(partial),
                });
            }
            AssistantMessageEvent::Done { message, .. } => {
                if !started {
                    emit(AgentEvent::MessageStart {
                        message: Message::Assistant(message.clone()),
                    });
                }
                emit(AgentEvent::MessageEnd {
                    message: Message::Assistant(message.clone()),
                });
                final_message = Some(message);
                break;
            }
            AssistantMessageEvent::Error { error, .. } => {
                if !started {
                    emit(AgentEvent::MessageStart {
                        message: Message::Assistant(error.clone()),
                    });
                }
                emit(AgentEvent::MessageEnd {
                    message: Message::Assistant(error.clone()),
                });
                final_message = Some(error);
                break;
            }
            _ => unreachable!(),
        }
    }

    if let Some(msg) = final_message {
        msg
    } else {
        let mut fallback = AssistantMessage::empty_for_model(&config.model);
        let aborted = config.cancel.is_cancelled();
        fallback.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        fallback.error_message = Some(if aborted {
            "Operation aborted".to_string()
        } else {
            "Stream closed without final message".to_string()
        });
        if !started {
            emit(AgentEvent::MessageStart {
                message: Message::Assistant(fallback.clone()),
            });
        }
        emit(AgentEvent::MessageEnd {
            message: Message::Assistant(fallback.clone()),
        });
        fallback
    }
}

async fn fail_truncated_tool_calls(
    tool_calls: &[ToolCall],
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> Vec<ToolResultMessage> {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let mut results = Vec::new();
    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        });

        let err_text = format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            tool_call.name
        );
        let result = AgentToolResult::text(err_text);

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: result.clone(),
            is_error: true,
        });

        let msg = ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: result.content,
            details: result.details,
            is_error: true,
            timestamp: now_ms(),
        };

        emit(AgentEvent::MessageStart {
            message: Message::ToolResult(msg.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(msg.clone()),
        });

        results.push(msg);
    }
    results
}

struct SingleToolOutcome {
    tool_result_message: ToolResultMessage,
    terminate: bool,
}

async fn execute_single_tool(
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> SingleToolOutcome {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let tool_opt = config.tools.iter().find(|t| t.name() == tool_call.name);
    let tool = match tool_opt {
        Some(t) => t,
        None => {
            let res = AgentToolResult::text(format!("Tool {} not found", tool_call.name));
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                result: res.clone(),
                is_error: true,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                },
                terminate: false,
            };
        }
    };

    let tool_def = tool_to_definition(tool.as_ref());
    let validated_args = match mehh_ai::validation::validate_tool_arguments(&tool_def, tool_call) {
        Ok(args) => args,
        Err(err_msg) => {
            let res = AgentToolResult::text(err_msg);
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                result: res.clone(),
                is_error: true,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                },
                terminate: false,
            };
        }
    };

    let mut terminate = false;
    if let Some(ref hook) = config.before_tool_call {
        let before_ctx = BeforeToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: validated_args.clone(),
        };
        let before_res = hook(&before_ctx);
        if before_res.block {
            let reason = before_res
                .reason
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            let res = AgentToolResult::text(reason);
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                result: res.clone(),
                is_error: true,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                },
                terminate: before_res.terminate,
            };
        }
        if before_res.terminate {
            terminate = true;
        }
    }

    if config.cancel.is_cancelled() {
        let res = AgentToolResult::text("Operation aborted");
        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: res.clone(),
            is_error: true,
        });
        return SingleToolOutcome {
            tool_result_message: ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: res.content,
                details: res.details,
                is_error: true,
                timestamp: now_ms(),
            },
            terminate: false,
        };
    }

    let on_update_cb: Option<AgentToolUpdateCallback> = if on_event.is_some() {
        let on_event_clone = on_event.clone();
        let tc_id = tool_call.id.clone();
        let tc_name = tool_call.name.clone();
        let tc_args = tool_call.arguments.clone();
        Some(Arc::new(move |partial: AgentToolResult| {
            if let Some(ref cb) = on_event_clone {
                cb(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tc_id.clone(),
                    tool_name: tc_name.clone(),
                    args: tc_args.clone(),
                    partial_result: partial,
                });
            }
        }))
    } else {
        None
    };

    let exec_res = tool
        .execute(
            &tool_call.id,
            validated_args.clone(),
            config.cancel.clone(),
            on_update_cb,
        )
        .await;

    let (mut result, mut is_error) = match exec_res {
        Ok(res) => (res, false),
        Err(err_msg) => (AgentToolResult::text(err_msg), true),
    };

    if let Some(ref hook) = config.after_tool_call {
        let after_ctx = AfterToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: validated_args.clone(),
            result: result.clone(),
            is_error,
        };
        let after_res = hook(&after_ctx);
        if let Some(c) = after_res.content {
            result.content = c;
        }
        if let Some(d) = after_res.details {
            result.details = Some(d);
        }
        if let Some(e) = after_res.is_error {
            is_error = e;
        }
        if after_res.terminate {
            terminate = true;
        }
    }

    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        result: result.clone(),
        is_error,
    });

    SingleToolOutcome {
        tool_result_message: ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: result.content,
            details: result.details,
            is_error,
            timestamp: now_ms(),
        },
        terminate,
    }
}

async fn execute_tool_calls_sequential(
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> (Vec<ToolResultMessage>, bool) {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let mut outcomes = Vec::new();
    let mut messages = Vec::new();

    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.arguments.clone(),
        });

        let outcome = execute_single_tool(assistant_message, tc, config, on_event).await;

        emit(AgentEvent::MessageStart {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });

        messages.push(outcome.tool_result_message.clone());
        outcomes.push(outcome);

        if config.cancel.is_cancelled() {
            break;
        }
    }

    let terminate = !outcomes.is_empty() && outcomes.iter().all(|o| o.terminate);
    (messages, terminate)
}

async fn execute_tool_calls_parallel(
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> (Vec<ToolResultMessage>, bool) {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.arguments.clone(),
        });
    }

    let futures = tool_calls
        .iter()
        .map(|tc| execute_single_tool(assistant_message, tc, config, on_event));

    let outcomes = futures::future::join_all(futures).await;

    let mut messages = Vec::new();
    for outcome in &outcomes {
        emit(AgentEvent::MessageStart {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });
        messages.push(outcome.tool_result_message.clone());
    }

    let terminate = !outcomes.is_empty() && outcomes.iter().all(|o| o.terminate);
    (messages, terminate)
}

async fn execute_tool_calls(
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> (Vec<ToolResultMessage>, bool) {
    let has_sequential = tool_calls.iter().any(|tc| {
        config
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .is_some_and(|t| {
                t.execution_mode() == ToolExecutionMode::Sequential
            })
    });

    if has_sequential {
        execute_tool_calls_sequential(assistant_message, tool_calls, config, on_event).await
    } else {
        execute_tool_calls_parallel(assistant_message, tool_calls, config, on_event).await
    }
}
