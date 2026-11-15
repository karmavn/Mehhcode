use std::collections::HashMap;
use std::time::SystemTime;
use serde::{Deserialize, Serialize};

/// Current unix timestamp in milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
    Pending,
}

/// Serializes an AssistantMessage ensuring "role": "assistant" is included.
pub fn assistant_message_to_json(msg: &AssistantMessage) -> serde_json::Value {
    let mut val = serde_json::to_value(msg).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(ref mut map) = val {
        if !map.contains_key("role") {
            map.insert("role".to_string(), serde_json::Value::String("assistant".to_string()));
        }
    }
    val
}

/// Serde serializer helper for AssistantMessage including "role": "assistant".
pub fn serialize_assistant_with_role<S>(msg: &AssistantMessage, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let val = assistant_message_to_json(msg);
    val.serialize(serializer)
}

/// Serializes a ToolCall ensuring "type": "toolCall" is included.
pub fn tool_call_to_json(tc: &ToolCall) -> serde_json::Value {
    let mut val = serde_json::to_value(tc).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(ref mut map) = val {
        if !map.contains_key("type") {
            map.insert("type".to_string(), serde_json::Value::String("toolCall".to_string()));
        }
    }
    val
}

/// Serde serializer helper for ToolCall including "type": "toolCall".
pub fn serialize_tool_call_with_type<S>(tc: &ToolCall, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let val = tool_call_to_json(tc);
    val.serialize(serializer)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

impl Usage {
    pub fn calculate_cost(&mut self, cost: &ModelCost) {
        self.cost.input = (cost.input / 1_000_000.0) * self.input as f64;
        self.cost.output = (cost.output / 1_000_000.0) * self.output as f64;
        self.cost.cache_read = (cost.cache_read / 1_000_000.0) * self.cache_read as f64;
        self.cost.cache_write = (cost.cache_write / 1_000_000.0) * self.cache_write as f64;
        self.cost.total =
            self.cost.input + self.cost.output + self.cost.cache_read + self.cost.cache_write;
        if self.total_tokens == 0 {
            self.total_tokens = self.input + self.output + self.cache_read + self.cache_write;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

impl UserContent {
    pub fn text(&self) -> String {
        match self {
            UserContent::Text(s) => s.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    UserContentBlock::Text(t) => Some(t.text.as_str()),
                    UserContentBlock::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub content: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<AssistantContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantMessage {
    pub fn empty_for_model(model: &Model) -> Self {
        Self {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        }
    }

    /// Clones metadata and usage without cloning accumulated content blocks.
    pub fn metadata_clone(&self) -> Self {
        Self {
            content: Vec::new(),
            api: self.api.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            response_id: self.response_id.clone(),
            usage: self.usage.clone(),
            stop_reason: self.stop_reason,
            error_message: self.error_message.clone(),
            timestamp: self.timestamp,
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                AssistantContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                AssistantContentBlock::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_mid_convo_effort: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_tools: Option<bool>,
}

pub type ThinkingLevelMap = HashMap<String, Option<String>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// Resolves the cache retention policy from an explicit option and an optional environment value.
/// Explicit takes priority. When explicit is None, "none" resolves to `CacheRetention::None`,
/// "long" resolves to `CacheRetention::Long`, and any other value (or None) resolves to `CacheRetention::Short`.
pub fn resolve_cache_retention_from(
    explicit: Option<CacheRetention>,
    env_value: Option<&str>,
) -> CacheRetention {
    if let Some(explicit) = explicit {
        return explicit;
    }
    match env_value {
        Some(v) if v.eq_ignore_ascii_case("none") => CacheRetention::None,
        Some(v) if v.eq_ignore_ascii_case("long") => CacheRetention::Long,
        _ => CacheRetention::Short,
    }
}

/// Resolves the cache retention policy from an explicit option and the `PI_CACHE_RETENTION` environment variable.
pub fn resolve_cache_retention(explicit: Option<CacheRetention>) -> CacheRetention {
    let env_val = std::env::var("PI_CACHE_RETENTION").ok();
    resolve_cache_retention_from(explicit, env_val.as_deref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ModelCompat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<AnthropicEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved_thinking: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantMessageEvent {
    #[serde(rename = "start")]
    Start {
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall", serialize_with = "serialize_tool_call_with_type")]
        tool_call: ToolCall,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        partial: AssistantMessage,
    },
    #[serde(rename = "done")]
    Done {
        reason: StopReason,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        message: AssistantMessage,
    },
    #[serde(rename = "error")]
    Error {
        reason: StopReason,
        #[serde(serialize_with = "serialize_assistant_with_role")]
        error: AssistantMessage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thinking_level_serde() {
        let serialized = serde_json::to_string(&ThinkingLevel::Xhigh).unwrap();
        assert_eq!(serialized, "\"xhigh\"");
        let deserialized: ThinkingLevel = serde_json::from_str("\"minimal\"").unwrap();
        assert_eq!(deserialized, ThinkingLevel::Minimal);
    }

    #[test]
    fn test_stop_reason_serde() {
        let serialized = serde_json::to_string(&StopReason::ToolUse).unwrap();
        assert_eq!(serialized, "\"toolUse\"");
        let deserialized: StopReason = serde_json::from_str("\"toolUse\"").unwrap();
        assert_eq!(deserialized, StopReason::ToolUse);

        let serialized_pending = serde_json::to_string(&StopReason::Pending).unwrap();
        assert_eq!(serialized_pending, "\"pending\"");
        let deserialized_pending: StopReason = serde_json::from_str("\"pending\"").unwrap();
        assert_eq!(deserialized_pending, StopReason::Pending);
    }

    #[test]
    fn test_calculate_cost() {
        let mut usage = Usage {
            input: 1_000_000,
            output: 500_000,
            cache_read: 200_000,
            cache_write: 100_000,
            total_tokens: 0,
            cost: UsageCost::default(),
        };
        let cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        };
        usage.calculate_cost(&cost);
        assert!((usage.cost.input - 3.0).abs() < 1e-6);
        assert!((usage.cost.output - 7.5).abs() < 1e-6);
        assert!((usage.cost.cache_read - 0.06).abs() < 1e-6);
        assert!((usage.cost.cache_write - 0.375).abs() < 1e-6);
        assert!((usage.cost.total - 10.935).abs() < 1e-6);
        assert_eq!(usage.total_tokens, 1_800_000);
    }

    #[test]
    fn test_assistant_message_helpers() {
        let model = Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            ..Default::default()
        };
        let mut msg = AssistantMessage::empty_for_model(&model);
        assert_eq!(msg.model, "test-model");
        assert_eq!(msg.text_content(), "");
        assert_eq!(msg.tool_calls().len(), 0);

        msg.content.push(AssistantContentBlock::Text(TextContent {
            text: "Hello world".to_string(),
            text_signature: None,
        }));
        msg.content.push(AssistantContentBlock::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: serde_json::Map::new(),
            arguments_error: None,
            thought_signature: None,
        }));

        assert_eq!(msg.text_content(), "Hello world");
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn test_message_serde() {
        let user_msg = Message::User(UserMessage {
            content: UserContent::Text("ping".to_string()),
            timestamp: 12345,
        });
        let serialized = serde_json::to_string(&user_msg).unwrap();
        assert!(serialized.contains("\"role\":\"user\""));

        let deserialized: Message = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            Message::User(u) => assert_eq!(u.content.text(), "ping"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_assistant_message_event_serialization_role_and_type() {
        let model = Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
            headers: None,
            ..Default::default()
        };
        let msg = AssistantMessage::empty_for_model(&model);
        let start_ev = AssistantMessageEvent::Start {
            partial: msg.clone(),
        };
        let start_val = serde_json::to_value(&start_ev).unwrap();
        assert_eq!(start_val["type"], "start");
        assert_eq!(start_val["partial"]["role"], "assistant");

        let tc = ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: serde_json::Map::new(),
            arguments_error: None,
            thought_signature: None,
        };
        let tool_end_ev = AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: tc,
            partial: msg.clone(),
        };
        let tool_end_val = serde_json::to_value(&tool_end_ev).unwrap();
        assert_eq!(tool_end_val["type"], "toolcall_end");
        assert_eq!(tool_end_val["toolCall"]["type"], "toolCall");
        assert_eq!(tool_end_val["toolCall"]["name"], "bash");
        assert_eq!(tool_end_val["partial"]["role"], "assistant");

        // Message::Assistant round-trip and verify no duplicate role keys
        let asst_msg = Message::Assistant(msg);
        let serialized = serde_json::to_string(&asst_msg).unwrap();
        let role_matches: Vec<_> = serialized.match_indices("\"role\":").collect();
        assert_eq!(role_matches.len(), 1, "Duplicate role key found in: {serialized}");
        let deserialized: Message = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            Message::Assistant(a) => assert_eq!(a.model, "test-model"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn model_and_stream_options_default_new_optional_fields() {
        // Deserializing existing JSON without compat or cacheRetention succeeds with None
        let model_json = r#"{
            "id": "test-1",
            "name": "Test 1",
            "api": "test",
            "provider": "test",
            "baseUrl": "https://example.com",
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
            "contextWindow": 1000,
            "maxTokens": 100
        }"#;
        let model: Model = serde_json::from_str(model_json).unwrap();
        assert_eq!(model.compat, None);
        assert_eq!(model.thinking_level_map, None);

        // Serializing skips None fields
        let serialized_model = serde_json::to_string(&model).unwrap();
        assert!(!serialized_model.contains("compat"));
        assert!(!serialized_model.contains("thinkingLevelMap"));

        // StreamOptions deserializing without new fields succeeds with None
        let opts_json = r#"{
            "apiKey": "test-key"
        }"#;
        let opts: StreamOptions = serde_json::from_str(opts_json).unwrap();
        assert_eq!(opts.cache_retention, None);
        assert_eq!(opts.thinking_budgets, None);
        assert_eq!(opts.thinking_budget_tokens, None);
        assert_eq!(opts.effort, None);
        assert_eq!(opts.thinking_display, None);
        assert_eq!(opts.interleaved_thinking, None);

        // Serializing StreamOptions skips None fields
        let serialized_opts = serde_json::to_string(&opts).unwrap();
        assert!(!serialized_opts.contains("cacheRetention"));
        assert!(!serialized_opts.contains("thinkingBudgets"));
        assert!(!serialized_opts.contains("thinkingBudgetTokens"));
        assert!(!serialized_opts.contains("effort"));
        assert!(!serialized_opts.contains("thinkingDisplay"));
        assert!(!serialized_opts.contains("interleavedThinking"));

        // Round-trip with all new fields populated
        let full_opts = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            thinking_budgets: Some(ThinkingBudgets {
                minimal: Some(100),
                low: Some(500),
                medium: Some(1000),
                high: Some(2000),
            }),
            thinking_budget_tokens: Some(4096),
            effort: Some(AnthropicEffort::High),
            thinking_display: Some(AnthropicThinkingDisplay::Summarized),
            interleaved_thinking: Some(true),
            ..Default::default()
        };
        let full_json = serde_json::to_string(&full_opts).unwrap();
        assert!(full_json.contains(r#""cacheRetention":"long""#));
        assert!(full_json.contains(r#""effort":"high""#));
        assert!(full_json.contains(r#""thinkingDisplay":"summarized""#));
        assert!(full_json.contains(r#""interleavedThinking":true"#));

        let deserialized_full: StreamOptions = serde_json::from_str(&full_json).unwrap();
        assert_eq!(deserialized_full, full_opts);
    }

    #[test]
    fn test_resolve_cache_retention_from() {
        // Explicit overrides env_value
        assert_eq!(
            resolve_cache_retention_from(Some(CacheRetention::None), Some("long")),
            CacheRetention::None
        );
        assert_eq!(
            resolve_cache_retention_from(Some(CacheRetention::Long), Some("none")),
            CacheRetention::Long
        );
        assert_eq!(
            resolve_cache_retention_from(Some(CacheRetention::Short), Some("none")),
            CacheRetention::Short
        );

        // Env value takes effect when explicit is None
        assert_eq!(
            resolve_cache_retention_from(None, Some("none")),
            CacheRetention::None
        );
        assert_eq!(
            resolve_cache_retention_from(None, Some("NONE")),
            CacheRetention::None
        );
        assert_eq!(
            resolve_cache_retention_from(None, Some("long")),
            CacheRetention::Long
        );
        assert_eq!(
            resolve_cache_retention_from(None, Some("LONG")),
            CacheRetention::Long
        );
        assert_eq!(
            resolve_cache_retention_from(None, Some("short")),
            CacheRetention::Short
        );

        // Default when unset or unknown is Short
        assert_eq!(
            resolve_cache_retention_from(None, None),
            CacheRetention::Short
        );
        assert_eq!(
            resolve_cache_retention_from(None, Some("invalid")),
            CacheRetention::Short
        );
    }
}
