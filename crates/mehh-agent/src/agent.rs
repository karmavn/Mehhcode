use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use mehh_ai::{
    now_ms, LlmProvider, Message, Model, StreamOptions, ThinkingLevel, UserContent,
    UserMessage,
};
use crate::agent_loop::{run_agent_loop, AgentLoopConfig, DynamicToolsList};
use crate::types::{
    AfterToolCallContext, AfterToolCallHook, AfterToolCallResult, AgentEvent, AgentState, AgentTool,
    BeforeToolCallContext, BeforeToolCallHook, BeforeToolCallResult, FinishTurnHook,
    PrepareNextTurnHook, PrepareRequestHook, QueueMode, TransformContextHook,
};

#[derive(Clone)]
pub struct CancelHandle {
    token: Arc<std::sync::RwLock<CancellationToken>>,
}

impl CancelHandle {
    fn new() -> Self {
        Self {
            token: Arc::new(std::sync::RwLock::new(CancellationToken::new())),
        }
    }

    pub fn cancel(&self) {
        mehh_ai::read_recover(&self.token).cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        mehh_ai::read_recover(&self.token).is_cancelled()
    }

    fn current(&self) -> CancellationToken {
        mehh_ai::read_recover(&self.token).clone()
    }

    fn reset_if_cancelled(&self) {
        let mut guard = mehh_ai::write_recover(&self.token);
        if guard.is_cancelled() {
            *guard = CancellationToken::new();
        }
    }
}

type EventListener = Arc<dyn Fn(AgentEvent) + Send + Sync>;

pub struct Agent {
    model: Model,
    provider: Arc<dyn LlmProvider>,
    system_prompt: Option<String>,
    tools: Vec<Arc<dyn AgentTool>>,
    stream_options: StreamOptions,
    messages: Vec<Message>,
    steering_queue: Arc<Mutex<VecDeque<Message>>>,
    follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    listeners: Vec<(usize, EventListener)>,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    dynamic_tools: Option<DynamicToolsList>,
    max_turns: Option<usize>,
    prepare_request: Option<PrepareRequestHook>,
    prepare_next_turn: Option<PrepareNextTurnHook>,
    transform_context: Option<TransformContextHook>,
    finish_turn: Option<FinishTurnHook>,
    cancel: CancelHandle,
    state: Arc<std::sync::RwLock<AgentState>>,
}

impl Agent {
    pub fn new(model: Model, provider: Arc<dyn LlmProvider>) -> Self {
        let initial_state = AgentState {
            system_prompt: String::new(),
            model: model.clone(),
            thinking_level: None,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: std::collections::HashSet::new(),
            error_message: None,
        };
        Self {
            model,
            provider,
            system_prompt: None,
            tools: Vec::new(),
            stream_options: StreamOptions::default(),
            messages: Vec::new(),
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            listeners: Vec::new(),
            before_tool_call: None,
            after_tool_call: None,
            dynamic_tools: None,
            max_turns: Some(crate::agent_loop::DEFAULT_MAX_AGENT_TURNS),
            prepare_request: None,
            prepare_next_turn: None,
            transform_context: None,
            finish_turn: None,
            cancel: CancelHandle::new(),
            state: Arc::new(std::sync::RwLock::new(initial_state)),
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        let p = prompt.into();
        mehh_ai::write_recover(&self.state).system_prompt = p.clone();
        self.system_prompt = Some(p);
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        mehh_ai::write_recover(&self.state).tools = tools.clone();
        self.tools = tools;
        self
    }

    pub fn with_stream_options(mut self, options: StreamOptions) -> Self {
        mehh_ai::write_recover(&self.state).thinking_level = options.reasoning;
        self.stream_options = options;
        self
    }

    pub fn with_thinking_level(mut self, level: Option<ThinkingLevel>) -> Self {
        self.stream_options.reasoning = level;
        mehh_ai::write_recover(&self.state).thinking_level = level;
        self
    }

    pub fn with_before_tool_call(
        mut self,
        hook: impl Fn(&BeforeToolCallContext) -> BeforeToolCallResult + Send + Sync + 'static,
    ) -> Self {
        self.before_tool_call = Some(Arc::new(move |ctx, _cancel| {
            let res = hook(&ctx);
            Box::pin(async move { res })
        }));
        self
    }

    pub fn with_after_tool_call(
        mut self,
        hook: impl Fn(&AfterToolCallContext) -> AfterToolCallResult + Send + Sync + 'static,
    ) -> Self {
        self.after_tool_call = Some(Arc::new(move |ctx, _cancel| {
            let res = hook(&ctx);
            Box::pin(async move { res })
        }));
        self
    }

    pub fn with_dynamic_tools(mut self, dt: DynamicToolsList) -> Self {
        self.dynamic_tools = Some(dt);
        self
    }

    pub fn with_max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_prepare_request(mut self, hook: PrepareRequestHook) -> Self {
        self.prepare_request = Some(hook);
        self
    }

    pub fn with_prepare_next_turn(mut self, hook: PrepareNextTurnHook) -> Self {
        self.prepare_next_turn = Some(hook);
        self
    }

    pub fn with_transform_context(mut self, hook: TransformContextHook) -> Self {
        self.transform_context = Some(hook);
        self
    }

    pub fn with_finish_turn(mut self, hook: FinishTurnHook) -> Self {
        self.finish_turn = Some(hook);
        self
    }

    pub fn set_system_prompt(&mut self, prompt: Option<String>) {
        mehh_ai::write_recover(&self.state).system_prompt = prompt.clone().unwrap_or_default();
        self.system_prompt = prompt;
    }

    pub fn set_model(&mut self, model: Model) {
        mehh_ai::write_recover(&self.state).model = model.clone();
        self.model = model;
    }

    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        mehh_ai::write_recover(&self.state).tools = tools.clone();
        self.tools = tools;
    }

    pub fn set_thinking_level(&mut self, level: Option<ThinkingLevel>) {
        mehh_ai::write_recover(&self.state).thinking_level = level;
        self.stream_options.reasoning = level;
    }

    pub fn set_before_tool_call(
        &mut self,
        hook: Option<BeforeToolCallHook>,
    ) {
        self.before_tool_call = hook;
    }

    pub fn set_after_tool_call(
        &mut self,
        hook: Option<AfterToolCallHook>,
    ) {
        self.after_tool_call = hook;
    }

    pub fn set_dynamic_tools(&mut self, dt: Option<DynamicToolsList>) {
        self.dynamic_tools = dt;
    }

    pub fn set_max_turns(&mut self, max_turns: Option<usize>) {
        self.max_turns = max_turns;
    }

    pub fn set_prepare_request(&mut self, hook: Option<PrepareRequestHook>) {
        self.prepare_request = hook;
    }

    pub fn set_prepare_next_turn(&mut self, hook: Option<PrepareNextTurnHook>) {
        self.prepare_next_turn = hook;
    }

    pub fn set_transform_context(&mut self, hook: Option<TransformContextHook>) {
        self.transform_context = hook;
    }

    pub fn set_finish_turn(&mut self, hook: Option<FinishTurnHook>) {
        self.finish_turn = hook;
    }

    pub fn state(&self) -> AgentState {
        mehh_ai::read_recover(&self.state).clone()
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&AgentState) -> R) -> R {
        f(&mehh_ai::read_recover(&self.state))
    }

    pub fn reset(&mut self) {
        self.clear_messages();
        let mut st = mehh_ai::write_recover(&self.state);
        st.messages.clear();
        st.is_streaming = false;
        st.streaming_message = None;
        st.pending_tool_calls.clear();
        st.error_message = None;
        mehh_ai::lock_recover(&self.steering_queue).clear();
        mehh_ai::lock_recover(&self.follow_up_queue).clear();
    }

    pub fn steer(&self, message: Message) {
        mehh_ai::lock_recover(&self.steering_queue).push_back(message);
    }

    pub fn follow_up(&self, message: Message) {
        mehh_ai::lock_recover(&self.follow_up_queue).push_back(message);
    }

    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
    }

    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.steering_mode
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode
    }

    pub fn subscribe(&mut self, listener: impl Fn(AgentEvent) + Send + Sync + 'static) -> usize {
        let id = self.listeners.last().map_or(0, |(id, _)| id + 1);
        self.listeners.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&mut self, id: usize) {
        self.listeners.retain(|(candidate, _)| *candidate != id);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        mehh_ai::write_recover(&self.state).messages.clear();
    }

    pub fn steering_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.steering_queue.clone()
    }

    pub fn follow_up_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.follow_up_queue.clone()
    }

    pub fn cancellation_token(&self) -> CancelHandle {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub async fn prompt_text(&mut self, text: impl Into<String>) -> Vec<Message> {
        let message = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        self.prompt(vec![message]).await
    }

    pub async fn prompt(&mut self, messages: Vec<Message>) -> Vec<Message> {
        self.run_turn(messages).await
    }

    pub async fn continue_turn(&mut self) -> Vec<Message> {
        self.run_turn(Vec::new()).await
    }

    async fn run_turn(&mut self, messages: Vec<Message>) -> Vec<Message> {
        self.cancel.reset_if_cancelled();

        {
            let mut st = mehh_ai::write_recover(&self.state);
            st.is_streaming = true;
            st.streaming_message = None;
            st.error_message = None;
        }

        let config = self.build_loop_config();
        let on_event = self.build_on_event();
        let mut guard = StreamingStateGuard {
            state: self.state.clone(),
            cancel: config.cancel.clone(),
            initial_len: self.messages.len(),
            completed: false,
        };

        let new_messages = run_agent_loop(
            messages,
            self.messages.clone(),
            config,
            on_event,
        )
        .await;

        guard.completed = true;
        drop(guard);

        self.messages.extend(new_messages.iter().cloned());
        new_messages
    }

    fn build_loop_config(&self) -> AgentLoopConfig {
        AgentLoopConfig {
            model: self.model.clone(),
            provider: self.provider.clone(),
            stream_options: self.stream_options.clone(),
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            dynamic_tools: self.dynamic_tools.clone(),
            max_turns: self.max_turns,
            steering_queue: self.steering_queue.clone(),
            follow_up_queue: self.follow_up_queue.clone(),
            steering_mode: self.steering_mode,
            follow_up_mode: self.follow_up_mode,
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
            prepare_request: self.prepare_request.clone(),
            prepare_next_turn: self.prepare_next_turn.clone(),
            transform_context: self.transform_context.clone(),
            finish_turn: self.finish_turn.clone(),
            cancel: self.cancel.current(),
        }
    }

    fn build_on_event(&self) -> Option<Arc<dyn Fn(AgentEvent) + Send + Sync>> {
        let state_lock = self.state.clone();
        let listeners: Vec<Arc<dyn Fn(AgentEvent) + Send + Sync>> =
            self.listeners.iter().map(|(_, listener)| listener.clone()).collect();
        Some(Arc::new(move |event: AgentEvent| {
            {
                let mut st = mehh_ai::write_recover(&state_lock);
                match &event {
                    AgentEvent::MessageStart { message } => {
                        st.streaming_message = Some(message.clone());
                    }
                    AgentEvent::MessageUpdate { message, .. } => {
                        st.streaming_message = Some(Message::Assistant(message.clone()));
                    }
                    AgentEvent::MessageEnd { message } => {
                        st.streaming_message = None;
                        st.messages.push(message.clone());
                    }
                    AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                        st.pending_tool_calls.insert(tool_call_id.clone());
                    }
                    AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        st.pending_tool_calls.remove(tool_call_id);
                    }
                    AgentEvent::TurnEnd { message, .. } => {
                        if let mehh_ai::StopReason::Error = message.stop_reason {
                            st.error_message = message.error_message.clone();
                        }
                    }
                    _ => {}
                }
            }
            if let Some((last, rest)) = listeners.split_last() {
                for listener in rest {
                    listener(event.clone());
                }
                last(event);
            }
        }))
    }
}

struct StreamingStateGuard {
    state: Arc<std::sync::RwLock<AgentState>>,
    cancel: CancellationToken,
    initial_len: usize,
    completed: bool,
}

impl Drop for StreamingStateGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.cancel.cancel();
        }
        let mut st = mehh_ai::write_recover(&self.state);
        if !self.completed {
            st.messages.truncate(self.initial_len);
        }
        st.is_streaming = false;
        st.streaming_message = None;
        st.pending_tool_calls.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use mehh_ai::{
        faux_assistant_message, faux_model, faux_tool_call, faux_tool_call_message,
        FauxProvider, StopReason, ToolResultContentBlock,
    };
    use crate::types::{
        AgentToolResult, AgentToolUpdateCallback,
    };

    struct EchoTool;
    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes input"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "required": ["input"]
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: serde_json::Map<String, serde_json::Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            let input = params.get("input").and_then(|v| v.as_str()).unwrap_or("");
            Ok(AgentToolResult::text(format!("echo: {}", input)))
        }
    }

    struct AddTool;
    #[async_trait::async_trait]
    impl AgentTool for AddTool {
        fn name(&self) -> &str {
            "add"
        }
        fn description(&self) -> &str {
            "Adds two integers"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                },
                "required": ["a", "b"]
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: serde_json::Map<String, serde_json::Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            let a = params.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
            let b = params.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(AgentToolResult::text(format!("{}", a + b)))
        }
    }

    struct SpyTool {
        executed: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl AgentTool for SpyTool {
        fn name(&self) -> &str {
            "spy"
        }
        fn description(&self) -> &str {
            "Spy tool that tracks execution"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                }
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            self.executed.store(true, Ordering::SeqCst);
            Ok(AgentToolResult::text("spy executed"))
        }
    }

    struct SteeringTriggerTool {
        steering_queue: Arc<Mutex<VecDeque<Message>>>,
    }
    #[async_trait::async_trait]
    impl AgentTool for SteeringTriggerTool {
        fn name(&self) -> &str {
            "trigger_steering"
        }
        fn description(&self) -> &str {
            "Injects a steering message during execution"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            self.steering_queue.lock().unwrap().push_back(Message::User(UserMessage {
                content: UserContent::Text("Steering: change course".to_string()),
                timestamp: now_ms(),
            }));
            Ok(AgentToolResult::text("tool step 1 completed"))
        }
    }

    // 1. Single-turn text prompt & response
    #[tokio::test]
    async fn test_single_turn_text() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("Hello from agent"));

        let mut agent = Agent::new(faux_model(), provider);
        let new_msgs = agent.prompt_text("Hi").await;

        assert_eq!(new_msgs.len(), 2);
        match &new_msgs[0] {
            Message::User(u) => assert_eq!(u.content.text(), "Hi"),
            _ => panic!("expected user message"),
        }
        match &new_msgs[1] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Hello from agent"),
            _ => panic!("expected assistant message"),
        }
        assert_eq!(agent.messages().len(), 2);
    }

    // 2. Multi-turn tool call -> tool execution -> tool result -> final response
    #[tokio::test]
    async fn test_multi_turn_tool_call() {
        let provider = Arc::new(FauxProvider::new());
        let call = faux_tool_call("call-1", "echo", serde_json::json!({ "input": "world" }));
        provider.add_response(faux_tool_call_message(vec![call]));
        provider.add_response(faux_assistant_message("Done echoing"));

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();

        let mut agent = Agent::new(faux_model(), provider)
            .with_tools(vec![Arc::new(EchoTool)]);
        agent.subscribe(move |e| {
            events_clone.lock().unwrap().push(e);
        });

        let new_msgs = agent.prompt_text("Echo world").await;
        assert_eq!(new_msgs.len(), 4);

        // [0]: User ("Echo world")
        // [1]: Assistant (ToolCall echo)
        // [2]: ToolResult ("echo: world")
        // [3]: Assistant ("Done echoing")
        match &new_msgs[2] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_name, "echo");
                assert!(!tr.is_error);
                match &tr.content[0] {
                    ToolResultContentBlock::Text(t) => assert_eq!(t.text, "echo: world"),
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected tool result"),
        }

        let recorded_events = events.lock().unwrap();
        assert!(recorded_events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. })));
        assert!(recorded_events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. })));
    }

    // 3. Invalid tool arguments fail validation and return is_error: true tool result to model
    #[tokio::test]
    async fn test_invalid_tool_arguments_fail_validation() {
        let provider = Arc::new(FauxProvider::new());
        // Missing required property 'b'
        let call = faux_tool_call("call-bad", "add", serde_json::json!({ "a": 10 }));
        provider.add_response(faux_tool_call_message(vec![call]));
        provider.add_response(faux_assistant_message("Noticed validation error"));

        let mut agent = Agent::new(faux_model(), provider)
            .with_tools(vec![Arc::new(AddTool)]);

        let new_msgs = agent.prompt_text("Add 10").await;
        assert_eq!(new_msgs.len(), 4);

        match &new_msgs[2] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_name, "add");
                assert!(tr.is_error);
                match &tr.content[0] {
                    ToolResultContentBlock::Text(t) => {
                        assert!(t.text.contains("Missing required property \"b\""));
                    }
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected tool result"),
        }
    }

    // 4. Truncated tool call (StopReason::Length) skips tool execution and returns error tool result
    #[tokio::test]
    async fn test_truncated_tool_call_skips_execution() {
        let executed = Arc::new(AtomicBool::new(false));
        let spy = Arc::new(SpyTool {
            executed: executed.clone(),
        });

        let provider = Arc::new(FauxProvider::new());
        let call = faux_tool_call("call-trunc", "spy", serde_json::json!({ "cmd": "run" }));
        let mut trunc_msg = faux_tool_call_message(vec![call]);
        trunc_msg.stop_reason = StopReason::Length;
        provider.add_response(trunc_msg);
        provider.add_response(faux_assistant_message("Recovered from truncated call"));

        let mut agent = Agent::new(faux_model(), provider)
            .with_tools(vec![spy]);

        let new_msgs = agent.prompt_text("Run spy").await;
        assert_eq!(new_msgs.len(), 4);

        // Tool execute() was never invoked
        assert!(!executed.load(Ordering::SeqCst));

        match &new_msgs[2] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_name, "spy");
                assert!(tr.is_error);
                match &tr.content[0] {
                    ToolResultContentBlock::Text(t) => {
                        assert!(t.text.contains("output token limit"));
                    }
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected tool result"),
        }
    }

    // 5. Steering message injected during tool execution is processed in next turn
    #[tokio::test]
    async fn test_steering_message_during_tool_execution() {
        let provider = Arc::new(FauxProvider::new());
        let call = faux_tool_call("call-steer", "trigger_steering", serde_json::json!({}));
        provider.add_response(faux_tool_call_message(vec![call]));
        provider.add_response(faux_assistant_message("Acknowledged steering"));

        let mut agent = Agent::new(faux_model(), provider);
        let tool = Arc::new(SteeringTriggerTool {
            steering_queue: agent.steering_queue(),
        });
        agent.set_tools(vec![tool]);

        let new_msgs = agent.prompt_text("Begin").await;
        assert_eq!(new_msgs.len(), 5);

        // [0]: User ("Begin")
        // [1]: Assistant (ToolCall trigger_steering)
        // [2]: ToolResult ("tool step 1 completed")
        // [3]: User ("Steering: change course")
        // [4]: Assistant ("Acknowledged steering")
        match &new_msgs[3] {
            Message::User(u) => assert_eq!(u.content.text(), "Steering: change course"),
            _ => panic!("expected steering message at index 3"),
        }
        match &new_msgs[4] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Acknowledged steering"),
            _ => panic!("expected assistant response at index 4"),
        }
    }

    // 6. Follow-up message queued is processed after main turn completes
    #[tokio::test]
    async fn test_follow_up_message_processed_after_main_turn() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("Main turn complete"));
        provider.add_response(faux_assistant_message("Follow-up turn complete"));

        let mut agent = Agent::new(faux_model(), provider);

        agent.follow_up(Message::User(UserMessage {
            content: UserContent::Text("Follow-up prompt".to_string()),
            timestamp: now_ms(),
        }));

        let new_msgs = agent.prompt_text("Main prompt").await;
        assert_eq!(new_msgs.len(), 4);

        match &new_msgs[0] {
            Message::User(u) => assert_eq!(u.content.text(), "Main prompt"),
            _ => panic!("expected main prompt"),
        }
        match &new_msgs[1] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Main turn complete"),
            _ => panic!("expected assistant response 1"),
        }
        match &new_msgs[2] {
            Message::User(u) => assert_eq!(u.content.text(), "Follow-up prompt"),
            _ => panic!("expected follow-up prompt"),
        }
        match &new_msgs[3] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Follow-up turn complete"),
            _ => panic!("expected assistant response 2"),
        }
    }

    // 7. Test QueueMode::OneAtATime vs QueueMode::All for follow-up and steering
    #[tokio::test]
    async fn test_follow_up_queue_mode_all_vs_one_at_a_time() {
        // QueueMode::All: drains both follow-ups in a single follow-up turn
        let provider_all = Arc::new(FauxProvider::new());
        provider_all.add_response(faux_assistant_message("Main turn complete"));
        provider_all.add_response(faux_assistant_message("Both follow-ups processed"));

        let mut agent_all = Agent::new(faux_model(), provider_all);
        agent_all.set_follow_up_mode(QueueMode::All);
        assert_eq!(agent_all.follow_up_mode(), QueueMode::All);

        agent_all.follow_up(Message::User(UserMessage {
            content: UserContent::Text("Follow-up 1".to_string()),
            timestamp: now_ms(),
        }));
        agent_all.follow_up(Message::User(UserMessage {
            content: UserContent::Text("Follow-up 2".to_string()),
            timestamp: now_ms(),
        }));

        let new_msgs_all = agent_all.prompt_text("Main prompt").await;
        // Expected: User(Main), Assistant(Main), User(F1), User(F2), Assistant(Both) => 5 messages (2 turns)
        assert_eq!(new_msgs_all.len(), 5);
        match &new_msgs_all[4] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Both follow-ups processed"),
            _ => panic!("expected assistant response"),
        }

        // QueueMode::OneAtATime: drains one follow-up per turn (requiring 3 turns total)
        let provider_one = Arc::new(FauxProvider::new());
        provider_one.add_response(faux_assistant_message("Main turn complete"));
        provider_one.add_response(faux_assistant_message("Follow-up 1 complete"));
        provider_one.add_response(faux_assistant_message("Follow-up 2 complete"));

        let mut agent_one = Agent::new(faux_model(), provider_one);
        agent_one.set_follow_up_mode(QueueMode::OneAtATime);
        assert_eq!(agent_one.follow_up_mode(), QueueMode::OneAtATime);

        agent_one.follow_up(Message::User(UserMessage {
            content: UserContent::Text("Follow-up 1".to_string()),
            timestamp: now_ms(),
        }));
        agent_one.follow_up(Message::User(UserMessage {
            content: UserContent::Text("Follow-up 2".to_string()),
            timestamp: now_ms(),
        }));

        let new_msgs_one = agent_one.prompt_text("Main prompt").await;
        // Expected: User(Main), Assistant(Main), User(F1), Assistant(F1), User(F2), Assistant(F2) => 6 messages (3 turns)
        assert_eq!(new_msgs_one.len(), 6);
        match &new_msgs_one[3] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Follow-up 1 complete"),
            _ => panic!("expected assistant response 1"),
        }
        match &new_msgs_one[5] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Follow-up 2 complete"),
            _ => panic!("expected assistant response 2"),
        }
    }

    #[tokio::test]
    async fn test_steering_queue_mode_all_vs_one_at_a_time() {
        // QueueMode::All: initial steering messages are all drained at start of turn
        let provider_all = Arc::new(FauxProvider::new());
        provider_all.add_response(faux_assistant_message("Response to main and both steerings"));

        let mut agent_all = Agent::new(faux_model(), provider_all);
        agent_all.set_steering_mode(QueueMode::All);
        assert_eq!(agent_all.steering_mode(), QueueMode::All);

        agent_all.steer(Message::User(UserMessage {
            content: UserContent::Text("Steer 1".to_string()),
            timestamp: now_ms(),
        }));
        agent_all.steer(Message::User(UserMessage {
            content: UserContent::Text("Steer 2".to_string()),
            timestamp: now_ms(),
        }));

        let new_msgs_all = agent_all.prompt_text("Main prompt").await;
        // User(Main), User(Steer 1), User(Steer 2), Assistant(...) => 4 messages (1 turn)
        assert_eq!(new_msgs_all.len(), 4);
        assert!(agent_all.steering_queue().lock().unwrap().is_empty());

        // QueueMode::OneAtATime: only first steering message is popped before turn 1, then second is popped after turn 1
        let provider_one = Arc::new(FauxProvider::new());
        provider_one.add_response(faux_assistant_message("Turn 1 response"));
        provider_one.add_response(faux_assistant_message("Turn 2 response"));

        let mut agent_one = Agent::new(faux_model(), provider_one);
        agent_one.set_steering_mode(QueueMode::OneAtATime);
        assert_eq!(agent_one.steering_mode(), QueueMode::OneAtATime);

        agent_one.steer(Message::User(UserMessage {
            content: UserContent::Text("Steer 1".to_string()),
            timestamp: now_ms(),
        }));
        agent_one.steer(Message::User(UserMessage {
            content: UserContent::Text("Steer 2".to_string()),
            timestamp: now_ms(),
        }));

        let new_msgs_one = agent_one.prompt_text("Main prompt").await;
        // User(Main), User(Steer 1), Assistant(Turn 1), User(Steer 2), Assistant(Turn 2) => 5 messages (2 turns)
        assert_eq!(new_msgs_one.len(), 5);
        assert!(agent_one.steering_queue().lock().unwrap().is_empty());
    }

    // 8. Test before_tool_call and after_tool_call hooks
    #[tokio::test]
    async fn test_tool_hooks() {
        let provider = Arc::new(FauxProvider::new());
        let call = faux_tool_call("call-1", "echo", serde_json::json!({ "input": "secret" }));
        provider.add_response(faux_tool_call_message(vec![call]));
        provider.add_response(faux_assistant_message("Handled blocked call"));

        let mut agent = Agent::new(faux_model(), provider)
            .with_tools(vec![Arc::new(EchoTool)])
            .with_before_tool_call(|ctx| {
                if ctx.tool_call.name == "echo" {
                    BeforeToolCallResult {
                        block: true,
                        reason: Some("Echoing secret is blocked".to_string()),
                        ..Default::default()
                    }
                } else {
                    BeforeToolCallResult::default()
                }
            });

        let new_msgs = agent.prompt_text("Echo secret").await;
        assert_eq!(new_msgs.len(), 4);

        match &new_msgs[2] {
            Message::ToolResult(tr) => {
                assert!(tr.is_error);
                match &tr.content[0] {
                    ToolResultContentBlock::Text(t) => {
                        assert_eq!(t.text, "Echoing secret is blocked");
                    }
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected tool result"),
        }
    }

    #[tokio::test]
    async fn agent_state_tracks_streaming_pending_tools_and_reset() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![
            faux_tool_call("c1", "echo", serde_json::json!({ "input": "hello" })),
        ]));
        provider.add_response(faux_assistant_message("Done echoing"));

        let mut agent = Agent::new(faux_model(), provider)
            .with_system_prompt("You are a helpful test agent")
            .with_tools(vec![Arc::new(EchoTool)]);

        let initial_state = agent.state();
        assert_eq!(initial_state.system_prompt, "You are a helpful test agent");
        assert_eq!(initial_state.tools.len(), 1);
        assert!(!initial_state.is_streaming);
        assert!(initial_state.streaming_message.is_none());
        assert!(initial_state.pending_tool_calls.is_empty());
        assert!(initial_state.error_message.is_none());

        let out = agent.prompt_text("Say hi").await;
        assert_eq!(out.len(), 4);

        let post_state = agent.state();
        assert!(!post_state.is_streaming);
        assert!(post_state.streaming_message.is_none());
        assert!(post_state.pending_tool_calls.is_empty());
        assert_eq!(post_state.messages.len(), 4);

        agent.reset();
        let reset_state = agent.state();
        assert!(reset_state.messages.is_empty());
        assert!(!reset_state.is_streaming);
        assert!(reset_state.streaming_message.is_none());
        assert!(reset_state.pending_tool_calls.is_empty());
        assert!(reset_state.error_message.is_none());
    }

    #[tokio::test]
    async fn agent_builder_hooks_and_max_turns() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("Hello from model"));

        let transform_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tc_flag = transform_called.clone();

        let mut agent = Agent::new(faux_model(), provider)
            .with_max_turns(Some(5))
            .with_transform_context(Arc::new(move |messages, _cancel| {
                tc_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move { messages })
            }));

        let msgs = agent.prompt_text("test prompt").await;
        assert_eq!(msgs.len(), 2);
        assert!(transform_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_handle_works_after_a_cancelled_turn() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("first"));
        provider.add_response(faux_assistant_message("second"));
        let mut agent = Agent::new(faux_model(), provider);
        let handle = agent.cancellation_token();

        let first = agent.prompt_text("first").await;
        assert!(matches!(&first[1], Message::Assistant(a) if a.stop_reason == StopReason::Stop));

        handle.cancel();

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let handle_clone = handle.clone();
        agent.subscribe(move |event| {
            if matches!(event, AgentEvent::AgentStart)
                && !fired_clone.swap(true, Ordering::SeqCst)
            {
                handle_clone.cancel();
            }
        });

        let second = agent.prompt_text("second").await;
        assert!(fired.load(Ordering::SeqCst));
        assert!(
            !second.iter().any(|m| matches!(m, Message::Assistant(_))),
            "cancelled turn must not produce a fresh assistant reply"
        );
    }

    #[test]
    fn with_state_exposes_borrowed_state() {
        let agent = Agent::new(faux_model(), Arc::new(FauxProvider::new()))
            .with_system_prompt("hello");
        assert_eq!(agent.with_state(|state| state.system_prompt.len()), 5);
    }

    #[tokio::test]
    async fn unsubscribe_stops_listener_events() {
        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_assistant_message("hi"));
        let mut agent = Agent::new(faux_model(), provider);
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = received.clone();
        let id = agent.subscribe(move |_event| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        agent.unsubscribe(id);

        let _ = agent.prompt_text("hi").await;
        assert_eq!(received.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropping_mid_flight_prompt_resets_streaming_state_and_cancels() {
        let started = Arc::new(tokio::sync::Notify::new());

        struct HangingTool {
            started: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl AgentTool for HangingTool {
            fn name(&self) -> &str {
                "hang"
            }
            fn description(&self) -> &str {
                "Waits until cancelled"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _tool_call_id: &str,
                _params: serde_json::Map<String, serde_json::Value>,
                cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                self.started.notify_one();
                cancel.cancelled().await;
                Err("cancelled".to_string())
            }
        }

        let provider = Arc::new(FauxProvider::new());
        provider.add_response(faux_tool_call_message(vec![faux_tool_call(
            "call-hang",
            "hang",
            serde_json::json!({}),
        )]));

        let mut agent = Agent::new(faux_model(), provider).with_tools(vec![Arc::new(HangingTool {
            started: started.clone(),
        })]);

        let saw_tool_start = Arc::new(AtomicBool::new(false));
        let saw_tool_start_clone = saw_tool_start.clone();
        agent.subscribe(move |ev| {
            if matches!(ev, AgentEvent::ToolExecutionStart { tool_call_id, .. } if tool_call_id == "call-hang") {
                saw_tool_start_clone.store(true, Ordering::SeqCst);
            }
        });

        let cancel_handle = agent.cancellation_token();
        {
            let mut prompt_fut = Box::pin(agent.prompt_text("start"));
            tokio::select! {
                _ = &mut prompt_fut => panic!("hanging tool prompt should not finish before drop"),
                _ = started.notified() => {}
            }
        }

        assert!(saw_tool_start.load(Ordering::SeqCst));
        assert!(cancel_handle.is_cancelled());
        let st = agent.state();
        assert!(!st.is_streaming, "is_streaming must reset on drop");
        assert!(st.streaming_message.is_none(), "streaming_message must clear on drop");
        assert!(st.pending_tool_calls.is_empty(), "pending_tool_calls must clear on drop");
        assert_eq!(
            st.messages.len(),
            agent.messages().len(),
            "state.messages must stay synchronized with agent.messages on drop"
        );
    }
}
