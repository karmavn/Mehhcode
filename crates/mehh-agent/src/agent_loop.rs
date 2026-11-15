use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
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

/// Default upper bound on assistant turns per `run_agent_loop` invocation.
pub const DEFAULT_MAX_AGENT_TURNS: usize = 200;

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub stream_options: StreamOptions,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub dynamic_tools: Option<Arc<RwLock<Vec<Arc<dyn AgentTool>>>>>,
    pub max_turns: Option<usize>,
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
            dynamic_tools: None,
            max_turns: Some(DEFAULT_MAX_AGENT_TURNS),
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

fn resolve_tools(config: &AgentLoopConfig) -> Vec<Arc<dyn AgentTool>> {
    let mut tools = config.tools.clone();
    if let Some(ref dynamic_lock) = config.dynamic_tools {
        let dyn_snapshot = dynamic_lock
            .read()
            .expect("dynamic_tools RwLock poisoned")
            .clone();
        for tool in dyn_snapshot {
            if !tools.iter().any(|existing| existing.name() == tool.name()) {
                tools.push(tool);
            }
        }
    }
    tools
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
    let mut turns_completed: usize = 0;

    emit(AgentEvent::AgentStart);
    if config.max_turns == Some(0) || config.cancel.is_cancelled() {
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
        emit(AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        });
        return new_messages;
    }

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

    let mut pending_messages: Vec<Message> = drain_queue(&config.steering_queue, config.steering_mode);

    'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if config.cancel.is_cancelled()
                || config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                break 'outer;
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
            turns_completed += 1;

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
                    let batch = fail_truncated_tool_calls(&tool_calls, &on_event);
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

            if config.cancel.is_cancelled()
                || config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                break 'outer;
            }

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

    let active_tools = resolve_tools(config);
    let tool_definitions: Vec<ToolDefinition> = active_tools
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

fn emit_synthetic_tool_errors(
    tool_calls: &[ToolCall],
    make_reason: impl Fn(&ToolCall) -> String,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> Vec<ToolResultMessage> {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let mut results = Vec::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        });

        let result = AgentToolResult::text(make_reason(tool_call));

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

fn fail_truncated_tool_calls(
    tool_calls: &[ToolCall],
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> Vec<ToolResultMessage> {
    emit_synthetic_tool_errors(
        tool_calls,
        |tc| {
            format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tc.name
            )
        },
        on_event,
    )
}

struct SingleToolOutcome {
    tool_result_message: ToolResultMessage,
    terminate: bool,
}

async fn execute_single_tool(
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    tools: &[Arc<dyn AgentTool>],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> SingleToolOutcome {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let tool_opt = tools.iter().find(|t| t.name() == tool_call.name);
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
    tools: &[Arc<dyn AgentTool>],
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

    for (idx, tc) in tool_calls.iter().enumerate() {
        if config.cancel.is_cancelled() {
            let remaining = emit_synthetic_tool_errors(
                &tool_calls[idx..],
                |_| "Tool call aborted because operation was cancelled".to_string(),
                on_event,
            );
            messages.extend(remaining);
            break;
        }

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.arguments.clone(),
        });

        let outcome = execute_single_tool(assistant_message, tc, tools, config, on_event).await;

        emit(AgentEvent::MessageStart {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });

        messages.push(outcome.tool_result_message.clone());
        outcomes.push(outcome);

        if config.cancel.is_cancelled() {
            if idx + 1 < tool_calls.len() {
                let remaining = emit_synthetic_tool_errors(
                    &tool_calls[idx + 1..],
                    |_| "Tool call aborted because operation was cancelled".to_string(),
                    on_event,
                );
                messages.extend(remaining);
            }
            break;
        }
    }

    let terminate = !outcomes.is_empty() && outcomes.iter().all(|o| o.terminate);
    (messages, terminate)
}

async fn execute_tool_calls_parallel(
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    tools: &[Arc<dyn AgentTool>],
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
        .map(|tc| execute_single_tool(assistant_message, tc, tools, config, on_event));

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
    let tools = resolve_tools(config);
    let has_sequential = tool_calls.iter().any(|tc| {
        tools
            .iter()
            .find(|t| t.name() == tc.name)
            .is_some_and(|t| {
                t.execution_mode() == ToolExecutionMode::Sequential
            })
    });

    if has_sequential {
        execute_tool_calls_sequential(assistant_message, tool_calls, &tools, config, on_event).await
    } else {
        execute_tool_calls_parallel(assistant_message, tool_calls, &tools, config, on_event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{
        faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message, FauxProvider,
        ToolResultContentBlock, UserContent, UserMessage,
    };

    struct DummyTool {
        tool_name: String,
        mode: ToolExecutionMode,
        on_exec: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[async_trait::async_trait]
    impl AgentTool for DummyTool {
        fn name(&self) -> &str {
            &self.tool_name
        }
        fn description(&self) -> &str {
            "Dummy tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn execution_mode(&self) -> ToolExecutionMode {
            self.mode
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            if let Some(ref cb) = self.on_exec {
                cb();
            }
            Ok(AgentToolResult::text("ok"))
        }
    }

    #[tokio::test]
    async fn test_agent_loop_enforces_max_turns() {
        let provider = Arc::new(FauxProvider::new());
        for i in 0..5 {
            let call = faux_tool_call(&format!("c{i}"), "ping", serde_json::json!({}));
            provider.add_response(faux_tool_call_message(vec![call]));
        }

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![Arc::new(DummyTool {
            tool_name: "ping".to_string(),
            mode: ToolExecutionMode::Parallel,
            on_exec: None,
        })];
        config.max_turns = Some(2);

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("go".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(vec![prompt], vec![], config, None).await;
        // Prompt + 2 * (Assistant + ToolResult) = 5 messages
        let assistant_count = out
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        assert_eq!(assistant_count, 2);
    }

    #[tokio::test]
    async fn test_agent_loop_dynamic_tools_activated_mid_loop() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![faux_tool_call(
            "c1",
            "activator",
            serde_json::json!({}),
        )]));
        provider.add_response(faux_tool_call_message(vec![faux_tool_call(
            "c2",
            "late_tool",
            serde_json::json!({}),
        )]));
        provider.add_response(faux_assistant_message("done"));

        let dynamic = Arc::new(RwLock::new(Vec::<Arc<dyn AgentTool>>::new()));
        let dyn_clone = dynamic.clone();

        let activator = Arc::new(DummyTool {
            tool_name: "activator".to_string(),
            mode: ToolExecutionMode::Sequential,
            on_exec: Some(Arc::new(move || {
                dyn_clone.write().unwrap().push(Arc::new(DummyTool {
                    tool_name: "late_tool".to_string(),
                    mode: ToolExecutionMode::Parallel,
                    on_exec: None,
                }));
            })),
        });

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![activator];
        config.dynamic_tools = Some(dynamic);

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("activate".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(vec![prompt], vec![], config, None).await;
        let tool_results: Vec<&ToolResultMessage> = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(tr) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert_eq!(tool_results[1].tool_name, "late_tool");
        assert!(!tool_results[1].is_error);
    }

    #[tokio::test]
    async fn test_sequential_cancel_synthesizes_remaining_tool_results() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "seq", serde_json::json!({})),
            faux_tool_call("c2", "seq", serde_json::json!({})),
            faux_tool_call("c3", "seq", serde_json::json!({})),
        ]));

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let seq_tool = Arc::new(DummyTool {
            tool_name: "seq".to_string(),
            mode: ToolExecutionMode::Sequential,
            on_exec: Some(Arc::new(move || {
                cancel_clone.cancel();
            })),
        });

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![seq_tool];
        config.cancel = cancel;

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run 3".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(vec![prompt], vec![], config, None).await;

        let results: Vec<&ToolResultMessage> = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(tr) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].tool_call_id, "c1");
        assert!(!results[0].is_error);
        assert_eq!(results[1].tool_call_id, "c2");
        assert!(results[1].is_error);
        assert_eq!(results[2].tool_call_id, "c3");
        assert!(results[2].is_error);
        match &results[1].content[0] {
            ToolResultContentBlock::Text(t) => assert!(t.text.contains("aborted")),
            _ => panic!("expected text"),
        }
    }
}
