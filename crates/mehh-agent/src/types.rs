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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolResult {
    pub content: Vec<ToolResultContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl AgentToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: s.into(),
                text_signature: None,
            })],
            details: None,
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

#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: ToolCall,
    pub args: serde_json::Map<String, serde_json::Value>,
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
}

#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ToolResultContentBlock>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub terminate: bool,
}

pub type BeforeToolCallHook = Arc<dyn Fn(&BeforeToolCallContext) -> BeforeToolCallResult + Send + Sync>;
pub type AfterToolCallHook = Arc<dyn Fn(&AfterToolCallContext) -> AfterToolCallResult + Send + Sync>;

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
    },
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
