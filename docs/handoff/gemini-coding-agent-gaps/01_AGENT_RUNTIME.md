# Dump 01 - Agent runtime, hook va inspection

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-agent/src/agent_loop.rs | 66.5 | 1817 | bb1fbe73391f93d946fdaa21f70089eaedea7313b5a95eb24d2c14aa08620990 |
| crates/mehh-agent/src/types.rs | 14.6 | 445 | be004decd53793409098f559563be31ce34875792b6292d731e1a4a0c70694b6 |
| crates/mehh-coding-agent/src/agent_session.rs | 75.7 | 2001 | 2e67c6202a5ec8801a68f27aabf390e2138fff6966476665300d9454bf40d705 |
| crates/mehh-coding-agent/src/tool_inspection.rs | 20.4 | 591 | 17e0831efa6d6f1baa6edeb0c9ea156b6f0f7094177117f16f3231eb0a34291a |

## crates/mehh-agent/src/agent_loop.rs

`````rust
use std::collections::{HashSet, VecDeque};
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

/// Shared thread-safe handle to dynamic tools list.
pub type DynamicToolsList = Arc<RwLock<Vec<Arc<dyn AgentTool>>>>;

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub stream_options: StreamOptions,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub dynamic_tools: Option<DynamicToolsList>,
    pub max_turns: Option<usize>,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
    pub prepare_request: Option<crate::types::PrepareRequestHook>,
    pub prepare_next_turn: Option<crate::types::PrepareNextTurnHook>,
    pub transform_context: Option<crate::types::TransformContextHook>,
    pub finish_turn: Option<crate::types::FinishTurnHook>,
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
            prepare_request: None,
            prepare_next_turn: None,
            transform_context: None,
            finish_turn: None,
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
        let mut names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
        for tool in dyn_snapshot {
            if names.insert(tool.name().to_string()) {
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
    let mut current_model = config.model.clone();
    let mut current_provider = config.provider.clone();
    let mut current_api_key = config.stream_options.api_key.clone();
    let mut current_thinking_level = config.stream_options.reasoning;

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
    let mut turn_open = true;

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
    let mut last_completed_turn: Option<crate::types::AgentTurnContext> = None;

    'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if config.cancel.is_cancelled()
                || config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                break 'outer;
            }

            if let Some(turn_ctx) = last_completed_turn.take() {
                if let Some(ref hook) = config.prepare_next_turn {
                    if let Some(update) = hook(turn_ctx, config.cancel.clone()).await {
                        if let Some(ctx) = update.context {
                            current_messages = ctx.messages;
                        }
                        if let Some(msgs) = update.messages {
                            for msg in msgs {
                                emit(AgentEvent::MessageStart {
                                    message: msg.clone(),
                                });
                                emit(AgentEvent::MessageEnd {
                                    message: msg.clone(),
                                });
                                current_messages.push(msg.clone());
                                new_messages.push(msg);
                            }
                        }
                        if let Some(m) = update.model {
                            current_model = m;
                        }
                        if let Some(p) = update.provider {
                            current_provider = p;
                        }
                        if let Some(ak) = update.api_key {
                            current_api_key = ak;
                        }
                        if let Some(tl) = update.thinking_level {
                            current_thinking_level = tl;
                        }
                    }
                }

                if pending_messages.is_empty()
                    || config.steering_mode == crate::types::QueueMode::All
                {
                    let extra_steering = drain_queue(&config.steering_queue, config.steering_mode);
                    pending_messages.extend(extra_steering);
                }
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

            if let Some(ref hook) = config.transform_context {
                current_messages = hook(current_messages, config.cancel.clone()).await;
            }

            let mut active_tools = resolve_tools(&config);

            if let Some(ref hook) = config.prepare_request {
                let req_ctx = crate::types::PrepareRequestContext {
                    context: crate::types::AgentContext {
                        messages: current_messages.clone(),
                        tools: Some(active_tools.clone()),
                    },
                    model: current_model.clone(),
                    thinking_level: current_thinking_level,
                };
                if let Some(update) = hook(req_ctx, config.cancel.clone()).await {
                    if let Some(ctx) = update.context {
                        current_messages = ctx.messages;
                        if let Some(tools) = ctx.tools {
                            active_tools = tools;
                        }
                    }
                    if let Some(m) = update.model {
                        current_model = m;
                    }
                    if let Some(p) = update.provider {
                        current_provider = p;
                    }
                    if let Some(ak) = update.api_key {
                        current_api_key = ak;
                    }
                    if let Some(tl) = update.thinking_level {
                        current_thinking_level = tl;
                    }
                }
            }

            let assistant_message = stream_assistant_response(
                &current_messages,
                &active_tools,
                &current_model,
                &current_provider,
                current_api_key.as_deref(),
                current_thinking_level,
                &config,
                &on_event,
            )
            .await;
            turns_completed += 1;

            current_messages.push(Message::Assistant(assistant_message.clone()));
            new_messages.push(Message::Assistant(assistant_message.clone()));

            if assistant_message.stop_reason == StopReason::Error
                || assistant_message.stop_reason == StopReason::Aborted
            {
                if let Some(ref hook) = config.finish_turn {
                    let turn_ctx = crate::types::AgentTurnContext {
                        turn_index: turns_completed,
                        messages: current_messages.clone(),
                        assistant_message: assistant_message.clone(),
                        tool_results: Vec::new(),
                    };
                    let _ = hook(turn_ctx, config.cancel.clone()).await;
                }
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
                        &active_tools,
                        &current_messages,
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

            let mut finish_decision = None;
            if let Some(ref hook) = config.finish_turn {
                let turn_ctx = crate::types::AgentTurnContext {
                    turn_index: turns_completed,
                    messages: current_messages.clone(),
                    assistant_message: assistant_message.clone(),
                    tool_results: tool_results.clone(),
                };
                finish_decision = hook(turn_ctx, config.cancel.clone()).await;
            }

            emit(AgentEvent::TurnEnd {
                message: assistant_message.clone(),
                tool_results: tool_results.clone(),
            });
            turn_open = false;

            if let Some(decision) = finish_decision {
                match decision.action {
                    crate::types::TurnAction::End => break 'outer,
                    crate::types::TurnAction::Continue => {
                        has_more_tool_calls = true;
                    }
                }
            }

            if config.cancel.is_cancelled()
                || config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                break 'outer;
            }

            if config.prepare_next_turn.is_some() {
                last_completed_turn = Some(crate::types::AgentTurnContext {
                    turn_index: turns_completed,
                    messages: current_messages.clone(),
                    assistant_message: assistant_message.clone(),
                    tool_results: tool_results.clone(),
                });
            }

            pending_messages = drain_queue(&config.steering_queue, config.steering_mode);

            if (has_more_tool_calls || !pending_messages.is_empty())
                && !config.cancel.is_cancelled()
                && !config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                emit(AgentEvent::TurnStart);
                turn_open = true;
            }
        }

        let follow_up_messages: Vec<Message> = drain_queue(&config.follow_up_queue, config.follow_up_mode);

        if !follow_up_messages.is_empty() {
            pending_messages = follow_up_messages;
            if !config.cancel.is_cancelled()
                && !config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                emit(AgentEvent::TurnStart);
                turn_open = true;
            }
            continue 'outer;
        }

        break 'outer;
    }

    if turn_open {
        let mut empty_msg = AssistantMessage::empty_for_model(&current_model);
        empty_msg.stop_reason = if config.cancel.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Stop
        };
        emit(AgentEvent::TurnEnd {
            message: empty_msg,
            tool_results: Vec::new(),
        });
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });

    new_messages
}

async fn stream_assistant_response(
    messages: &[Message],
    tools: &[Arc<dyn AgentTool>],
    model: &Model,
    provider: &Arc<dyn mehh_ai::LlmProvider>,
    api_key: Option<&str>,
    thinking_level: Option<mehh_ai::ThinkingLevel>,
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> AssistantMessage {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    let tool_definitions: Vec<ToolDefinition> = tools
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

    let mut stream_opts = config.stream_options.clone();
    stream_opts.reasoning = thinking_level;
    stream_opts.api_key = api_key.map(str::to_string);

    let mut stream = provider.stream(
        model,
        &context,
        &stream_opts,
        config.cancel.clone(),
    );

    let mut started = false;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        let partial_update: Option<&AssistantMessage> = match &event {
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
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
                message: partial.clone(),
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
        let mut fallback = AssistantMessage::empty_for_model(model);
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
            duration_ms: None,
        });

        let msg = ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: result.content,
            details: result.details,
            is_error: true,
            timestamp: now_ms(),
            duration_ms: None,
            usage: None,
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

fn aborted_tool_outcome(
    tool_call: &ToolCall,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> SingleToolOutcome {
    let result = AgentToolResult::text("Operation aborted");
    if let Some(ref cb) = on_event {
        cb(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: result.clone(),
            is_error: true,
            duration_ms: None,
        });
    }
    SingleToolOutcome {
        tool_result_message: ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: result.content,
            details: result.details,
            is_error: true,
            timestamp: now_ms(),
            duration_ms: None,
            usage: None,
        },
        terminate: false,
    }
}

async fn execute_single_tool(
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    tools: &[Arc<dyn AgentTool>],
    current_messages: &[Message],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> SingleToolOutcome {
    let emit = |event: AgentEvent| {
        if let Some(ref cb) = on_event {
            cb(event);
        }
    };

    if config.cancel.is_cancelled() {
        return aborted_tool_outcome(tool_call, on_event);
    }

    let agent_ctx = if config.before_tool_call.is_some() || config.after_tool_call.is_some() {
        Some(crate::types::AgentContext {
            messages: current_messages.to_vec(),
            tools: Some(tools.to_vec()),
        })
    } else {
        None
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
                duration_ms: None,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                    duration_ms: None,
                    usage: None,
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
                duration_ms: None,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                    duration_ms: None,
                    usage: None,
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
            context: agent_ctx.clone().expect("agent context required by before_tool_call"),
        };
        let before_res = hook(before_ctx, config.cancel.clone()).await;
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
                duration_ms: None,
            });
            return SingleToolOutcome {
                tool_result_message: ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: res.content,
                    details: res.details,
                    is_error: true,
                    timestamp: now_ms(),
                    duration_ms: None,
                    usage: None,
                },
                terminate: before_res.terminate,
            };
        }
        if before_res.terminate {
            terminate = true;
        }
    }

    if config.cancel.is_cancelled() {
        return aborted_tool_outcome(tool_call, on_event);
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

    let start = std::time::Instant::now();
    let exec_res = tool
        .execute(
            &tool_call.id,
            validated_args.clone(),
            config.cancel.clone(),
            on_update_cb,
        )
        .await;
    let duration_ms = Some(start.elapsed().as_millis() as u64);

    let (mut result, mut is_error) = match exec_res {
        Ok(res) => {
            let err = res.is_error.unwrap_or(false);
            if res.terminate == Some(true) {
                terminate = true;
            }
            (res, err)
        }
        Err(err_msg) => (AgentToolResult::text(err_msg), true),
    };

    if let Some(ref hook) = config.after_tool_call {
        let after_ctx = AfterToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: validated_args.clone(),
            result: result.clone(),
            is_error,
            context: agent_ctx.expect("agent context required by after_tool_call"),
        };
        let after_res = hook(after_ctx, config.cancel.clone()).await;
        let content_replaced = after_res.content.is_some();
        if let Some(c) = after_res.content {
            result.content = c;
        }
        if let Some(d) = after_res.details {
            result.details = Some(d);
        }
        if let Some(e) = after_res.is_error {
            is_error = e;
        }
        if let Some(u) = after_res.usage {
            result.usage = Some(u);
        }
        if let Some(t) = after_res.terminate {
            terminate = t;
        }
        if after_res.structured_content.is_some() {
            result.structured_content = after_res.structured_content;
        } else if content_replaced {
            result.structured_content = None;
        }
    }

    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        result: result.clone(),
        is_error,
        duration_ms,
    });

    let usage = result.usage.clone();
    SingleToolOutcome {
        tool_result_message: ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: result.content,
            details: result.details,
            is_error,
            timestamp: now_ms(),
            duration_ms,
            usage,
        },
        terminate,
    }
}

async fn execute_tool_calls_sequential(
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    tools: &[Arc<dyn AgentTool>],
    current_messages: &[Message],
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

        let outcome = execute_single_tool(assistant_message, tc, tools, current_messages, config, on_event).await;

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
    current_messages: &[Message],
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
        .map(|tc| execute_single_tool(assistant_message, tc, tools, current_messages, config, on_event));

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
    tools: &[Arc<dyn AgentTool>],
    current_messages: &[Message],
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> (Vec<ToolResultMessage>, bool) {
    let has_sequential = tool_calls.iter().any(|tc| {
        tools
            .iter()
            .find(|t| t.name() == tc.name)
            .is_some_and(|t| {
                t.execution_mode() == ToolExecutionMode::Sequential
            })
    });

    if has_sequential {
        execute_tool_calls_sequential(assistant_message, tool_calls, &tools, current_messages, config, on_event).await
    } else {
        execute_tool_calls_parallel(assistant_message, tool_calls, &tools, current_messages, config, on_event).await
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
            let call = faux_tool_call(format!("c{i}"), "ping", serde_json::json!({}));
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

    #[tokio::test]
    async fn executed_tool_call_records_duration_ms() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "dummy", serde_json::json!({})),
        ]));

        let dummy_tool = Arc::new(DummyTool {
            tool_name: "dummy".to_string(),
            mode: ToolExecutionMode::Sequential,
            on_exec: None,
        });

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![dummy_tool];

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run tool".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(
            vec![prompt],
            vec![],
            config,
            Some(Arc::new(move |ev| {
                events_clone.lock().unwrap().push(ev);
            })),
        )
        .await;

        let results: Vec<&ToolResultMessage> = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(tr) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].duration_ms.is_some(), "persisted ToolResultMessage must have duration_ms");

        let recorded_events = events.lock().unwrap();
        let end_event = recorded_events.iter().find_map(|ev| match ev {
            AgentEvent::ToolExecutionEnd { tool_call_id, duration_ms, .. } if tool_call_id == "c1" => {
                Some(*duration_ms)
            }
            _ => None,
        });
        assert!(end_event.is_some(), "ToolExecutionEnd must be emitted");
        assert!(end_event.unwrap().is_some(), "ToolExecutionEnd must record duration_ms");
    }

    #[tokio::test]
    async fn blocked_tool_call_omits_duration_ms() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "dummy", serde_json::json!({})),
        ]));

        let dummy_tool = Arc::new(DummyTool {
            tool_name: "dummy".to_string(),
            mode: ToolExecutionMode::Sequential,
            on_exec: None,
        });

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![dummy_tool];
        config.before_tool_call = Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async {
                crate::types::BeforeToolCallResult {
                    block: true,
                    reason: Some("blocked by policy".to_string()),
                    terminate: false,
                }
            })
        }));

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run blocked tool".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(
            vec![prompt],
            vec![],
            config,
            Some(Arc::new(move |ev| {
                events_clone.lock().unwrap().push(ev);
            })),
        )
        .await;

        let results: Vec<&ToolResultMessage> = out
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(tr) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].duration_ms, None, "blocked tool call must omit duration_ms on message");

        let recorded_events = events.lock().unwrap();
        let end_event = recorded_events.iter().find_map(|ev| match ev {
            AgentEvent::ToolExecutionEnd { tool_call_id, duration_ms, .. } if tool_call_id == "c1" => {
                Some(*duration_ms)
            }
            _ => None,
        });
        assert!(end_event.is_some(), "ToolExecutionEnd must be emitted");
        assert_eq!(end_event.unwrap(), None, "blocked tool call must omit duration_ms on event");
    }

    #[tokio::test]
    async fn async_tool_hooks_receive_context_and_override_structured_content_usage() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "structured_tool", serde_json::json!({})),
        ]));

        struct StructuredTool;
        #[async_trait::async_trait]
        impl AgentTool for StructuredTool {
            fn name(&self) -> &str { "structured_tool" }
            fn description(&self) -> &str { "tool with structured content" }
            fn parameters(&self) -> serde_json::Value { serde_json::json!({ "type": "object" }) }
            async fn execute(
                &self,
                _tool_call_id: &str,
                _params: serde_json::Map<String, serde_json::Value>,
                _cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                Ok(AgentToolResult {
                    content: vec![ToolResultContentBlock::Text(mehh_ai::TextContent {
                        text: "original content".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    structured_content: Some(serde_json::json!({ "score": 42 })),
                    usage: Some(mehh_ai::Usage {
                        input: 10,
                        output: 20,
                        ..Default::default()
                    }),
                    is_error: None,
                    terminate: None,
                })
            }
        }

        let context_seen = Arc::new(Mutex::new(Vec::new()));
        let context_seen_clone = context_seen.clone();

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![Arc::new(StructuredTool)];
        config.before_tool_call = Some(Arc::new(move |ctx, _cancel| {
            let seen = context_seen_clone.clone();
            Box::pin(async move {
                seen.lock().unwrap().extend(ctx.context.messages.clone());
                crate::types::BeforeToolCallResult::default()
            })
        }));
        config.after_tool_call = Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async move {
                crate::types::AfterToolCallResult {
                    content: Some(vec![ToolResultContentBlock::Text(mehh_ai::TextContent {
                        text: "replaced content".to_string(),
                        text_signature: None,
                    })]),
                    structured_content: None,
                    usage: Some(mehh_ai::Usage {
                        input: 100,
                        output: 200,
                        ..Default::default()
                    }),
                    details: None,
                    is_error: None,
                    terminate: None,
                }
            })
        }));

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run structured".to_string()),
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
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].usage, Some(mehh_ai::Usage {
            input: 100,
            output: 200,
            ..Default::default()
        }));
        assert_eq!(context_seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turn_lifecycle_hooks_control_request_context_and_finish_decision() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("turn 1 response"));
        provider.add_response(faux_assistant_message("turn 2 response"));

        let transformed_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transformed_called_clone = transformed_called.clone();

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.transform_context = Some(Arc::new(move |msgs, _cancel| {
            transformed_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { msgs })
        }));
        config.prepare_request = Some(Arc::new(|req, _cancel| {
            Box::pin(async move {
                assert!(!req.context.messages.is_empty());
                None
            })
        }));
        let finished_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished_count_clone = finished_count.clone();
        config.finish_turn = Some(Arc::new(move |_turn, _cancel| {
            let count = finished_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if count == 0 {
                    Some(crate::types::FinishTurnDecision {
                        action: crate::types::TurnAction::Continue,
                    })
                } else {
                    Some(crate::types::FinishTurnDecision {
                        action: crate::types::TurnAction::End,
                    })
                }
            })
        }));

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("hello lifecycle".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(vec![prompt], vec![], config, None).await;

        assert!(transformed_called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(finished_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn after_tool_call_can_override_terminate_hint_to_false() {
        struct TermTool;
        #[async_trait::async_trait]
        impl AgentTool for TermTool {
            fn name(&self) -> &str {
                "term_tool"
            }
            fn description(&self) -> &str {
                "Term tool"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            fn execution_mode(&self) -> ToolExecutionMode {
                ToolExecutionMode::Sequential
            }
            async fn execute(
                &self,
                _id: &str,
                _p: serde_json::Map<String, serde_json::Value>,
                _c: CancellationToken,
                _u: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                let mut res = AgentToolResult::text("did work");
                res.terminate = Some(true);
                Ok(res)
            }
        }

        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "term_tool", serde_json::json!({})),
        ]));
        provider.add_response(faux_assistant_message("continued turn response"));

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.tools = vec![Arc::new(TermTool)];

        // Hook overrides terminate hint back to false so loop continues
        config.after_tool_call = Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async move {
                crate::types::AfterToolCallResult {
                    terminate: Some(false),
                    ..Default::default()
                }
            })
        }));

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(vec![prompt], vec![], config, None).await;
        // User + Assistant (tool call) + ToolResult + Assistant (continued turn) = 4
        assert_eq!(out.len(), 4);
    }

    #[tokio::test]
    async fn finish_turn_is_invoked_on_error_or_aborted_turn() {
        let provider = Arc::new(FauxProvider::new());
        let mut err_msg = faux_assistant_message("rate limited");
        err_msg.stop_reason = StopReason::Error;
        err_msg.error_message = Some("rate limit exceeded".to_string());
        provider.add_response(err_msg);

        let finish_turn_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finish_turn_clone = finish_turn_called.clone();

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        config.finish_turn = Some(Arc::new(move |_turn, _cancel| {
            finish_turn_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { None })
        }));

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("run".to_string()),
            timestamp: now_ms(),
        });
        let _ = run_agent_loop(vec![prompt], vec![], config, None).await;
        assert!(finish_turn_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn prepare_next_turn_is_only_invoked_when_continuing_to_subsequent_turn() {
        // Case 1: Terminal single turn without tool calls -> hook must NOT be called
        let provider1 = Arc::new(FauxProvider::new());
        provider1.add_response(faux_assistant_message("single turn answer"));

        let called_count1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called_clone1 = called_count1.clone();

        let mut config1 = AgentLoopConfig::new(faux_model(), provider1);
        config1.prepare_next_turn = Some(Arc::new(move |_turn, _cancel| {
            called_clone1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { None })
        }));

        let prompt1 = Message::User(UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: now_ms(),
        });
        let _ = run_agent_loop(vec![prompt1], vec![], config1, None).await;
        assert_eq!(
            called_count1.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "prepare_next_turn must not run on a terminating turn"
        );

        // Case 2: Multi-turn with tool calls -> hook must run before subsequent turn
        struct SimpleEchoTool;
        #[async_trait::async_trait]
        impl AgentTool for SimpleEchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "Echo"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            fn execution_mode(&self) -> ToolExecutionMode {
                ToolExecutionMode::Sequential
            }
            async fn execute(
                &self,
                _id: &str,
                _p: serde_json::Map<String, serde_json::Value>,
                _c: CancellationToken,
                _u: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                Ok(AgentToolResult::text("echoed"))
            }
        }

        let provider2 = Arc::new(FauxProvider::new());
        provider2.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "echo", serde_json::json!({})),
        ]));
        provider2.add_response(faux_assistant_message("final response"));

        let called_count2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called_clone2 = called_count2.clone();

        let mut config2 = AgentLoopConfig::new(faux_model(), provider2);
        config2.tools = vec![Arc::new(SimpleEchoTool)];
        config2.prepare_next_turn = Some(Arc::new(move |_turn, _cancel| {
            called_clone2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { None })
        }));

        let prompt2 = Message::User(UserMessage {
            content: UserContent::Text("run echo".to_string()),
            timestamp: now_ms(),
        });
        let _ = run_agent_loop(vec![prompt2], vec![], config2, None).await;
        assert_eq!(
            called_count2.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "prepare_next_turn must run exactly once before the continued turn"
        );
    }

    #[tokio::test]
    async fn steering_queue_one_at_a_time_does_not_double_drain_across_turn_prep() {
        let provider = Arc::new(FauxProvider::new());
        // Turn 1: model calls tool
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("call_1", "push_steering", serde_json::json!({})),
        ]));
        // Turn 2: response after first steering message
        provider.add_response(faux_assistant_message("turn 2 reply"));
        // Turn 3: response after second steering message
        provider.add_response(faux_assistant_message("turn 3 reply"));

        let mut config = AgentLoopConfig::new(faux_model(), provider);
        let queue_clone = config.steering_queue.clone();
        struct PushSteeringTool {
            queue: Arc<Mutex<VecDeque<Message>>>,
        }
        #[async_trait::async_trait]
        impl AgentTool for PushSteeringTool {
            fn name(&self) -> &str {
                "push_steering"
            }
            fn description(&self) -> &str {
                "PushSteering"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            fn execution_mode(&self) -> ToolExecutionMode {
                ToolExecutionMode::Sequential
            }
            async fn execute(
                &self,
                _id: &str,
                _p: serde_json::Map<String, serde_json::Value>,
                _c: CancellationToken,
                _u: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                let mut q = self.queue.lock().unwrap();
                q.push_back(Message::User(UserMessage {
                    content: UserContent::Text("steer 1".to_string()),
                    timestamp: now_ms(),
                }));
                q.push_back(Message::User(UserMessage {
                    content: UserContent::Text("steer 2".to_string()),
                    timestamp: now_ms(),
                }));
                Ok(AgentToolResult::text("pushed"))
            }
        }

        config.tools = vec![Arc::new(PushSteeringTool { queue: queue_clone })];
        config.steering_mode = QueueMode::OneAtATime;

        let turn_messages: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let tm_clone = turn_messages.clone();

        let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
            if let AgentEvent::TurnStart = event {
                tm_clone.lock().unwrap().push(Vec::new());
            } else if let AgentEvent::MessageStart { message: Message::User(u) } = event {
                let mut turns = tm_clone.lock().unwrap();
                if let Some(current_turn) = turns.last_mut() {
                    current_turn.push(u.content.text().to_string());
                }
            }
        });

        let prompt = Message::User(UserMessage {
            content: UserContent::Text("initial prompt".to_string()),
            timestamp: now_ms(),
        });

        let _ = run_agent_loop(vec![prompt], vec![], config.clone(), Some(on_event)).await;

        let turns = turn_messages.lock().unwrap().clone();
        // Turn 1 had initial prompt.
        // Turn 2 must have steer 1 only.
        // Turn 3 must have steer 2 only.
        assert!(turns.len() >= 3, "Expected at least 3 turns, got {}", turns.len());
        assert_eq!(turns[1], vec!["steer 1".to_string()], "Turn 2 should only have steer 1");
        assert_eq!(turns[2], vec!["steer 2".to_string()], "Turn 3 should only have steer 2");
    }

    struct SilentProvider;

    #[async_trait::async_trait]
    impl LlmProvider for SilentProvider {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            _options: &StreamOptions,
            _cancel: CancellationToken,
        ) -> mehh_ai::AssistantEventStream {
            let (sender, stream) = mehh_ai::create_event_stream();
            sender.push(AssistantMessageEvent::Start {
                partial: AssistantMessage::empty_for_model(model),
            });
            drop(sender);
            stream
        }
    }

    #[tokio::test]
    async fn stream_closed_without_final_message_uses_prepared_model() {
        let mut config = AgentLoopConfig::new(faux_model(), Arc::new(SilentProvider));
        config.prepare_request = Some(Arc::new(|req, _cancel| {
            let mut routed = req.model.clone();
            routed.id = "routed-model".to_string();
            routed.api = "routed-api".to_string();
            routed.provider = "routed-provider".to_string();
            Box::pin(async move {
                Some(crate::types::AgentRequestUpdate {
                    model: Some(routed),
                    ..Default::default()
                })
            })
        }));

        let out = run_agent_loop(vec![], vec![], config, None).await;
        let assistant = out.iter().find_map(|msg| match msg {
            Message::Assistant(a) => Some(a),
            _ => None,
        });
        let assistant = assistant.expect("fallback assistant message");
        assert_eq!(assistant.model, "routed-model");
        assert_eq!(assistant.api, "routed-api");
        assert_eq!(assistant.provider, "routed-provider");
    }

    #[tokio::test]
    async fn prepare_request_hook_tools_are_used_by_stream() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("ok"));
        let mut config = AgentLoopConfig::new(faux_model(), provider.clone());
        config.tools = vec![Arc::new(DummyTool {
            tool_name: "base_tool".to_string(),
            mode: ToolExecutionMode::Parallel,
            on_exec: None,
        })];
        let replacement = Arc::new(DummyTool {
            tool_name: "hook_tool".to_string(),
            mode: ToolExecutionMode::Parallel,
            on_exec: None,
        });
        let hook_tools = replacement.clone();
        config.prepare_request = Some(Arc::new(move |req, _cancel| {
            let tools: Vec<Arc<dyn AgentTool>> = vec![hook_tools.clone()];
            Box::pin(async move {
                Some(crate::types::AgentRequestUpdate {
                    context: Some(crate::types::AgentContext {
                        messages: req.context.messages,
                        tools: Some(tools),
                    }),
                    ..Default::default()
                })
            })
        }));

        let _ = run_agent_loop(vec![], vec![], config, None).await;
        let requests = provider.recorded_requests();
        let tools = requests[0]
            .context
            .tools
            .as_ref()
            .expect("stream received tool definitions");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "hook_tool");
    }

    #[tokio::test]
    async fn cancelled_tool_does_not_invoke_before_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let calls = hook_calls.clone();
        let mut config = AgentLoopConfig::new(faux_model(), Arc::new(FauxProvider::new()));
        config.tools = vec![Arc::new(DummyTool {
            tool_name: "some_tool".to_string(),
            mode: ToolExecutionMode::Parallel,
            on_exec: None,
        })];
        config.before_tool_call = Some(Arc::new(move |_ctx, _cancel| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { crate::types::BeforeToolCallResult::default() })
        }));
        let cancel = CancellationToken::new();
        cancel.cancel();
        config.cancel = cancel;

        let assistant = faux_assistant_message("unused");
        let tool_call = faux_tool_call("c-cancel", "some_tool", serde_json::json!({}));
        let outcome = execute_single_tool(
            &assistant,
            &tool_call,
            &config.tools,
            &[],
            &config,
            &None,
        )
        .await;

        assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
        assert!(outcome.tool_result_message.is_error);
    }

    #[tokio::test]
    async fn prepare_request_can_clear_thinking_level() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("ok"));
        let mut config = AgentLoopConfig::new(faux_model(), provider.clone());
        config.stream_options.reasoning = Some(mehh_ai::ThinkingLevel::High);
        config.prepare_request = Some(Arc::new(|_req, _cancel| {
            Box::pin(async {
                Some(crate::types::AgentRequestUpdate {
                    thinking_level: Some(None),
                    ..Default::default()
                })
            })
        }));

        let _ = run_agent_loop(vec![], vec![], config, None).await;
        assert!(provider.recorded_requests()[0]
            .options
            .reasoning
            .is_none());
    }
}
`````

## crates/mehh-agent/src/types.rs

`````rust
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use mehh_ai::{
    serialize_assistant_with_role, AssistantMessage, AssistantMessageEvent, Message, TextContent,
    ToolCall, ToolDefinition, ToolResultContentBlock, ToolResultMessage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    #[serde(alias = "one_at_a_time", alias = "OneAtATime")]
    OneAtATime,
    #[serde(alias = "All")]
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentToolResult {
    pub content: Vec<ToolResultContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "structuredContent", alias = "structured_content")]
    pub structured_content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<mehh_ai::Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "isError", alias = "is_error")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: s.into(),
                text_signature: None,
            })],
            details: None,
            structured_content: None,
            usage: None,
            is_error: None,
            terminate: None,
        }
    }
}

pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }
    fn is_read_only(&self) -> Option<bool> {
        None
    }
    fn is_safe_for_auto_approve(&self, _args: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
        self.is_read_only()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String>;
}

pub fn tool_to_definition(tool: &dyn AgentTool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters(),
    }
}

#[derive(Clone)]
pub struct AgentContext {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<Arc<dyn AgentTool>>>,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("messages", &self.messages)
            .field("tools_count", &self.tools.as_ref().map(|t| t.len()))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub context: AgentContext,
}

#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ToolResultContentBlock>>,
    pub details: Option<serde_json::Value>,
    pub structured_content: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<mehh_ai::Usage>,
    pub terminate: Option<bool>,
}

pub type BeforeToolCallHook = Arc<
    dyn Fn(BeforeToolCallContext, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, BeforeToolCallResult>
        + Send
        + Sync,
>;
pub type AfterToolCallHook = Arc<
    dyn Fn(AfterToolCallContext, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, AfterToolCallResult>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAction {
    Continue,
    End,
}

#[derive(Debug, Clone)]
pub struct FinishTurnDecision {
    pub action: TurnAction,
}

#[derive(Debug, Clone)]
pub struct AgentTurnContext {
    pub turn_index: usize,
    pub messages: Vec<Message>,
    pub assistant_message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
}

pub type FinishTurnHook = Arc<
    dyn Fn(AgentTurnContext, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, Option<FinishTurnDecision>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub struct PrepareRequestContext {
    pub context: AgentContext,
    pub model: mehh_ai::Model,
    pub thinking_level: Option<mehh_ai::ThinkingLevel>,
}

#[derive(Clone, Default)]
pub struct AgentRequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<mehh_ai::Model>,
    pub provider: Option<Arc<dyn mehh_ai::LlmProvider>>,
    pub api_key: Option<Option<String>>,
    pub thinking_level: Option<Option<mehh_ai::ThinkingLevel>>,
}

impl std::fmt::Debug for AgentRequestUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRequestUpdate")
            .field("context", &self.context)
            .field("model", &self.model)
            .field("provider", &self.provider.as_ref().map(|_| "<LlmProvider>"))
            .field("api_key", &self.api_key)
            .field("thinking_level", &self.thinking_level)
            .finish()
    }
}

pub type PrepareRequestHook = Arc<
    dyn Fn(PrepareRequestContext, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, Option<AgentRequestUpdate>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub messages: Option<Vec<Message>>,
    pub model: Option<mehh_ai::Model>,
    pub provider: Option<Arc<dyn mehh_ai::LlmProvider>>,
    pub api_key: Option<Option<String>>,
    pub thinking_level: Option<Option<mehh_ai::ThinkingLevel>>,
}

impl std::fmt::Debug for AgentLoopTurnUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopTurnUpdate")
            .field("context", &self.context)
            .field("messages", &self.messages)
            .field("model", &self.model)
            .field("provider", &self.provider.as_ref().map(|_| "<LlmProvider>"))
            .field("api_key", &self.api_key)
            .field("thinking_level", &self.thinking_level)
            .finish()
    }
}

pub type PrepareNextTurnHook = Arc<
    dyn Fn(AgentTurnContext, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

pub type TransformContextHook = Arc<
    dyn Fn(Vec<Message>, tokio_util::sync::CancellationToken) -> futures::future::BoxFuture<'static, Vec<Message>>
        + Send
        + Sync,
>;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    #[serde(rename_all = "camelCase")]
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    #[serde(rename_all = "camelCase")]
    TurnEnd {
        #[serde(serialize_with = "serialize_assistant_with_role")]
        message: AssistantMessage,
        #[serde(alias = "tool_results")]
        tool_results: Vec<ToolResultMessage>,
    },
    #[serde(rename_all = "camelCase")]
    MessageStart {
        message: Message,
    },
    #[serde(rename_all = "camelCase")]
    MessageUpdate {
        #[serde(serialize_with = "serialize_assistant_with_role")]
        message: AssistantMessage,
        #[serde(alias = "assistant_message_event")]
        assistant_message_event: AssistantMessageEvent,
    },
    #[serde(rename_all = "camelCase")]
    MessageEnd {
        message: Message,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionStart {
        #[serde(alias = "tool_call_id")]
        tool_call_id: String,
        #[serde(alias = "tool_name")]
        tool_name: String,
        args: serde_json::Map<String, serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionUpdate {
        #[serde(alias = "tool_call_id")]
        tool_call_id: String,
        #[serde(alias = "tool_name")]
        tool_name: String,
        args: serde_json::Map<String, serde_json::Value>,
        #[serde(alias = "partial_result")]
        partial_result: AgentToolResult,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd {
        #[serde(alias = "tool_call_id")]
        tool_call_id: String,
        #[serde(alias = "tool_name")]
        tool_name: String,
        result: AgentToolResult,
        #[serde(alias = "is_error")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "durationMs", alias = "duration_ms")]
        duration_ms: Option<u64>,
    },
}

#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: mehh_ai::Model,
    pub thinking_level: Option<mehh_ai::ThinkingLevel>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<Message>,
    pub is_streaming: bool,
    pub streaming_message: Option<Message>,
    pub pending_tool_calls: std::collections::HashSet<String>,
    pub error_message: Option<String>,
}

impl std::fmt::Debug for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentState")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("tools_count", &self.tools.len())
            .field("messages", &self.messages)
            .field("is_streaming", &self.is_streaming)
            .field("streaming_message", &self.streaming_message)
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{Model, ModelCost};

    fn dummy_assistant_message() -> AssistantMessage {
        let model = Model {
            id: "dummy".to_string(),
            name: "Dummy".to_string(),
            api: "test".to_string(),
            provider: "test".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            ..Default::default()
        };
        AssistantMessage::empty_for_model(&model)
    }

    #[test]
    fn test_agent_event_serde_camel_case_fields() {
        let ev = AgentEvent::ToolExecutionStart {
            tool_call_id: "call_123".to_string(),
            tool_name: "test_tool".to_string(),
            args: serde_json::Map::new(),
        };
        let val = serde_json::to_value(&ev).unwrap();
        assert_eq!(val["type"], "tool_execution_start");
        assert_eq!(val["toolCallId"], "call_123");
        assert_eq!(val["toolName"], "test_tool");
        assert!(val.get("tool_call_id").is_none());

        // Deserialize from camelCase
        let deser: AgentEvent = serde_json::from_value(val).unwrap();
        match deser {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => {
                assert_eq!(tool_call_id, "call_123");
                assert_eq!(tool_name, "test_tool");
            }
            _ => panic!("wrong variant"),
        }

        // Deserialize from snake_case alias
        let snake_json = serde_json::json!({
            "type": "tool_execution_start",
            "tool_call_id": "call_456",
            "tool_name": "bash",
            "args": {}
        });
        let deser_snake: AgentEvent = serde_json::from_value(snake_json).unwrap();
        match deser_snake {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => {
                assert_eq!(tool_call_id, "call_456");
                assert_eq!(tool_name, "bash");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_agent_event_turn_end_message_role_assistant() {
        let msg = dummy_assistant_message();
        let ev = AgentEvent::TurnEnd {
            message: msg,
            tool_results: Vec::new(),
        };
        let val = serde_json::to_value(&ev).unwrap();
        assert_eq!(val["type"], "turn_end");
        assert_eq!(val["message"]["role"], "assistant");
        assert!(val.get("toolResults").is_some());
    }

    #[test]
    fn test_queue_mode_serde() {
        assert_eq!(serde_json::to_string(&QueueMode::OneAtATime).unwrap(), "\"one-at-a-time\"");
        assert_eq!(serde_json::to_string(&QueueMode::All).unwrap(), "\"all\"");

        let d1: QueueMode = serde_json::from_str("\"one-at-a-time\"").unwrap();
        assert_eq!(d1, QueueMode::OneAtATime);

        let d2: QueueMode = serde_json::from_str("\"one_at_a_time\"").unwrap();
        assert_eq!(d2, QueueMode::OneAtATime);

        let d3: QueueMode = serde_json::from_str("\"OneAtATime\"").unwrap();
        assert_eq!(d3, QueueMode::OneAtATime);

        let d4: QueueMode = serde_json::from_str("\"all\"").unwrap();
        assert_eq!(d4, QueueMode::All);

        let d5: QueueMode = serde_json::from_str("\"All\"").unwrap();
        assert_eq!(d5, QueueMode::All);
    }
}
`````

## crates/mehh-coding-agent/src/agent_session.rs

`````rust
//! Orchestrates an agent session with tool executions, model streaming, and history tracking.

/// Marker used by `TodoState::format_moim_block` and by the idempotent injector below.
const MOIM_START: &str = "<working_checklist>";
const MOIM_END: &str = "</working_checklist>";

/// Replaces a previously injected MOIM block in place, or appends it once.
fn replace_moim_text(text: &mut String, block: &str) {
    if let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, block);
            return;
        }
    }
    text.push_str("\n\n");
    text.push_str(block);
}

/// Removes any previously injected MOIM block. Returns whether text changed.
fn strip_moim_text(text: &mut String) -> bool {
    if let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, "");
            return true;
        }
    }
    false
}

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use mehh_agent::{run_agent_loop, AgentEvent, AgentLoopConfig, AgentTool, DEFAULT_MAX_AGENT_TURNS};
use mehh_ai::{
    default_http_client, is_context_overflow, now_ms, AnthropicProvider,
    AzureOpenAiResponsesProvider, BedrockProvider, FauxProvider, GoogleProvider, LlmProvider,
    Message, MistralProvider, Model, OpenAiCompletionsProvider, OpenAiResponsesProvider,
    ThinkingLevel, UserContent, UserMessage,
};

use crate::auth::resolve_api_key_async;
use crate::prompt_templates::PromptTemplate;
use crate::session::SessionManager;
use crate::settings::Settings;
use crate::skills::Skill;
use crate::subagent::SubagentTool;
use crate::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
use crate::system_prompt::build_system_prompt_with_skills;
use crate::tool_search::{DeferredToolCatalog, ToolSearchTool};

/// MCP tool count above which `mcp__*` tools are registered behind `tool_search`.
pub const DEFERRED_MCP_TOOL_THRESHOLD: usize = 20;

/// High-level agent session that pairs conversation history, model configuration,
/// system prompt assembly, tools, skills, and prompt templates.
pub struct AgentSession {
    pub cwd: PathBuf,
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub session: SessionManager,
    pub settings: Settings,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub system_prompt: String,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub api_key: Option<String>,
    has_explicit_api_key: bool,
    pub thinking_level: Option<ThinkingLevel>,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub no_context_files: bool,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub active_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub active_bash_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub scoped_models: Vec<Model>,
    pub trusted: bool,
    dynamic_tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    deferred_catalog: Option<Arc<DeferredToolCatalog>>,
    codemode_tool: Option<Arc<mehh_codemode::CodemodeTool>>,
    pub inspection_manager: Arc<crate::tool_inspection::ToolInspectionManager>,
    pub approval_handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
    /// Shared with the codemode interceptor so `set_approval_handler` takes
    /// effect without recreating the sandbox tool.
    approval_handler_slot: Arc<RwLock<Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>>>,
    pub todo_state: Arc<tokio::sync::RwLock<crate::todo::TodoState>>,
    pub lead_worker: Arc<Mutex<crate::lead_worker::LeadWorkerManager>>,
    pub subagent_control: Arc<SubagentControl>,
}

/// Resolves the credential that a cross-provider subagent worker must use.
/// This must never fall back to the Lead key: at turn zero `active_api_key()`
/// is the Lead key even when the worker model uses a different provider.
fn subagent_api_key(
    manager: &crate::lead_worker::LeadWorkerManager,
    api_key: &Option<String>,
    worker_provider: &str,
    session_provider: &str,
) -> Option<String> {
    if worker_provider.eq_ignore_ascii_case(session_provider) {
        api_key.clone()
    } else {
        manager.worker_api_key.clone()
    }
}

impl AgentSession {
    /// Creates a new agent session assuming project trust (`trusted = true` for existing callers/tests).
    /// CLI and untrusted entrypoints should call [`AgentSession::new_with_trust`].
    pub fn new(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
    ) -> Self {
        Self::new_with_trust(cwd, model, provider, session, settings, true)
    }

    /// Creates a new agent session loading repo-local skills and prompt templates only when `trusted` is true.
    pub fn new_with_trust(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
        trusted: bool,
    ) -> Self {
        let skills = crate::skills::load_skills_with_trust(&cwd, trusted);
        let prompt_templates =
            crate::prompt_templates::load_prompt_templates_with_trust(&cwd, trusted);
        let mut tools = mehh_tools::create_coding_tools(cwd.clone());

        let thinking_level = settings.default_thinking_level;
        let default_mode = if model.provider == "faux" || model.api == "faux" {
            crate::tool_inspection::PermissionMode::Auto
        } else {
            crate::tool_inspection::PermissionMode::SmartApprove
        };
        let permission_mode = settings.permission_mode.unwrap_or(default_mode);
        let perm_store =
            crate::permissions::PermissionStore::load(&cwd, trusted).unwrap_or_default();
        let inspection_manager = Arc::new(
            crate::tool_inspection::ToolInspectionManager::new_with_store(
                permission_mode,
                cwd.clone(),
                perm_store,
            ),
        );

        let (worker_model, worker_provider) = if let Some(ref wm) = settings.worker_model {
            if let Ok(reg) = crate::models_config::ModelRegistry::load_default() {
                if let Some(m) = reg.resolve(wm, None) {
                    let p = Self::default_provider_for_model(&m);
                    (Some(m), Some(p))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let worker_api_key = if let Some(ref wm) = worker_model {
            if wm.provider == "faux" || wm.api == "faux" {
                None
            } else {
                crate::auth::resolve_api_key(&wm.provider, None, None)
            }
        } else {
            None
        };

        let mut lw_mgr = crate::lead_worker::LeadWorkerManager::new(
            model.clone(),
            provider.clone(),
            worker_model,
            worker_provider,
            settings.lead_turns.unwrap_or(1),
        );
        lw_mgr.set_worker_api_key(worker_api_key.clone());
        let lead_worker = Arc::new(Mutex::new(lw_mgr));

        let subagent_model = lead_worker
            .lock()
            .unwrap()
            .worker_model()
            .cloned()
            .unwrap_or_else(|| model.clone());
        let subagent_provider = lead_worker
            .lock()
            .unwrap()
            .worker_provider()
            .cloned()
            .unwrap_or_else(|| provider.clone());
        let subagent_key = if subagent_model.provider == model.provider {
            None
        } else {
            worker_api_key
        };

        let subagent_control = Arc::new(SubagentControl::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS));
        let subagent = Arc::new(
            SubagentTool::new(
                cwd.clone(),
                subagent_model,
                subagent_provider,
                subagent_key,
                tools.clone(),
                0,
            )
            .with_inspection_manager(inspection_manager.clone())
            .with_control(subagent_control.clone())
            .with_lead_worker(lead_worker.clone()),
        );
        tools.push(subagent);
        let todo_state = Arc::new(tokio::sync::RwLock::new(crate::todo::TodoState::default()));
        let todo_tool = Arc::new(crate::todo::TodoTool::new(todo_state.clone()));
        tools.push(todo_tool);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let system_prompt = build_system_prompt_with_skills(&cwd, None, None, &tool_names, &skills);

        Self {
            cwd,
            model,
            provider,
            session,
            settings,
            tools,
            system_prompt,
            custom_system_prompt: None,
            append_system_prompt: None,
            api_key: None,
            has_explicit_api_key: false,
            thinking_level,
            skills,
            prompt_templates,
            no_context_files: false,
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            active_cancel: Arc::new(Mutex::new(None)),
            active_bash_cancel: Arc::new(Mutex::new(None)),
            scoped_models: Vec::new(),
            trusted,
            dynamic_tools: Arc::new(RwLock::new(Vec::new())),
            deferred_catalog: None,
            codemode_tool: None,
            inspection_manager,
            approval_handler: None,
            approval_handler_slot: Arc::new(RwLock::new(None)),
            todo_state,
            lead_worker,
            subagent_control,
        }
    }

    /// Queues a steering user message to be processed at the next turn or tool call boundary.
    pub fn steer(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        self.steering_queue.lock().unwrap().push_back(msg);
    }

    /// Queues a follow-up user message to be processed once the current turn completes.
    pub fn follow_up(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        self.follow_up_queue.lock().unwrap().push_back(msg);
    }

    /// Returns the number of pending steering messages.
    pub fn pending_steering_count(&self) -> usize {
        self.steering_queue.lock().unwrap().len()
    }

    /// Returns the number of pending follow-up messages.
    pub fn pending_follow_up_count(&self) -> usize {
        self.follow_up_queue.lock().unwrap().len()
    }

    /// Clears both steering and follow-up queues.
    pub fn clear_queues(&self) {
        self.steering_queue.lock().unwrap().clear();
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// Aborts any currently active running prompt loop. Returns true if a cancel was triggered.
    pub fn abort(&self) -> bool {
        let cancel = self.active_cancel.lock().unwrap().take();
        if let Some(c) = cancel {
            c.cancel();
            true
        } else {
            false
        }
    }

    /// Returns whether a prompt execution is currently in progress and not cancelled.
    pub fn is_active(&self) -> bool {
        self.active_cancel
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| !c.is_cancelled())
            .unwrap_or(false)
    }

    /// Sets the list of scoped models available for cycling.
    pub fn set_scoped_models(&mut self, models: Vec<Model>) {
        self.scoped_models = models;
    }

    /// Returns a reference to the scoped models list.
    pub fn scoped_models(&self) -> &[Model] {
        &self.scoped_models
    }

    /// Cycles to the next model in `scoped_models` (or registry if scoped_models is empty).
    pub fn cycle_model(&mut self) -> Result<Option<Model>> {
        let models_list: Vec<Model> = if !self.scoped_models.is_empty() {
            self.scoped_models.clone()
        } else if let Ok(reg) = crate::models_config::ModelRegistry::load_default() {
            reg.models().to_vec()
        } else {
            Vec::new()
        };

        if models_list.is_empty() {
            return Ok(None);
        }

        let curr_idx = models_list.iter().position(|m| {
            m.id.eq_ignore_ascii_case(&self.model.id)
                && m.provider.eq_ignore_ascii_case(&self.model.provider)
        });
        let next_idx = match curr_idx {
            Some(i) => (i + 1) % models_list.len(),
            None => 0,
        };
        let next_model = models_list[next_idx].clone();
        let prov = Self::default_provider_for_model(&next_model);
        self.set_model(next_model.clone(), prov);
        Ok(Some(next_model))
    }

    /// Aborts any currently running bash tool command.
    pub fn abort_bash(&self) {
        if let Some(tok) = self.active_bash_cancel.lock().unwrap().take() {
            tok.cancel();
        }
    }

    /// Returns the shared steering queue.
    pub fn steering_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.steering_queue.clone()
    }

    /// Returns the shared follow-up queue.
    pub fn follow_up_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.follow_up_queue.clone()
    }

    /// Returns the shared subagent control (registry + global budget).
    pub fn subagent_control(&self) -> Arc<SubagentControl> {
        self.subagent_control.clone()
    }

    /// Returns the slot holding the active cancellation token.
    pub fn active_cancel_slot(&self) -> Arc<Mutex<Option<CancellationToken>>> {
        self.active_cancel.clone()
    }

    /// Sets or clears the interactive approval handler for tool inspection.
    pub fn set_approval_handler(
        &mut self,
        handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
    ) {
        self.approval_handler = handler;
        *self.approval_handler_slot.write().unwrap() = self.approval_handler.clone();
        self.refresh_subagent_tool();
    }

    /// Overrides active tools for the session.
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        self.tools = tools;
        self.refresh_subagent_tool();
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
        self.rebuild_system_prompt();
    }

    /// Returns a slice of the active tools.
    pub fn tools(&self) -> &[Arc<dyn AgentTool>] {
        &self.tools
    }

    /// Stores custom/append system prompt overrides and rebuilds the active system prompt.
    pub fn set_custom_system_prompt(&mut self, custom: Option<String>, append: Option<String>) {
        self.custom_system_prompt = custom;
        self.append_system_prompt = append;
        self.rebuild_system_prompt();
    }

    /// Rebuilds the structured system prompt from current tools, skills, and preserved prompt overrides.
    pub fn rebuild_system_prompt(&mut self) {
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
        self.system_prompt = crate::system_prompt::build_system_prompt_with_skills_and_context(
            &self.cwd,
            self.custom_system_prompt.as_deref(),
            self.append_system_prompt.as_deref(),
            &tool_names,
            &self.skills,
            !self.no_context_files,
        );
    }

    /// Skips loading AGENTS.md / CLAUDE.md into the system prompt.
    pub fn set_no_context_files(&mut self, value: bool) {
        self.no_context_files = value;
        self.rebuild_system_prompt();
    }

    /// Reloads settings, skills, and prompt templates from disk, then rebuilds system prompt.
    pub fn reload(&mut self) {
        if let Ok(settings) =
            crate::settings::SettingsManager::try_load_with_trust(&self.cwd, self.trusted)
        {
            self.settings = settings;
        }
        self.skills = crate::skills::load_skills_with_trust(&self.cwd, self.trusted);
        self.prompt_templates =
            crate::prompt_templates::load_prompt_templates_with_trust(&self.cwd, self.trusted);
        self.rebuild_system_prompt();
    }

    /// Injects MCP tools (deferring large MCP toolsets behind `tool_search` when codemode is off)
    /// and optionally wraps all tools in CodemodeTool, rebuilding the system prompt.
    pub fn enable_mcp_and_codemode(
        &mut self,
        mcp_tools: Vec<Arc<dyn AgentTool>>,
        enable_codemode: bool,
    ) {
        let mcp_server_tool_count = mcp_tools
            .iter()
            .filter(|t| t.name().starts_with("mcp__"))
            .count();
        if !enable_codemode && mcp_server_tool_count > DEFERRED_MCP_TOOL_THRESHOLD {
            self.codemode_tool = None;
            let mut eager = Vec::new();
            let mut deferred = Vec::new();
            for tool in mcp_tools {
                if tool.name().starts_with("mcp__") {
                    deferred.push(tool);
                } else {
                    eager.push(tool);
                }
            }
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
            self.tools.extend(mcp_tools);
            if enable_codemode {
                let codemode_tool = self.create_codemode_tool(self.tools.clone());
                self.codemode_tool = Some(codemode_tool.clone());
                self.tools.push(codemode_tool);
            } else {
                self.codemode_tool = None;
            }
            self.rebuild_system_prompt();
            self.refresh_subagent_tool();
        }
    }

    fn create_codemode_tool(
        &self,
        tools: Vec<Arc<dyn AgentTool>>,
    ) -> Arc<mehh_codemode::CodemodeTool> {
        let inspection_mgr = self.inspection_manager.clone();
        let approval_slot = self.approval_handler_slot.clone();
        let session_cwd = self.cwd.clone();

        let mut codemode = mehh_codemode::CodemodeTool::new(tools);
        // Read the live tool list on every call: `replace_tools` must be able to
        // refresh `is_read_only` without rebuilding the interceptor.
        let live_tools = codemode.tools_handle();
        let interceptor: mehh_codemode::ToolCallInterceptor = Arc::new(move |name, args| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_slot = approval_slot.clone();
            let session_cwd = session_cwd.clone();
            let live_tools = live_tools.clone();
            let name_str = name.to_string();
            let args_map = args.clone();
            Box::pin(async move {
                let is_read_only = live_tools
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|t| t.name() == name_str)
                    .and_then(|t| t.is_read_only());
                // Approval may be installed after the tool was created (TUI
                // calls set_approval_handler on session startup).
                let approval_h = approval_slot
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id: format!("codemode/{name_str}"),
                    tool_name: name_str,
                    args: args_map,
                    is_read_only,
                    cwd: session_cwd,
                    origin: Some("codemode".to_string()),
                };
                let res = inspection_mgr
                    .inspect_and_authorize(&inspect_ctx, approval_h.as_deref())
                    .await;
                if res.block {
                    Err(res
                        .reason
                        .unwrap_or_else(|| "Tool call blocked by inspection policy".to_string()))
                } else {
                    Ok(())
                }
            })
        });

        codemode.set_interceptor(Some(interceptor));
        Arc::new(codemode)
    }

    /// Synchronizes updated MCP tools into the session, updating deferred catalog and codemode bindings in-place.
    pub fn sync_mcp_tools(&mut self, mcp_tools: Vec<Arc<dyn AgentTool>>, enable_codemode: bool) {
        self.tools.retain(|t| {
            !t.name().starts_with("mcp__")
                && t.name() != "list_mcp_resources"
                && t.name() != "list_mcp_resource_templates"
                && t.name() != "read_mcp_resource"
                && t.name() != "tool_search"
                && t.name() != "codemode"
        });

        let mcp_server_tool_count = mcp_tools
            .iter()
            .filter(|t| t.name().starts_with("mcp__"))
            .count();

        if !enable_codemode && mcp_server_tool_count > DEFERRED_MCP_TOOL_THRESHOLD {
            self.codemode_tool = None;
            let mut eager = Vec::new();
            let mut deferred = Vec::new();
            for tool in mcp_tools {
                if tool.name().starts_with("mcp__") {
                    deferred.push(tool);
                } else {
                    eager.push(tool);
                }
            }
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
            if self.deferred_catalog.is_some() {
                self.clear_deferred_mcp_tools();
            }
            self.tools.extend(mcp_tools);
            if enable_codemode {
                if let Some(ref codemode) = self.codemode_tool {
                    codemode.replace_tools(self.tools.clone());
                    self.tools.push(codemode.clone());
                } else {
                    let codemode_tool = self.create_codemode_tool(self.tools.clone());
                    self.codemode_tool = Some(codemode_tool.clone());
                    self.tools.push(codemode_tool);
                }
            } else {
                self.codemode_tool = None;
            }
            self.rebuild_system_prompt();
            self.refresh_subagent_tool();
        }
    }

    /// Returns the active CodemodeTool handle, if enabled.
    pub fn codemode_tool(&self) -> Option<Arc<mehh_codemode::CodemodeTool>> {
        self.codemode_tool.clone()
    }

    /// Returns the deferred tool catalog handle, if active.
    pub fn deferred_catalog(&self) -> Option<Arc<DeferredToolCatalog>> {
        self.deferred_catalog.clone()
    }

    /// Clears deferred `tool_search` tools prior to resynchronizing MCP tools.
    pub fn clear_deferred_mcp_tools(&mut self) {
        self.dynamic_tools.write().unwrap().clear();
        self.deferred_catalog = None;
        self.tools.retain(|t| t.name() != "tool_search");
    }

    /// Registers deferred tools behind `tool_search` and links them into `dynamic_tools`.
    pub fn enable_deferred_mcp_tools(&mut self, deferred: Vec<Arc<dyn AgentTool>>) {
        if deferred.is_empty() {
            return;
        }
        if let Some(ref catalog) = self.deferred_catalog {
            catalog.replace_deferred(deferred);
            if !self.tools.iter().any(|t| t.name() == "tool_search") {
                self.tools
                    .push(Arc::new(ToolSearchTool::new(catalog.clone())));
            }
        } else {
            let catalog = Arc::new(DeferredToolCatalog::new(deferred));
            self.dynamic_tools = catalog.dynamic_tools_handle();
            self.deferred_catalog = Some(Arc::clone(&catalog));
            self.tools.retain(|t| t.name() != "tool_search");
            self.tools.push(Arc::new(ToolSearchTool::new(catalog)));
        }
        self.rebuild_system_prompt();
    }

    fn refresh_subagent_tool(&mut self) {
        let base_tools: Vec<Arc<dyn AgentTool>> = self
            .tools
            .iter()
            .filter(|t| t.name() != "subagent" && t.name() != "codemode")
            .cloned()
            .collect();
        let (subagent_model, subagent_provider, subagent_api_key) = {
            let lw = self.lead_worker.lock().unwrap();
            let wm = lw
                .worker_model()
                .cloned()
                .unwrap_or_else(|| self.model.clone());
            let wp = lw
                .worker_provider()
                .cloned()
                .unwrap_or_else(|| self.provider.clone());
            let ak = subagent_api_key(
                &lw,
                &self.api_key,
                wm.provider.as_str(),
                self.model.provider.as_str(),
            );
            (wm, wp, ak)
        };
        let updated = Arc::new(
            SubagentTool::new(
                self.cwd.clone(),
                subagent_model,
                subagent_provider,
                subagent_api_key,
                base_tools,
                0,
            )
            .with_inspection_manager(self.inspection_manager.clone())
            .with_approval_handler(self.approval_handler.clone())
            .with_control(self.subagent_control.clone())
            .with_lead_worker(self.lead_worker.clone()),
        );
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == "subagent") {
            *slot = updated;
        }
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
    }

    /// Sets or updates the system prompt directly.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
    }

    /// Sets or updates the provider API key (clearing any explicit CLI override marker).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = false;
        self.api_key = api_key.clone();
        self.lead_worker.lock().unwrap().set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Sets an explicit CLI `--api-key` override that must not be clobbered on same-provider switches.
    pub fn set_explicit_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
        self.api_key = api_key.clone();
        self.lead_worker.lock().unwrap().set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Returns whether the active key was explicitly pinned by CLI `--api-key`.
    pub fn has_explicit_api_key(&self) -> bool {
        self.has_explicit_api_key
    }

    /// Sets or updates reasoning/thinking level.
    pub fn set_thinking_level(&mut self, level: Option<ThinkingLevel>) {
        self.thinking_level = level;
    }

    /// Updates both model metadata and its backing provider implementation.
    /// Clears `api_key` whenever switching to a different provider so keys never leak across providers.
    pub fn set_model(&mut self, model: Model, provider: Arc<dyn LlmProvider>) {
        if !self.model.provider.eq_ignore_ascii_case(&model.provider) {
            self.api_key = None;
            self.has_explicit_api_key = false;
            self.lead_worker.lock().unwrap().set_lead_api_key(None);
        }
        self.model = model;
        self.provider = provider;
        {
            let mut mgr = self.lead_worker.lock().unwrap();
            mgr.lead_model = self.model.clone();
            mgr.lead_provider = self.provider.clone();
        }
        self.refresh_subagent_tool();
    }

    /// Returns a reference to the active session manager.
    pub fn session(&self) -> &SessionManager {
        &self.session
    }

    /// Returns a mutable reference to the active session manager.
    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    /// Returns the currently active model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Returns the active thinking/reasoning level.
    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.thinking_level
    }

    /// Returns the slice of loaded skills.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Returns the slice of loaded prompt templates.
    pub fn prompt_templates(&self) -> &[PromptTemplate] {
        &self.prompt_templates
    }

    /// Resolves the default provider implementation for the given model metadata.
    pub fn default_provider_for_model(model: &Model) -> Arc<dyn LlmProvider> {
        if model.provider == "faux" || model.api == "faux" {
            Arc::new(FauxProvider::default())
        } else if model.api == "mistral-conversations" || model.provider == "mistral" {
            Arc::new(MistralProvider::new())
        } else if model.api == "azure-openai-responses" || model.provider == "azure-openai" {
            Arc::new(AzureOpenAiResponsesProvider::new())
        } else if model.api == "bedrock-converse-stream"
            || model.provider == "amazon-bedrock"
            || model.provider == "bedrock"
        {
            Arc::new(BedrockProvider::new())
        } else if model.api == "google-vertex"
            || model.api == "google-generative-ai"
            || model.provider == "google"
            || model.provider == "gemini"
        {
            Arc::new(GoogleProvider::new())
        } else if model.api == "anthropic-messages" || model.provider == "anthropic" {
            Arc::new(AnthropicProvider::new(default_http_client()))
        } else if model.api == "openai-responses" {
            Arc::new(OpenAiResponsesProvider::new())
        } else {
            Arc::new(OpenAiCompletionsProvider::new(default_http_client()))
        }
    }

    /// Expands `/skill:` commands and prompt template commands in the user input.
    pub fn expand_input(&self, input: &str) -> String {
        if let Some(expanded) = crate::skills::expand_skill_command(input, &self.skills) {
            return expanded;
        }
        if let Some(expanded) =
            crate::prompt_templates::expand_prompt_template(input, &self.prompt_templates)
        {
            return expanded;
        }
        input.to_string()
    }

    /// Prepares input, history, and agent loop configuration for a prompt turn with arbitrary user content.
    pub fn prepare_prompt_content(
        &self,
        content: UserContent,
    ) -> (Message, Vec<Message>, AgentLoopConfig) {
        // Routing decisions for the first turn must see a fresh prompt counter.
        self.lead_worker.lock().unwrap().reset_for_new_prompt();
        let expanded_content = match content {
            UserContent::Text(text) => UserContent::Text(self.expand_input(&text)),
            UserContent::Blocks(blocks) => {
                let expanded_blocks = blocks
                    .into_iter()
                    .map(|b| match b {
                        mehh_ai::UserContentBlock::Text(t) => {
                            mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                                text: self.expand_input(&t.text),
                                text_signature: t.text_signature,
                            })
                        }
                        image => image,
                    })
                    .collect();
                UserContent::Blocks(expanded_blocks)
            }
        };

        let user_msg = Message::User(UserMessage {
            content: expanded_content,
            timestamp: now_ms(),
        });

        let history = self.session.build_context_messages();

        let cancel = CancellationToken::new();
        *self.active_cancel.lock().unwrap() = Some(cancel.clone());

        let mut config = AgentLoopConfig::new(self.model.clone(), self.provider.clone());
        config.steering_queue = self.steering_queue.clone();
        config.follow_up_queue = self.follow_up_queue.clone();
        config.steering_mode = self.settings.steering_mode.unwrap_or_default();
        config.follow_up_mode = self.settings.follow_up_mode.unwrap_or_default();
        config.cancel = cancel;
        config.system_prompt = Some(self.system_prompt.clone());
        config.tools = self.tools.clone();
        config.dynamic_tools = Some(Arc::clone(&self.dynamic_tools));
        config.max_turns = Some(DEFAULT_MAX_AGENT_TURNS);
        config.stream_options.api_key = self.api_key.clone();
        {
            let mgr = self.lead_worker.lock().unwrap();
            if mgr.should_use_worker() {
                config.model = mgr.active_model();
                config.provider = mgr.active_provider();
                config.stream_options.api_key = mgr.worker_api_key.clone();
            }
        }
        config.stream_options.reasoning = self.thinking_level;
        if let Some(t) = self.settings.temperature {
            config.stream_options.temperature = Some(t);
        }
        if let Some(m) = self.settings.max_tokens {
            config.stream_options.max_tokens = Some(m);
        }

        let inspection_mgr = self.inspection_manager.clone();
        let approval_h = self.approval_handler.clone();
        let session_cwd = self.cwd.clone();

        config.before_tool_call = Some(Arc::new(move |before_ctx, _cancel| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_h = approval_h.clone();
            let session_cwd = session_cwd.clone();

            Box::pin(async move {
                let tool_name = before_ctx.tool_call.name.clone();
                let tool_call_id = before_ctx.tool_call.id.clone();
                let is_read_only = before_ctx
                    .context
                    .tools
                    .as_ref()
                    .and_then(|tools| tools.iter().find(|t| t.name() == tool_name))
                    .and_then(|t| t.is_read_only());

                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id,
                    tool_name,
                    args: before_ctx.args,
                    is_read_only,
                    cwd: session_cwd,
                    origin: None,
                };

                inspection_mgr
                    .inspect_and_authorize(&inspect_ctx, approval_h.as_deref())
                    .await
            })
        }));

        let lead_worker_outcomes = self.lead_worker.clone();
        config.after_tool_call = Some(Arc::new(move |after_ctx, _cancel| {
            lead_worker_outcomes
                .lock()
                .unwrap()
                .record_tool_outcome(after_ctx.is_error);
            Box::pin(async { mehh_agent::types::AfterToolCallResult::default() })
        }));

        let todo_state = self.todo_state.clone();
        let session_cwd_moim = self.cwd.clone();
        let session_trusted = self.trusted;

        config.prepare_request = Some(Arc::new(move |req_ctx, _cancel| {
            let todo_state = todo_state.clone();
            let session_cwd = session_cwd_moim.clone();

            Box::pin(async move {
                let block_opt = {
                    let read = todo_state.read().await;
                    read.format_moim_block(&session_cwd, session_trusted)
                };

                let mut ctx = req_ctx.context.clone();
                let mut changed = false;
                if let Some(ref moim_block) = block_opt {
                    if let Some(Message::User(ref mut u)) = ctx
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::User(_)))
                    {
                        match u.content {
                            UserContent::Text(ref mut t) => {
                                replace_moim_text(t, moim_block);
                                changed = true;
                            }
                            UserContent::Blocks(ref mut blocks) => {
                                if let Some(t) = blocks.iter_mut().find_map(|b| match b {
                                    mehh_ai::UserContentBlock::Text(t)
                                        if t.text.contains(MOIM_START) =>
                                    {
                                        Some(t)
                                    }
                                    _ => None,
                                }) {
                                    replace_moim_text(&mut t.text, moim_block);
                                } else {
                                    blocks.push(mehh_ai::UserContentBlock::Text(
                                        mehh_ai::TextContent {
                                            text: format!("\n\n{}", moim_block),
                                            text_signature: None,
                                        },
                                    ));
                                }
                                changed = true;
                            }
                        }
                    } else {
                        // Fallback: If no User message exists in history, insert at the beginning.
                        ctx.messages.insert(
                            0,
                            Message::User(UserMessage {
                                content: UserContent::Text(moim_block.clone()),
                                timestamp: now_ms(),
                            }),
                        );
                        changed = true;
                    }
                } else {
                    // Checklist cleared: remove the stale injected block as well.
                    for msg in ctx.messages.iter_mut() {
                        if let Message::User(u) = msg {
                            match u.content {
                                UserContent::Text(ref mut t) => {
                                    changed |= strip_moim_text(t);
                                }
                                UserContent::Blocks(ref mut blocks) => {
                                    let before = blocks.len();
                                    blocks.retain(|b| match b {
                                        mehh_ai::UserContentBlock::Text(t) => {
                                            !t.text.contains(MOIM_START)
                                        }
                                        _ => true,
                                    });
                                    changed |= blocks.len() != before;
                                }
                            }
                        }
                    }
                }

                if changed {
                    Some(mehh_agent::AgentRequestUpdate {
                        context: Some(ctx),
                        ..Default::default()
                    })
                } else {
                    None
                }
            })
        }));

        let lead_worker_turn = self.lead_worker.clone();
        config.prepare_next_turn = Some(Arc::new(move |turn_ctx, _cancel| {
            let lead_worker = lead_worker_turn.clone();
            Box::pin(async move {
                let mut mgr = lead_worker.lock().unwrap();
                let stop_reason = turn_ctx.assistant_message.stop_reason;
                let is_empty_assistant = turn_ctx.assistant_message.content.is_empty();
                let repetition_detected = turn_ctx.tool_results.iter().any(|tr| {
                    tr.is_error
                        && tr.content.iter().any(|b| match b {
                            mehh_ai::ToolResultContentBlock::Text(t) => {
                                t.text.contains("Repeated identical tool call")
                            }
                            _ => false,
                        })
                });
                let is_repetition_break = is_empty_assistant || repetition_detected;
                mgr.record_turn_outcome(stop_reason, is_repetition_break);

                if mgr.is_active() {
                    let next_model = mgr.active_model();
                    let next_provider = mgr.active_provider();

                    if next_model.id != turn_ctx.assistant_message.model
                        || next_model.provider != turn_ctx.assistant_message.provider
                    {
                        let next_api_key = mgr.active_api_key();
                        Some(mehh_agent::AgentLoopTurnUpdate {
                            model: Some(next_model),
                            provider: Some(next_provider),
                            api_key: Some(next_api_key),
                            ..Default::default()
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        }));

        (user_msg, history, config)
    }

    /// Prepares input, history, and agent loop configuration for a prompt turn.
    pub fn prepare_prompt(&self, text: &str) -> (Message, Vec<Message>, AgentLoopConfig) {
        self.prepare_prompt_content(UserContent::Text(text.to_string()))
    }

    /// Appends new messages from an agent turn to the session manager.
    pub fn finish_prompt(&mut self, new_messages: &[Message]) -> Result<()> {
        for msg in new_messages {
            self.session.append_message(msg.clone())?;
        }
        Ok(())
    }

    /// Resolves and refreshes credentials asynchronously for `provider` unless overridden by `--api-key`.
    pub(crate) async fn refresh_resolved_api_key(
        provider: &str,
        model_api: &str,
        has_explicit_api_key: bool,
        auth_file: Option<&std::path::Path>,
    ) -> Result<Option<String>> {
        if has_explicit_api_key || provider == "faux" || model_api == "faux" {
            return Ok(None);
        }
        if let Some(key) = resolve_api_key_async(provider, None, auth_file).await? {
            return Ok(Some(key));
        }
        match crate::models_config::ModelRegistry::load_default() {
            Ok(reg) => Ok(reg.provider_api_key(provider).map(str::to_string)),
            Err(err)
                if std::env::var_os("USERPROFILE").is_none()
                    && std::env::var_os("HOME").is_none() =>
            {
                let _ = err;
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Resolves and refreshes `api_key` asynchronously (refreshing expiring OAuth tokens across turns).
    pub async fn ensure_api_key(&mut self, auth_file: Option<&std::path::Path>) -> Result<()> {
        if let Some(key) = Self::refresh_resolved_api_key(
            &self.model.provider,
            &self.model.api,
            self.has_explicit_api_key,
            auth_file,
        )
        .await?
        {
            if self.api_key.as_deref() != Some(key.as_str()) {
                self.api_key = Some(key);
                self.lead_worker
                    .lock()
                    .unwrap()
                    .set_lead_api_key(self.api_key.clone());
                self.refresh_subagent_tool();
            }
        }
        Ok(())
    }

    /// Prepares proactive auto-compaction metadata synchronously when threshold is reached.
    pub fn prepare_proactive_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            auto_compact_enabled,
        );
        let current_tokens =
            crate::compaction::estimate_context_tokens(&self.session.build_context_messages())
                + self.system_prompt.chars().count().div_ceil(4)
                + crate::compaction::estimate_tool_definitions_tokens(&self.tools);
        if !crate::compaction::should_auto_compact(
            current_tokens,
            usize::try_from(self.model.context_window).unwrap_or(usize::MAX),
            &comp_settings,
        ) {
            return None;
        }
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();
        crate::compaction::prepare_compaction(&active_entries, comp_settings.keep_recent_tokens)
    }

    /// Prepares emergency overflow compaction metadata synchronously (falling back to 1024 recent tokens).
    pub fn prepare_overflow_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        if !self.settings.auto_compact.unwrap_or(true) {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        );
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();
        crate::compaction::prepare_compaction(&active_entries, comp_settings.keep_recent_tokens)
            .or_else(|| crate::compaction::prepare_compaction(&active_entries, 1024))
    }

    /// Persists a completed compaction entry into the active session branch.
    pub fn apply_prepared_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
    ) -> Result<()> {
        self.session
            .append_entry(crate::session::SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            })?;
        Ok(())
    }

    /// Runs proactive auto-compaction before a turn when enabled (default `true`).
    pub async fn run_proactive_compaction(&mut self) {
        if let Some(prep) = self.prepare_proactive_compaction() {
            if let Err(e) = self.run_prepared_compaction(prep, None).await {
                eprintln!("Warning: proactive auto-compaction failed: {e}");
            }
        }
    }

    /// Returns true if `messages` ended due to context overflow and auto-compaction is enabled.
    pub fn is_overflow_turn(&self, messages: &[Message]) -> bool {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return false;
        }
        messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(ast) => Some(ast),
                _ => None,
            })
            .is_some_and(|ast| is_context_overflow(ast, self.model.context_window))
    }

    /// Attempts emergency compaction after a context overflow turn. Propagates provider/cancel errors.
    pub async fn try_overflow_compaction(&mut self) -> Result<bool> {
        let Some(prep) = self.prepare_overflow_compaction() else {
            return Ok(false);
        };
        self.run_prepared_compaction(prep, None).await?;
        Ok(true)
    }

    /// Sends user content through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt_content(
        &mut self,
        content: UserContent,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        let pre_cancel = CancellationToken::new();
        *self.active_cancel.lock().unwrap() = Some(pre_cancel.clone());

        let auth_res = tokio::select! {
            _ = pre_cancel.cancelled() => {
                *self.active_cancel.lock().unwrap() = None;
                anyhow::bail!("Operation aborted");
            }
            res = self.ensure_api_key(None) => res,
        };
        if let Err(e) = auth_res {
            *self.active_cancel.lock().unwrap() = None;
            return Err(e);
        }
        if pre_cancel.is_cancelled() {
            *self.active_cancel.lock().unwrap() = None;
            anyhow::bail!("Operation aborted");
        }

        self.run_proactive_compaction().await;
        if pre_cancel.is_cancelled() {
            *self.active_cancel.lock().unwrap() = None;
            anyhow::bail!("Operation aborted");
        }

        let (user_msg, history, config) = self.prepare_prompt_content(content.clone());
        let new_messages = run_agent_loop(vec![user_msg], history, config, on_event.clone()).await;
        *self.active_cancel.lock().unwrap() = None;

        if self.is_overflow_turn(&new_messages) {
            match self.try_overflow_compaction().await {
                Ok(true) => {
                    let (retry_user, retry_history, retry_config) =
                        self.prepare_prompt_content(content);
                    let retry_messages =
                        run_agent_loop(vec![retry_user], retry_history, retry_config, on_event)
                            .await;
                    *self.active_cancel.lock().unwrap() = None;
                    self.finish_prompt(&retry_messages)?;
                    return Ok(retry_messages);
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("Warning: overflow auto-compaction failed: {e}");
                }
            }
        }

        self.finish_prompt(&new_messages)?;
        Ok(new_messages)
    }

    /// Sends a user prompt through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt(
        &mut self,
        text: &str,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        self.prompt_content(UserContent::Text(text.to_string()), on_event)
            .await
    }

    /// Manually triggers compaction on the active branch history, saving the summary entry.
    pub async fn compact(&mut self, custom_instructions: Option<&str>) -> Result<Option<String>> {
        let keep = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        )
        .keep_recent_tokens;
        self.compact_with_budget(custom_instructions, keep).await
    }

    async fn compact_with_budget(
        &mut self,
        custom_instructions: Option<&str>,
        keep_recent_tokens: usize,
    ) -> Result<Option<String>> {
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();

        let prep = match crate::compaction::prepare_compaction(&active_entries, keep_recent_tokens)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        let summary = self
            .run_prepared_compaction(prep, custom_instructions)
            .await?;
        Ok(Some(summary))
    }

    async fn run_prepared_compaction(
        &mut self,
        prep: crate::compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
    ) -> Result<String> {
        let existing = self.active_cancel.lock().unwrap().clone();
        let owned_slot = existing.is_none();
        let cancel = existing.unwrap_or_else(|| {
            let c = CancellationToken::new();
            *self.active_cancel.lock().unwrap() = Some(c.clone());
            c
        });
        let res = crate::compaction::compact_with_provider(
            &prep,
            &self.model,
            self.provider.as_ref(),
            self.api_key.as_deref(),
            custom_instructions,
            cancel,
        )
        .await;
        if owned_slot {
            *self.active_cancel.lock().unwrap() = None;
        }
        let summary = res?;
        self.apply_prepared_compaction(
            summary.clone(),
            prep.first_kept_entry_id,
            prep.tokens_before,
        )?;
        Ok(summary)
    }

    /// Navigates to a target entry in the session tree, optionally summarizing the abandoned branch.
    pub async fn navigate_tree(
        &mut self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<Option<String>> {
        let old_leaf_id = self.session.leaf_id().map(str::to_string);

        if self.session.get_entry(target_id).is_none() {
            return Err(anyhow!("Target entry ID not found: {target_id}"));
        }

        let summary = if summarize {
            let collected = crate::branch_summarization::collect_entries_for_branch_summary(
                &self.session,
                old_leaf_id.as_deref(),
                target_id,
            );
            if !collected.entries.is_empty() {
                let cancel = CancellationToken::new();
                let sum = crate::branch_summarization::generate_branch_summary(
                    &collected.entries,
                    &self.model,
                    self.provider.as_ref(),
                    self.api_key.as_deref(),
                    custom_instructions,
                    None,
                    cancel,
                )
                .await?;

                // Switch branch to target_id first
                self.session.branch_from(target_id)?;
                // Append BranchSummary on top of target branch
                let from_id = old_leaf_id.unwrap_or_else(|| "root".to_string());
                self.session
                    .append_entry(crate::session::SessionEntryKind::BranchSummary {
                        from_id,
                        summary: sum.clone(),
                    })?;
                Some(sum)
            } else {
                self.session.branch_from(target_id)?;
                None
            }
        } else {
            self.session.branch_from(target_id)?;
            None
        };

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEntryKind;
    use mehh_ai::{faux_model, StopReason};

    #[test]
    fn test_queues_and_abort() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_q_{}", uuid::Uuid::now_v7()));
        let session_mgr = SessionManager::in_memory(&temp_dir);
        let session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            session_mgr,
            Settings::default(),
        );

        assert_eq!(session.pending_steering_count(), 0);
        assert_eq!(session.pending_follow_up_count(), 0);
        assert!(!session.is_active());

        session.steer("Steer prompt");
        session.follow_up("Follow-up prompt");
        assert_eq!(session.pending_steering_count(), 1);
        assert_eq!(session.pending_follow_up_count(), 1);

        session.clear_queues();
        assert_eq!(session.pending_steering_count(), 0);
        assert_eq!(session.pending_follow_up_count(), 0);

        // Test cancel slot
        let cancel = CancellationToken::new();
        *session.active_cancel_slot().lock().unwrap() = Some(cancel.clone());
        assert!(session.is_active());
        assert!(session.abort());
        assert!(!session.is_active());
        assert!(cancel.is_cancelled());
        assert!(!session.abort());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_default_provider_routing() {
        let mut model = faux_model();
        model.api = "mistral-conversations".to_string();
        model.provider = "mistral".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "azure-openai-responses".to_string();
        model.provider = "azure-openai".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "bedrock-converse-stream".to_string();
        model.provider = "amazon-bedrock".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "google-vertex".to_string();
        model.provider = "google-vertex".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "anthropic-messages".to_string();
        model.provider = "anthropic".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "openai-responses".to_string();
        model.provider = "openai".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "faux".to_string();
        model.provider = "faux".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);
    }

    #[test]
    fn test_set_model_clears_api_key_on_provider_switch_and_preserves_custom_prompt() {
        let dir = std::env::temp_dir().join(format!("agent_session_key_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut anth = faux_model();
        anth.provider = "anthropic".to_string();
        anth.api = "anthropic-messages".to_string();

        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            anth.clone(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        session.set_custom_system_prompt(
            Some("Para 1.\n\nPara 2.".to_string()),
            Some("Tail rule.".to_string()),
        );
        session.set_api_key(Some("sk-ant-secret".to_string()));
        session.enable_mcp_and_codemode(Vec::new(), false);
        assert!(session.system_prompt.contains("Para 1.\n\nPara 2."));
        assert!(session.system_prompt.contains("Tail rule."));

        // Same provider preserves key
        let mut anth2 = anth;
        anth2.id = "claude-opus-4-5".to_string();
        session.set_model(anth2, Arc::new(FauxProvider::new()));
        assert_eq!(session.api_key.as_deref(), Some("sk-ant-secret"));

        // Different provider clears key so Anthropic key cannot leak as OpenAI bearer token
        let mut openai = faux_model();
        openai.provider = "openai".to_string();
        openai.api = "openai-completions".to_string();
        session.set_model(openai, Arc::new(FauxProvider::new()));
        assert!(session.api_key.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_reactive_overflow_auto_compaction_recovers() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_overflow_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut sm = SessionManager::in_memory(&dir);
        for i in 0..4 {
            sm.append_message(Message::User(UserMessage {
                content: UserContent::Text(format!("prior prompt {i}: {}", "data ".repeat(800))),
                timestamp: now_ms(),
            }))
            .unwrap();
            sm.append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                format!("prior reply {i}: {}", "reply ".repeat(800)),
            )))
            .unwrap();
        }

        // Responses consumed FIFO:
        // 1) Initial prompt -> context overflow error
        // 2) Compaction summary response
        // 3) Retry prompt -> final success
        let faux = Arc::new(FauxProvider::with_responses(vec![
            mehh_ai::AssistantMessage {
                content: vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                    text: String::new(),
                    text_signature: None,
                })],
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: mehh_ai::Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("prompt is too long: exceeded context length".to_string()),
                timestamp: now_ms(),
            },
            mehh_ai::faux_assistant_message("## Goal\nCompacted summary."),
            mehh_ai::faux_assistant_message("Recovered after compaction!"),
        ]));

        let mut model = faux_model();
        model.context_window = 200_000;
        let mut session = AgentSession::new(
            dir.clone(),
            model,
            faux,
            sm,
            Settings {
                auto_compact: None, // spec default: enabled (true)
                ..Settings::default()
            },
        );

        let msgs = session.prompt("continue", None).await.unwrap();
        let last = msgs.last().unwrap();
        match last {
            Message::Assistant(a) => {
                assert_eq!(a.stop_reason, StopReason::Stop);
                assert_eq!(a.text_content(), "Recovered after compaction!");
            }
            _ => panic!("expected assistant message"),
        }
        assert!(session
            .session
            .entries()
            .iter()
            .any(|e| { matches!(e.kind, crate::session::SessionEntryKind::Compaction { .. }) }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn navigate_tree_with_summarize_generates_and_appends_branch_summary() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_tree_{}", uuid::Uuid::now_v7()));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );

        // Root entry m0
        let m0 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Initial prompt".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Branch A entries: m1, m2
        let _m1 = session
            .session_mut()
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Working on branch A",
            )))
            .unwrap();
        let m2 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Follow up on branch A".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Switch to m0 without summary (creating branch B)
        session.navigate_tree(&m0, false, None).await.unwrap();
        let m3 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Branch B work".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Now from m3, navigate to m2 with summarize = true!
        let summary_opt = session.navigate_tree(&m2, true, None).await.unwrap();
        assert!(summary_opt.is_some());

        // Check active branch entries: leaf should now have branch summary
        let active = session.session().active_path_entries();
        let last_entry = active.last().unwrap();
        match &last_entry.kind {
            SessionEntryKind::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, &m3);
                assert!(!summary.is_empty());
            }
            _ => panic!("Expected BranchSummary as newest entry on active branch"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn prompt_content_forwards_user_blocks_with_image_to_agent_loop() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_blocks_{}", uuid::Uuid::now_v7()));
        let faux = Arc::new(mehh_ai::FauxProvider::with_responses(vec![
            mehh_ai::faux_assistant_message("I see the diagram."),
        ]));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            faux.clone(),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );

        let blocks = vec![
            mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                text: "describe this:".to_string(),
                text_signature: None,
            }),
            mehh_ai::UserContentBlock::Image(mehh_ai::ImageContent {
                mime_type: "image/png".to_string(),
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".to_string(),
            }),
        ];

        let res = session
            .prompt_content(mehh_ai::UserContent::Blocks(blocks), None)
            .await
            .unwrap();

        assert_eq!(res.len(), 2);
        let active = session.session().active_path_entries();
        let first_msg_entry = active.first().unwrap();
        match &first_msg_entry.kind {
            SessionEntryKind::Message {
                message: Message::User(u),
            } => match &u.content {
                UserContent::Blocks(b) => {
                    assert_eq!(b.len(), 2);
                    assert!(matches!(b[1], mehh_ai::UserContentBlock::Image(_)));
                }
                _ => panic!("Expected UserContent::Blocks"),
            },
            _ => panic!("Expected user message entry"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_agent_session_reload_picks_up_new_skill_and_prompt_template_from_disk() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_reload_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mut session = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            true,
        );

        assert!(session.skills.is_empty());
        assert!(session.prompt_templates.is_empty());

        // Create a new skill on disk
        let skill_dir = temp_dir.join(".agents").join("skills").join("calc");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: calc\ndescription: Calculates math\n---\nCalc instructions\n",
        )
        .unwrap();

        // Create a prompt template on disk
        let template_dir = temp_dir.join(".mehh").join("prompts");
        std::fs::create_dir_all(&template_dir).unwrap();
        std::fs::write(template_dir.join("review.md"), "Please review this code.").unwrap();

        session.reload();

        assert_eq!(session.skills.len(), 1);
        assert_eq!(session.skills[0].name, "calc");
        assert_eq!(session.prompt_templates.len(), 1);
        assert_eq!(session.prompt_templates[0].name, "review");
        assert!(session.system_prompt.contains("calc"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[derive(Debug)]
    struct MockTool {
        name: String,
    }

    #[async_trait::async_trait]
    impl AgentTool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<mehh_agent::AgentToolUpdateCallback>,
        ) -> Result<mehh_agent::AgentToolResult, String> {
            Ok(mehh_agent::AgentToolResult::text("ok"))
        }
    }

    #[test]
    fn test_sync_mcp_tools_preserves_codemode_and_catalog_handles() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_sync_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mut session = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            true,
        );

        // 1. Codemode preservation
        session.enable_mcp_and_codemode(vec![], true);
        let orig_codemode = session.codemode_tool().expect("codemode must be set");

        // Sync with new mcp tools preserves existing CodemodeTool instance
        session.sync_mcp_tools(vec![], true);
        let synced_codemode = session.codemode_tool().expect("codemode still set");
        assert!(Arc::ptr_eq(&orig_codemode, &synced_codemode));

        // 2. Deferred catalog preservation (> 20 tools)
        let dummy_tools: Vec<Arc<dyn AgentTool>> = (0..25)
            .map(|i| {
                Arc::new(MockTool {
                    name: format!("mcp__srv__tool_{i}"),
                }) as Arc<dyn AgentTool>
            })
            .collect();
        session.sync_mcp_tools(dummy_tools, false);
        let orig_catalog = session.deferred_catalog().expect("catalog must be set");

        let updated_tools: Vec<Arc<dyn AgentTool>> = (0..22)
            .map(|i| {
                Arc::new(MockTool {
                    name: format!("mcp__srv__tool_{i}"),
                }) as Arc<dyn AgentTool>
            })
            .collect();
        session.sync_mcp_tools(updated_tools, false);
        let synced_catalog = session.deferred_catalog().expect("catalog still set");
        assert!(Arc::ptr_eq(&orig_catalog, &synced_catalog));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn prepare_request_replaces_moim_block_instead_of_appending() {
        let dir = std::env::temp_dir().join(format!("agent_session_moim_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        session
            .todo_state
            .write()
            .await
            .add("first task".to_string());

        let (user, _history, config) = session.prepare_prompt("hello");
        let hook = config.prepare_request.expect("prepare hook");
        let mut messages = vec![user];
        for _ in 0..3 {
            let ctx = mehh_agent::types::PrepareRequestContext {
                context: mehh_agent::types::AgentContext {
                    messages: messages.clone(),
                    tools: None,
                },
                model: faux_model(),
                thinking_level: None,
            };
            let update = hook(ctx, CancellationToken::new()).await.expect("update");
            messages = update.context.expect("context").messages;
        }
        let text = match messages.last().expect("user message") {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        };
        assert_eq!(
            text.matches("<working_checklist>").count(),
            1,
            "moim block duplicated across turns: {text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cross_provider_worker_uses_worker_key_at_turn_zero() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_worker_key_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let (manager, lead_key, worker_provider) = {
            let mut mgr = session.lead_worker.lock().unwrap();
            let mut worker = faux_model();
            worker.provider = "openai".to_string();
            mgr.worker_model = Some(worker);
            mgr.set_worker_api_key(Some("worker-value".to_string()));
            mgr.set_lead_api_key(Some("lead-value".to_string()));
            (mgr.clone(), mgr.lead_api_key.clone(), "openai".to_string())
        };
        assert_eq!(
            manager.active_api_key().as_deref(),
            lead_key.as_deref(),
            "turn zero active key is the lead key"
        );
        assert_eq!(
            super::subagent_api_key(&manager, &lead_key, &worker_provider, "anthropic").as_deref(),
            Some("worker-value"),
            "worker tool must never receive the lead key"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn set_model_updates_lead_worker_state() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_lead_model_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let mut next = faux_model();
        next.id = "next-lead".to_string();
        let next_provider: Arc<dyn LlmProvider> = Arc::new(FauxProvider::new());
        session.set_model(next.clone(), next_provider.clone());
        let lw = session.lead_worker.lock().unwrap();
        assert_eq!(lw.lead_model.id, "next-lead");
        assert!(Arc::ptr_eq(&lw.lead_provider, &next_provider));
        drop(lw);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn after_tool_call_records_lead_worker_outcome() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_outcome_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let (_user, _history, config) = session.prepare_prompt("hello");
        let hook = config.after_tool_call.expect("after tool hook");
        let ctx = mehh_agent::types::AfterToolCallContext {
            assistant_message: mehh_ai::faux_assistant_message(""),
            tool_call: mehh_ai::ToolCall {
                id: "tc1".to_string(),
                name: "tool".to_string(),
                arguments: serde_json::Map::new(),
                arguments_error: None,
                thought_signature: None,
            },
            args: serde_json::Map::new(),
            result: mehh_agent::AgentToolResult::text("failed"),
            is_error: true,
            context: mehh_agent::types::AgentContext {
                messages: Vec::new(),
                tools: None,
            },
        };
        hook(ctx, CancellationToken::new()).await;
        assert_eq!(
            session.lead_worker.lock().unwrap().consecutive_tool_errors,
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn zero_lead_turns_starts_on_worker_model() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_zero_lead_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        {
            let mut mgr = session.lead_worker.lock().unwrap();
            let mut worker = faux_model();
            worker.id = "worker-first".to_string();
            mgr.worker_model = Some(worker);
            mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
            mgr.lead_turns = 0;
            mgr.set_worker_api_key(Some("worker-value".to_string()));
        }
        let (_user, _history, config) = session.prepare_prompt("hello");
        assert_eq!(config.model.id, "worker-first");
        assert_eq!(
            config.stream_options.api_key.as_deref(),
            Some("worker-value")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
`````

## crates/mehh-coding-agent/src/tool_inspection.rs

`````rust
//! Runtime tool governance pipeline: security inspection, permission modes, and repetition breakers.

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// Result of inspecting a candidate tool call before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectionVerdict {
    /// Proceed with tool execution without user prompt.
    Allow,
    /// Tool requires interactive or external user approval.
    RequireApproval {
        reason: String,
        preview: Option<String>,
    },
    /// Tool call is blocked due to security or policy constraint.
    Deny { reason: String },
}

/// Active permission mode controlling tool execution governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Executes all safe and mutating tools without approval prompts (security flags still block).
    Auto,
    /// Default interactive mode: auto-allows read-only exploration and whitelisted inspection commands,
    /// prompts for file mutations, unknown shell commands, and destructive actions.
    #[default]
    SmartApprove,
    /// Prompts for all tool calls unless explicitly marked always allow.
    Approve,
    /// Conversational exploration only: allows read-only tools, strictly denies mutating tools and shell execution.
    Chat,
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(PermissionMode::Auto),
            "smart_approve" | "smart" | "smartapprove" => Ok(PermissionMode::SmartApprove),
            "approve" => Ok(PermissionMode::Approve),
            "chat" => Ok(PermissionMode::Chat),
            other => Err(format!(
                "Invalid permission mode '{other}'. Supported modes: auto, smart_approve, approve, chat"
            )),
        }
    }
}

/// Context provided to each tool inspector.
#[derive(Debug, Clone)]
pub struct ToolInspectionContext {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub is_read_only: Option<bool>,
    pub cwd: PathBuf,
    /// Identifies the requesting agent (e.g. "subagent:#2:coding"); None = main session.
    pub origin: Option<String>,
}

/// Pluggable tool inspection component in the chain of responsibility.
#[async_trait]
pub trait ToolInspector: Send + Sync {
    fn name(&self) -> &str;
    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
}

/// Dangerous shell patterns matched by `SecurityInspector`.
static DANGEROUS_SHELL_PATTERNS: &[(&str, &str)] = &[
    (
        r"(?i)\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?(/|~|\$HOME)",
        "Recursive root or home directory deletion",
    ),
    (r"(?i)\bmkfs(\.\w+)?\b", "Filesystem creation"),
    (r"(?i)\bdd\s+if=.*of=/dev/", "Raw block device write"),
    (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:", "Fork bomb"),
    (
        r"(?i)(curl|wget)\s+.*\|\s*(sh|bash|zsh)",
        "Piping web download directly to shell interpreter",
    ),
    (
        r"(?i)(\.ssh/id_|\.mehh/agent/auth\.json)",
        "Accessing private keys or auth credentials",
    ),
    (
        r"(?i)(Invoke-Expression|iex)\s*(\(New-Object\s+Net\.WebClient|\(Invoke-WebRequest)",
        "PowerShell remote script execution",
    ),
];

/// Normalizes path for boundary and security checks.
fn normalize_path_for_check(base: &Path, target: &str) -> PathBuf {
    let p = Path::new(target);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut components = Vec::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(last) = components.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        components.pop();
                        continue;
                    }
                }
                components.push(comp);
            }
            _ => components.push(comp),
        }
    }
    let collapsed: PathBuf = components.into_iter().collect();
    #[cfg(windows)]
    {
        PathBuf::from(
            collapsed
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace('/', "\\"),
        )
    }
    #[cfg(not(windows))]
    {
        collapsed
    }
}

/// Priority 1 Inspector: Enforces invariant security constraints across ALL modes.
pub struct SecurityInspector {
    compiled_patterns: Vec<(Regex, &'static str)>,
}

impl SecurityInspector {
    pub fn new() -> Self {
        let mut compiled = Vec::new();
        for &(pat, desc) in DANGEROUS_SHELL_PATTERNS {
            if let Ok(re) = Regex::new(pat) {
                compiled.push((re, desc));
            }
        }
        Self {
            compiled_patterns: compiled,
        }
    }
}

impl Default for SecurityInspector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolInspector for SecurityInspector {
    fn name(&self) -> &str {
        "security_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // 1. Guard against direct mutation of .mehh configuration and auth files
        if matches!(ctx.tool_name.as_str(), "edit" | "write" | "undo_edit") {
            if let Some(path_val) = ctx.args.get("path").and_then(|v| v.as_str()) {
                let norm = path_val.replace('\\', "/").to_ascii_lowercase();
                if norm.starts_with(".mehh/")
                    || norm == ".mehh"
                    || norm.contains("/.mehh/")
                    || norm.contains("~/.mehh/")
                    || norm.contains("auth.json")
                    || norm.contains("trust.json")
                {
                    return InspectionVerdict::Deny {
                        reason: format!("Direct mutation of .mehh configuration metadata file '{path_val}' is blocked by security policy."),
                    };
                }

                let target_path = normalize_path_for_check(&ctx.cwd, path_val);
                let cwd_norm = normalize_path_for_check(&ctx.cwd, "");
                if !target_path.starts_with(&cwd_norm) {
                    return InspectionVerdict::RequireApproval {
                        reason: format!("File modification outside current working directory requires approval: '{path_val}'"),
                        preview: Some(path_val.to_string()),
                    };
                }
            }
        }

        // 2. Guard against catastrophic shell commands
        if matches!(ctx.tool_name.as_str(), "bash" | "powershell") {
            if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                for (re, desc) in &self.compiled_patterns {
                    if re.is_match(cmd) {
                        return InspectionVerdict::Deny {
                            reason: format!("Command blocked by security policy: {desc} ('{cmd}')"),
                        };
                    }
                }
            }
        }

        InspectionVerdict::Allow
    }
}

/// Priority 2 Inspector: Enforces user permission mode and granular rule overrides.
pub struct PermissionInspector {
    mode: PermissionMode,
    safe_commands: Vec<Regex>,
    store: crate::permissions::PermissionStore,
}

impl PermissionInspector {
    pub fn new(mode: PermissionMode) -> Self {
        Self::new_with_store(mode, crate::permissions::PermissionStore::default())
    }

    pub fn new_with_store(
        mode: PermissionMode,
        store: crate::permissions::PermissionStore,
    ) -> Self {
        let safe_cmd_patterns = &[
            r"^(git\s+(status|diff|log|show|branch|rev-parse))",
            r"^(cargo\s+(check|clippy|test|tree|version))",
            r"^(ls|dir|pwd|cat|find|which|echo)(\s+.*)?$",
            r"^(Get-ChildItem|Get-Location|Get-Content|Select-String)(\s+.*)?$",
        ];
        let mut compiled = Vec::new();
        for &p in safe_cmd_patterns {
            if let Ok(re) = Regex::new(p) {
                compiled.push(re);
            }
        }
        Self {
            mode,
            safe_commands: compiled,
            store,
        }
    }

    pub fn store(&self) -> &crate::permissions::PermissionStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut crate::permissions::PermissionStore {
        &mut self.store
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    fn is_safe_shell_command(&self, cmd: &str) -> bool {
        let trimmed = cmd.trim();
        // Check for shell chaining or output redirection that could mutate
        if trimmed.contains('>')
            || trimmed.contains(">>")
            || trimmed.contains('|')
            || trimmed.contains(';')
            || trimmed.contains("&&")
        {
            return false;
        }
        self.safe_commands.iter().any(|re| re.is_match(trimmed))
    }
}

#[async_trait]
impl ToolInspector for PermissionInspector {
    fn name(&self) -> &str {
        "permission_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // 0. Explicit rule matching from persisted permissions
        if let Some(rule_verdict) = self.store.evaluate(ctx) {
            return rule_verdict;
        }

        match self.mode {
            PermissionMode::Auto => InspectionVerdict::Allow,
            PermissionMode::Chat => {
                if ctx.is_read_only == Some(true)
                    || ctx.tool_name == "todo"
                    || ctx.tool_name == "tool_search"
                {
                    InspectionVerdict::Allow
                } else {
                    InspectionVerdict::Deny {
                        reason: format!("Tool '{}' is blocked in Chat mode (only read-only exploration tools are permitted).", ctx.tool_name),
                    }
                }
            }
            PermissionMode::Approve => InspectionVerdict::RequireApproval {
                reason: format!(
                    "Mode 'approve' requires confirmation for tool '{}'",
                    ctx.tool_name
                ),
                preview: ctx
                    .args
                    .get("command")
                    .or_else(|| ctx.args.get("path"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            },
            PermissionMode::SmartApprove => {
                // Read-only tools proceed automatically
                if ctx.is_read_only == Some(true)
                    || ctx.tool_name == "todo"
                    || ctx.tool_name == "tool_search"
                {
                    return InspectionVerdict::Allow;
                }

                // Check whitelisted shell commands
                if matches!(ctx.tool_name.as_str(), "bash" | "powershell") {
                    if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                        if self.is_safe_shell_command(cmd) {
                            return InspectionVerdict::Allow;
                        }
                        return InspectionVerdict::RequireApproval {
                            reason: format!("Shell command requires approval: '{cmd}'"),
                            preview: Some(cmd.to_string()),
                        };
                    }
                }

                // File mutations require approval
                if matches!(ctx.tool_name.as_str(), "edit" | "write" | "undo_edit") {
                    let path_str = ctx
                        .args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown)");
                    return InspectionVerdict::RequireApproval {
                        reason: format!(
                            "Tool '{}' on '{}' requires approval",
                            ctx.tool_name, path_str
                        ),
                        preview: Some(path_str.to_string()),
                    };
                }

                // Unannotated or unknown tool calls default to requiring approval
                InspectionVerdict::RequireApproval {
                    reason: format!("Tool '{}' requires approval", ctx.tool_name),
                    preview: None,
                }
            }
        }
    }
}

/// State tracking consecutive identical tool calls per requesting origin.
#[derive(Debug, Default)]
struct RepetitionEntry {
    last_key: Option<String>,
    count: usize,
}

#[derive(Debug, Default)]
struct RepetitionState {
    per_origin: std::collections::HashMap<Option<String>, RepetitionEntry>,
}

/// Priority 3 Inspector: Halts identical repeated tool calls after `max_repetitions`.
pub struct RepetitionInspector {
    max_repetitions: usize,
    state: Mutex<RepetitionState>,
}

impl RepetitionInspector {
    pub fn new(max_repetitions: usize) -> Self {
        Self {
            max_repetitions,
            state: Mutex::new(RepetitionState::default()),
        }
    }
}

impl Default for RepetitionInspector {
    fn default() -> Self {
        Self::new(3)
    }
}

#[async_trait]
impl ToolInspector for RepetitionInspector {
    fn name(&self) -> &str {
        "repetition_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        let key = format!(
            "{}:{}",
            ctx.tool_name,
            serde_json::to_string(&ctx.args).unwrap_or_default()
        );
        let mut st = self.state.lock().await;
        // Scope repetition tracking per origin so parallel subagents doing
        // legitimate identical reads do not trip each other's breaker.
        let entry = st.per_origin.entry(ctx.origin.clone()).or_default();
        if entry.last_key.as_deref() == Some(&key) {
            entry.count += 1;
            if entry.count > self.max_repetitions {
                return InspectionVerdict::Deny {
                    reason: format!(
                        "Repeated identical tool call '{}' detected ({} times). Change strategy or inspect prior output instead of repeating the exact same call.",
                        ctx.tool_name, entry.count
                    ),
                };
            }
        } else {
            entry.last_key = Some(key);
            entry.count = 1;
        }

        InspectionVerdict::Allow
    }
}

/// Manager orchestrating the priority chain of tool inspectors.
pub struct ToolInspectionManager {
    mode: Mutex<PermissionMode>,
    security: SecurityInspector,
    permission: Mutex<PermissionInspector>,
    repetition: RepetitionInspector,
    cwd: PathBuf,
    /// Serializes user approval prompts across the session and all subagents.
    approval_lock: Mutex<()>,
}

impl ToolInspectionManager {
    pub fn new(mode: PermissionMode, cwd: PathBuf) -> Self {
        Self::new_with_store(
            mode,
            cwd.clone(),
            crate::permissions::PermissionStore::load(&cwd, false).unwrap_or_default(),
        )
    }

    pub fn new_with_store(
        mode: PermissionMode,
        cwd: PathBuf,
        store: crate::permissions::PermissionStore,
    ) -> Self {
        Self {
            mode: Mutex::new(mode),
            security: SecurityInspector::new(),
            permission: Mutex::new(PermissionInspector::new_with_store(mode, store)),
            repetition: RepetitionInspector::default(),
            cwd,
            approval_lock: Mutex::new(()),
        }
    }

    pub async fn add_permission_rule(
        &self,
        rule: crate::permissions::PermissionRule,
    ) -> anyhow::Result<()> {
        let mut perm = self.permission.lock().await;
        perm.store_mut().add_rule(rule)
    }

    /// Adds a rule to the in-memory store for this session only.
    pub async fn add_session_permission_rule(&self, rule: crate::permissions::PermissionRule) {
        self.permission
            .lock()
            .await
            .store_mut()
            .add_session_rule(rule);
    }

    pub async fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().await = mode;
        self.permission.lock().await.set_mode(mode);
    }

    pub async fn mode(&self) -> PermissionMode {
        *self.mode.lock().await
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // Priority 1: Security Inspector (runs in all modes, strictly authoritative)
        let sec_verdict = self.security.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = sec_verdict {
            return sec_verdict;
        }
        if let InspectionVerdict::RequireApproval { .. } = sec_verdict {
            return sec_verdict;
        }

        // Priority 2: Permission Inspector
        let perm_verdict = self.permission.lock().await.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = perm_verdict {
            return perm_verdict;
        }

        // Priority 3: Repetition Inspector
        let rep_verdict = self.repetition.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = rep_verdict {
            return rep_verdict;
        }

        // If any inspector requested approval, return RequireApproval
        if let InspectionVerdict::RequireApproval { .. } = perm_verdict {
            return perm_verdict;
        }

        InspectionVerdict::Allow
    }

    /// Evaluates inspectors and queries the approval handler if `RequireApproval` is reached.
    pub async fn inspect_and_authorize(
        &self,
        ctx: &ToolInspectionContext,
        handler: Option<&dyn ApprovalHandler>,
    ) -> mehh_agent::BeforeToolCallResult {
        match self.inspect(ctx).await {
            InspectionVerdict::Allow => mehh_agent::BeforeToolCallResult::default(),
            InspectionVerdict::Deny { reason } => mehh_agent::BeforeToolCallResult {
                block: true,
                reason: Some(reason),
                terminate: false,
            },
            InspectionVerdict::RequireApproval { reason, preview } => {
                if let Some(h) = handler {
                    // One prompt at a time across session + subagents; label
                    // the request with its origin so the user knows who asks.
                    let _gate = self.approval_lock.lock().await;
                    let labeled = match &ctx.origin {
                        Some(o) => format!("[{o}] {reason}"),
                        None => reason.clone(),
                    };
                    if h.request_approval(ctx, &labeled, preview.as_deref()).await {
                        return mehh_agent::BeforeToolCallResult::default();
                    }
                }
                mehh_agent::BeforeToolCallResult {
                    block: true,
                    reason: Some(format!(
                        "Tool '{}' blocked: approval required but not granted ({})",
                        ctx.tool_name, reason
                    )),
                    terminate: false,
                }
            }
        }
    }
}

/// Handler deciding whether to authorize a tool call requiring user approval.
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request_approval(
        &self,
        ctx: &ToolInspectionContext,
        reason: &str,
        preview: Option<&str>,
    ) -> bool;
}

/// Headless approval handler: fails closed on any approval requirement.
pub struct HeadlessDenyHandler;

#[async_trait]
impl ApprovalHandler for HeadlessDenyHandler {
    async fn request_approval(
        &self,
        _ctx: &ToolInspectionContext,
        _reason: &str,
        _preview: Option<&str>,
    ) -> bool {
        false
    }
}
`````

