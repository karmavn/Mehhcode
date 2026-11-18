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
    pub thinking_level: Option<mehh_ai::ThinkingLevel>,
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
    pub thinking_level: Option<mehh_ai::ThinkingLevel>,
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
