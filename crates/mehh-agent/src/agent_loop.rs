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

/// Maximum number of consecutive turns whose tool-call response was truncated
/// at the output token limit. Retrying such a turn re-sends a strictly larger
/// context, so without a bound the loop can spin up to `max_turns` times.
const MAX_CONSECUTIVE_TRUNCATED_TURNS: usize = 2;

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
        let dyn_snapshot = mehh_ai::read_recover(dynamic_lock).clone();
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
    let mut q = mehh_ai::lock_recover(queue);
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
    let mut consecutive_truncated_turns: usize = 0;

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
                            current_messages = Arc::unwrap_or_clone(ctx.messages);
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
                let shared = Arc::new(std::mem::take(&mut current_messages));
                let req_ctx = crate::types::PrepareRequestContext {
                    context: crate::types::AgentContext {
                        messages: Arc::clone(&shared),
                        tools: Some(active_tools.clone()),
                    },
                    model: current_model.clone(),
                    thinking_level: current_thinking_level,
                };
                let update = hook(req_ctx, config.cancel.clone()).await;
                if let Some(update) = update {
                    if let Some(ctx) = update.context {
                        drop(shared);
                        current_messages = Arc::unwrap_or_clone(ctx.messages);
                        if let Some(tools) = ctx.tools {
                            active_tools = tools;
                        }
                    } else {
                        current_messages = Arc::unwrap_or_clone(shared);
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
                } else {
                    current_messages = Arc::unwrap_or_clone(shared);
                }
            }

            let assistant_message = stream_assistant_response(
                StreamRequest {
                    messages: &current_messages,
                    tools: &active_tools,
                    model: &current_model,
                    provider: &current_provider,
                    api_key: current_api_key.as_deref(),
                    thinking_level: current_thinking_level,
                },
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
                        messages: Arc::new(current_messages.clone()),
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

            let is_truncated_tool_turn =
                !tool_calls.is_empty() && assistant_message.stop_reason == StopReason::Length;
            if is_truncated_tool_turn {
                consecutive_truncated_turns += 1;
            } else {
                consecutive_truncated_turns = 0;
            }

            if !tool_calls.is_empty() {
                if is_truncated_tool_turn {
                    let batch = fail_truncated_tool_calls(&tool_calls, &on_event);
                    for tr in &batch {
                        current_messages.push(Message::ToolResult(tr.clone()));
                        new_messages.push(Message::ToolResult(tr.clone()));
                    }
                    tool_results.extend(batch);

                    if consecutive_truncated_turns >= MAX_CONSECUTIVE_TRUNCATED_TURNS {
                        let mut error_msg = AssistantMessage::empty_for_model(&current_model);
                        error_msg.stop_reason = StopReason::Error;
                        error_msg.error_message = Some(format!(
                            "Tool call arguments were truncated {consecutive_truncated_turns} consecutive times at the output token limit; stopping to avoid an unbounded retry loop."
                        ));
                        current_messages.push(Message::Assistant(error_msg.clone()));
                        new_messages.push(Message::Assistant(error_msg.clone()));
                        emit(AgentEvent::MessageStart {
                            message: Message::Assistant(error_msg.clone()),
                        });
                        emit(AgentEvent::MessageEnd {
                            message: Message::Assistant(error_msg.clone()),
                        });
                        if let Some(ref hook) = config.finish_turn {
                            let turn_ctx = crate::types::AgentTurnContext {
                                turn_index: turns_completed,
                                messages: Arc::new(current_messages.clone()),
                                assistant_message: error_msg.clone(),
                                tool_results: tool_results.clone(),
                            };
                            let _ = hook(turn_ctx, config.cancel.clone()).await;
                        }
                        emit(AgentEvent::TurnEnd {
                            message: error_msg,
                            tool_results,
                        });
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        });
                        return new_messages;
                    }

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

            let shared_turn_messages =
                if config.finish_turn.is_some() || config.prepare_next_turn.is_some() {
                    Some(Arc::new(current_messages.clone()))
                } else {
                    None
                };

            let mut finish_decision = None;
            if let Some(ref hook) = config.finish_turn {
                let turn_ctx = crate::types::AgentTurnContext {
                    turn_index: turns_completed,
                    messages: Arc::clone(shared_turn_messages.as_ref().expect("built above")),
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
                    messages: shared_turn_messages.expect("built above"),
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
            if config.cancel.is_cancelled()
                || config.max_turns.is_some_and(|cap| turns_completed >= cap)
            {
                // They were removed from the shared queue; put them back before
                // the outer guard ends the loop or they disappear silently.
                let mut queue = config
                    .follow_up_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for msg in follow_up_messages.into_iter().rev() {
                    queue.push_front(msg);
                }
                break 'outer;
            }
            pending_messages = follow_up_messages;
            emit(AgentEvent::TurnStart);
            turn_open = true;
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

struct StreamRequest<'a> {
    messages: &'a [Message],
    tools: &'a [Arc<dyn AgentTool>],
    model: &'a Model,
    provider: &'a Arc<dyn mehh_ai::LlmProvider>,
    api_key: Option<&'a str>,
    thinking_level: Option<mehh_ai::ThinkingLevel>,
}

async fn stream_assistant_response(
    request: StreamRequest<'_>,
    config: &AgentLoopConfig,
    on_event: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> AssistantMessage {
    let StreamRequest {
        messages,
        tools,
        model,
        provider,
        api_key,
        thinking_level,
    } = request;
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
    shared_messages: Option<&Arc<Vec<Message>>>,
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
            messages: shared_messages
                .map(Arc::clone)
                .unwrap_or_else(|| Arc::new(Vec::new())),
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
    let mut validated_args = match mehh_ai::validation::validate_tool_arguments(&tool_def, tool_call) {
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
        if let Some(target) = before_res.approved_path {
            // Bind the write to the exact path the inspector approved. Tools
            // revalidate it immediately before the rename, closing the gap
            // between inspection and execution.
            validated_args.insert(
                crate::types::INSPECTED_TARGET_ARG.to_string(),
                serde_json::Value::String(target.to_string_lossy().into_owned()),
            );
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
    let shared_messages = (config.before_tool_call.is_some() || config.after_tool_call.is_some())
        .then(|| Arc::new(current_messages.to_vec()));

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

        let outcome = execute_single_tool(
            assistant_message,
            tc,
            tools,
            shared_messages.as_ref(),
            config,
            on_event,
        )
        .await;

        emit(AgentEvent::MessageStart {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Message::ToolResult(outcome.tool_result_message.clone()),
        });

        let should_terminate = outcome.terminate;
        messages.push(outcome.tool_result_message.clone());
        outcomes.push(outcome);

        if should_terminate || config.cancel.is_cancelled() {
            if idx + 1 < tool_calls.len() {
                let reason = if should_terminate {
                    "Tool call skipped because a previous tool terminated the turn"
                } else {
                    "Tool call aborted because operation was cancelled"
                };
                let remaining = emit_synthetic_tool_errors(
                    &tool_calls[idx + 1..],
                    |_| reason.to_string(),
                    on_event,
                );
                messages.extend(remaining);
            }
            break;
        }
    }

    let terminate = outcomes.iter().any(|o| o.terminate);
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

    let shared_messages = (config.before_tool_call.is_some() || config.after_tool_call.is_some())
        .then(|| Arc::new(current_messages.to_vec()));
    let futures = tool_calls.iter().map(|tc| {
        execute_single_tool(
            assistant_message,
            tc,
            tools,
            shared_messages.as_ref(),
            config,
            on_event,
        )
    });

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

    let terminate = outcomes.iter().any(|o| o.terminate);
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
        execute_tool_calls_sequential(assistant_message, tool_calls, tools, current_messages, config, on_event).await
    } else {
        execute_tool_calls_parallel(assistant_message, tool_calls, tools, current_messages, config, on_event).await
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
    async fn truncated_tool_calls_terminate_after_bounded_retries() {
        let provider = Arc::new(FauxProvider::new());
        // Queue more truncated responses than the retry bound so we can prove
        // the loop stops early instead of draining the queue up to max_turns.
        for i in 0..5 {
            let mut msg = faux_tool_call_message(vec![faux_tool_call(
                format!("c{i}"),
                "ping",
                serde_json::json!({}),
            )]);
            msg.stop_reason = StopReason::Length;
            provider.add_response(msg);
        }
        // A normal final message that must NOT be consumed once the loop stops.
        provider.add_response(faux_assistant_message("should not be reached"));

        let mut config = AgentLoopConfig::new(faux_model(), provider.clone());
        config.tools = vec![Arc::new(DummyTool {
            tool_name: "ping".to_string(),
            mode: ToolExecutionMode::Parallel,
            on_exec: None,
        })];

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let prompt = Message::User(UserMessage {
            content: UserContent::Text("go".to_string()),
            timestamp: now_ms(),
        });
        let out = run_agent_loop(
            vec![prompt],
            vec![],
            config,
            Some(Arc::new(move |ev| events_clone.lock().unwrap().push(ev))),
        )
        .await;

        // Bounded to 2 consecutive truncated turns: stop before draining all queued responses.
        let request_count = provider.recorded_requests().len();
        assert_eq!(
            request_count, 2,
            "loop should terminate after the retry bound, got {request_count} requests"
        );

        let has_truncation_error = out.iter().any(|m| match m {
            Message::Assistant(a) => a
                .error_message
                .as_deref()
                .is_some_and(|e| e.contains("truncat")),
            _ => false,
        });
        assert!(
            has_truncation_error,
            "final messages must contain a truncation error"
        );

        let agent_end_count = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
            .count();
        assert_eq!(agent_end_count, 1, "AgentEnd must be emitted exactly once");
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
                    approved_path: None,
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
                seen.lock().unwrap().extend(ctx.context.messages.iter().cloned());
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
            None,
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

    #[tokio::test]
    async fn sequential_tool_terminate_stops_remaining_calls_and_parallel_uses_any() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let second_execs = Arc::new(AtomicUsize::new(0));
        let second_execs_clone = second_execs.clone();

        let mut config = AgentLoopConfig::new(faux_model(), Arc::new(FauxProvider::new()));
        config.tools = vec![
            Arc::new(DummyTool {
                tool_name: "first".to_string(),
                mode: ToolExecutionMode::Sequential,
                on_exec: None,
            }),
            Arc::new(DummyTool {
                tool_name: "second".to_string(),
                mode: ToolExecutionMode::Sequential,
                on_exec: Some(Arc::new(move || {
                    second_execs_clone.fetch_add(1, Ordering::SeqCst);
                })),
            }),
        ];
        config.before_tool_call = Some(Arc::new(|ctx, _cancel| {
            let terminate = ctx.tool_call.name == "first";
            Box::pin(async move {
                crate::types::BeforeToolCallResult {
                    terminate,
                    ..Default::default()
                }
            })
        }));

        let assistant = faux_assistant_message("tools");
        let calls = vec![
            faux_tool_call("c1", "first", serde_json::json!({})),
            faux_tool_call("c2", "second", serde_json::json!({})),
        ];

        let (msgs, terminate) =
            execute_tool_calls_sequential(&assistant, &calls, &config.tools, &[], &config, &None)
                .await;
        assert!(terminate);
        assert_eq!(second_execs.load(Ordering::SeqCst), 0);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].is_error);

        let (_p_msgs, p_terminate) =
            execute_tool_calls_parallel(&assistant, &calls, &config.tools, &[], &config, &None)
                .await;
        assert!(p_terminate, "parallel batch must terminate if any tool terminates");
    }
}
