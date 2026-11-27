use std::collections::{HashMap, HashSet};
use crate::types::{
    AssistantContentBlock, AssistantMessage, ImageContent, Message, Model, StopReason,
    TextContent, ToolResultContentBlock, ToolResultMessage, UserContent,
    UserContentBlock, UserMessage, now_ms,
};

pub const NON_VISION_USER_IMAGE_PLACEHOLDER: &str =
    "(image omitted: model does not support images)";
pub const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
pub const ERROR_TOOL_IMAGE_PLACEHOLDER: &str =
    "(image attached after the tool results)";

#[derive(Debug, Clone, PartialEq)]
pub struct SplitErrorToolResult {
    pub content: Vec<ToolResultContentBlock>,
    pub caption: String,
    pub images: Vec<ImageContent>,
}

trait ImageBlock: Sized + Clone {
    fn from_text(text: String) -> Self;
    fn as_text(&self) -> Option<&str>;
    fn is_image(&self) -> bool;
}

impl ImageBlock for UserContentBlock {
    fn from_text(text: String) -> Self {
        UserContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })
    }
    fn as_text(&self) -> Option<&str> {
        match self {
            UserContentBlock::Text(t) => Some(&t.text),
            UserContentBlock::Image(_) => None,
        }
    }
    fn is_image(&self) -> bool {
        matches!(self, UserContentBlock::Image(_))
    }
}

impl ImageBlock for ToolResultContentBlock {
    fn from_text(text: String) -> Self {
        ToolResultContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })
    }
    fn as_text(&self) -> Option<&str> {
        match self {
            ToolResultContentBlock::Text(t) => Some(&t.text),
            ToolResultContentBlock::Image(_) => None,
        }
    }
    fn is_image(&self) -> bool {
        matches!(self, ToolResultContentBlock::Image(_))
    }
}

fn replace_images_with_placeholder<T: ImageBlock>(content: &[T], placeholder: &str) -> Vec<T> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;

    for block in content {
        if block.is_image() {
            if !previous_was_placeholder {
                result.push(T::from_text(placeholder.to_string()));
            }
            previous_was_placeholder = true;
            continue;
        }

        result.push(block.clone());
        previous_was_placeholder = block.as_text() == Some(placeholder);
    }

    result
}

/// Splits the images out of an error tool result, for APIs that reject images in error results
/// (Anthropic, and Claude on Bedrock). Returns None if not an error or if there are no images.
pub fn split_error_tool_result_images(msg: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !msg.is_error {
        return None;
    }

    let images: Vec<ImageContent> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ToolResultContentBlock::Image(img) => Some(img.clone()),
            ToolResultContentBlock::Text(_) => None,
        })
        .collect();

    if images.is_empty() {
        return None;
    }

    Some(SplitErrorToolResult {
        content: replace_images_with_placeholder(&msg.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
        caption: format!(
            "Images from the error result of tool call {} ({}):",
            msg.tool_call_id, msg.tool_name
        ),
        images,
    })
}

/// Fast deterministic hash matching TypeScript `shortHash` in `packages/ai/src/utils/hash.ts`.
pub fn short_hash(s: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for ch in s.encode_utf16() {
        h1 = (h1 ^ (ch as u32)).wrapping_mul(2654435761);
        h2 = (h2 ^ (ch as u32)).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", to_base36(h2), to_base36(h1))
}

fn to_base36(mut val: u32) -> String {
    if val == 0 {
        return "0".to_string();
    }
    const RADIX: u32 = 36;
    let mut digits = Vec::new();
    while val > 0 {
        let rem = val % RADIX;
        let c = if rem < 10 {
            (b'0' + rem as u8) as char
        } else {
            (b'a' + (rem - 10) as u8) as char
        };
        digits.push(c);
        val /= RADIX;
    }
    digits.into_iter().rev().collect()
}

/// Sanitizes a tool call ID to `[A-Za-z0-9_-]` and caps its length to `max_len`.
/// When truncation is required, appends a deterministic `_<hash>` suffix derived
/// from the original ID so distinct long IDs sharing a prefix do not collide.
pub(crate) fn sanitize_and_cap_tool_call_id(id: &str, max_len: usize) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= max_len {
        return sanitized;
    }
    let full_hash = short_hash(id);
    let hash = &full_hash[..full_hash.len().min(8)];
    if max_len <= hash.len() + 1 {
        return hash[..max_len.min(hash.len())].to_string();
    }
    let prefix_len = max_len - hash.len() - 1;
    format!("{}_{}", &sanitized[..prefix_len], hash)
}

pub fn transform_messages<F>(
    messages: &[Message],
    model: &Model,
    mut normalize_tool_call_id: Option<F>,
) -> Vec<Message>
where
    F: FnMut(&str, &Model, &AssistantMessage) -> String,
{
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let supports_images = model.input.iter().any(|i| i == "image");

    // First pass: transform messages (unsupported image downgrade, thinking blocks, tool call ID normalization)
    let mut transformed = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::System(_) => {
                transformed.push(msg.clone());
            }
            Message::User(u) => {
                if !supports_images {
                    if let UserContent::Blocks(ref blocks) = u.content {
                        transformed.push(Message::User(UserMessage {
                            content: UserContent::Blocks(replace_images_with_placeholder(
                                blocks,
                                NON_VISION_USER_IMAGE_PLACEHOLDER,
                            )),
                            timestamp: u.timestamp,
                        }));
                        continue;
                    }
                }
                transformed.push(Message::User(u.clone()));
            }
            Message::ToolResult(tr) => {
                let content = if supports_images {
                    tr.content.clone()
                } else {
                    replace_images_with_placeholder(
                        &tr.content,
                        NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                    )
                };
                let tool_call_id = tool_call_id_map
                    .get(&tr.tool_call_id)
                    .filter(|normalized_id| *normalized_id != &tr.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| tr.tool_call_id.clone());
                transformed.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id,
                    tool_name: tr.tool_name.clone(),
                    content,
                    details: tr.details.clone(),
                    is_error: tr.is_error,
                    timestamp: tr.timestamp,
                    duration_ms: tr.duration_ms,
                    usage: tr.usage.clone(),
                }));
            }
            Message::Assistant(assistant_msg) => {
                let is_same_model = assistant_msg.provider == model.provider
                    && assistant_msg.api == model.api
                    && assistant_msg.model == model.id;
                let is_errored_or_aborted = matches!(
                    assistant_msg.stop_reason,
                    StopReason::Error | StopReason::Aborted
                );

                let mut transformed_content = Vec::with_capacity(assistant_msg.content.len());
                for block in &assistant_msg.content {
                    match block {
                        AssistantContentBlock::Thinking(thinking) => {
                            if !is_same_model {
                                continue;
                            }
                            if thinking.redacted == Some(true)
                                || thinking.thinking_signature.is_some()
                                || !thinking.thinking.trim().is_empty()
                            {
                                transformed_content
                                    .push(AssistantContentBlock::Thinking(thinking.clone()));
                            }
                        }
                        AssistantContentBlock::Text(text) => {
                            if is_same_model {
                                transformed_content.push(AssistantContentBlock::Text(text.clone()));
                            } else {
                                transformed_content.push(AssistantContentBlock::Text(TextContent {
                                    text: text.text.clone(),
                                    text_signature: None,
                                }));
                            }
                        }
                        AssistantContentBlock::ToolCall(tool_call) => {
                            let mut tool_call = tool_call.clone();
                            if !is_same_model {
                                if tool_call.thought_signature.is_some() {
                                    tool_call.thought_signature = None;
                                }
                                if !is_errored_or_aborted {
                                    if let Some(ref mut normalize) = normalize_tool_call_id {
                                        let normalized_id =
                                            normalize(&tool_call.id, model, assistant_msg);
                                        if normalized_id != tool_call.id {
                                            tool_call_id_map
                                                .insert(tool_call.id.clone(), normalized_id.clone());
                                            tool_call.id = normalized_id;
                                        }
                                    }
                                }
                            }
                            transformed_content.push(AssistantContentBlock::ToolCall(tool_call));
                        }
                    }
                }

                if !is_same_model && transformed_content.is_empty() {
                    continue;
                }
                let mut assistant_out = assistant_msg.metadata_clone();
                assistant_out.content = transformed_content;
                transformed.push(Message::Assistant(assistant_out));
            }
        }
    }

    // Second pass: insert synthetic empty tool results for orphaned tool calls
    let mut result = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new();
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();
    let mut held_system_messages: Vec<Message> = Vec::new();

    let close_pending_tool_calls = |result: &mut Vec<Message>,
                                        pending_tool_calls: &mut Vec<(String, String)>,
                                        existing_tool_result_ids: &mut HashSet<String>,
                                        held_system_messages: &mut Vec<Message>| {
        if !pending_tool_calls.is_empty() {
            for (tc_id, tc_name) in pending_tool_calls.iter() {
                if !existing_tool_result_ids.contains(tc_id) {
                    result.push(Message::ToolResult(ToolResultMessage {
                        tool_call_id: tc_id.clone(),
                        tool_name: tc_name.clone(),
                        content: vec![ToolResultContentBlock::Text(TextContent {
                            text: "No result provided".to_string(),
                            text_signature: None,
                        })],
                        details: None,
                        is_error: true,
                        timestamp: now_ms(),
                        duration_ms: None,
                        usage: None,
                    }));
                }
            }
            pending_tool_calls.clear();
            existing_tool_result_ids.clear();
        }
        result.append(held_system_messages);
    };

    for msg in transformed {
        match msg {
            Message::Assistant(assistant_msg) => {
                close_pending_tool_calls(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    &mut held_system_messages,
                );

                if matches!(
                    assistant_msg.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }

                for block in &assistant_msg.content {
                    if let AssistantContentBlock::ToolCall(tc) = block {
                        pending_tool_calls.push((tc.id.clone(), tc.name.clone()));
                    }
                }
                if !pending_tool_calls.is_empty() {
                    existing_tool_result_ids.clear();
                }

                result.push(Message::Assistant(assistant_msg));
            }
            Message::ToolResult(tr) => {
                if !pending_tool_calls
                    .iter()
                    .any(|(tc_id, _)| tc_id == &tr.tool_call_id)
                {
                    continue;
                }
                if !existing_tool_result_ids.insert(tr.tool_call_id.clone()) {
                    continue;
                }
                result.push(Message::ToolResult(tr));
            }
            Message::System(_) => {
                if !pending_tool_calls.is_empty() {
                    held_system_messages.push(msg);
                } else {
                    result.push(msg);
                }
            }
            Message::User(_) => {
                close_pending_tool_calls(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    &mut held_system_messages,
                );
                result.push(msg);
            }
        }
    }

    close_pending_tool_calls(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
        &mut held_system_messages,
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, SystemMessage, ThinkingContent, ToolCall, Usage};

    fn make_test_model(id: &str, provider: &str, api: &str, images: bool) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: "https://api.example.com".to_string(),
            reasoning: true,
            input: if images {
                vec!["text".to_string(), "image".to_string()]
            } else {
                vec!["text".to_string()]
            },
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
            thinking_level_map: None,
        }
    }

    fn make_assistant_message(
        model: &Model,
        content: Vec<AssistantContentBlock>,
        stop_reason: StopReason,
    ) -> AssistantMessage {
        AssistantMessage {
            content,
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: now_ms(),
        }
    }

    #[test]
    fn cross_model_thinking_dropped_and_same_model_retained() {
        let kimi = make_test_model("kimi-k3", "sference", "openai-completions", true);
        let glm = make_test_model("glm-5", "sference", "openai-completions", true);

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("question 1".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &glm,
                vec![
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "glm cross-model thinking".to_string(),
                        thinking_signature: Some("sig_glm".to_string()),
                        redacted: None,
                    }),
                    AssistantContentBlock::Text(TextContent {
                        text: "glm answer".to_string(),
                        text_signature: None,
                    }),
                ],
                StopReason::Stop,
            )),
            Message::User(UserMessage {
                content: UserContent::Text("question 2".to_string()),
                timestamp: 2,
            }),
            Message::Assistant(make_assistant_message(
                &kimi,
                vec![
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "kimi thinking 1".to_string(),
                        thinking_signature: Some("sig1".to_string()),
                        redacted: None,
                    }),
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "".to_string(),
                        thinking_signature: Some("sig2".to_string()),
                        redacted: None,
                    }),
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "".to_string(),
                        thinking_signature: None,
                        redacted: Some(true),
                    }),
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "   ".to_string(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                    AssistantContentBlock::Thinking(ThinkingContent {
                        thinking: "kimi thinking plain".to_string(),
                        thinking_signature: None,
                        redacted: None,
                    }),
                    AssistantContentBlock::Text(TextContent {
                        text: "kimi answer".to_string(),
                        text_signature: None,
                    }),
                ],
                StopReason::Stop,
            )),
        ];

        let result = transform_messages(&messages, &kimi, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result.len(), 4);

        // GLM assistant message (index 1): thinking should be dropped because !is_same_model
        if let Message::Assistant(ref ast) = result[1] {
            assert_eq!(ast.content.len(), 1);
            assert!(matches!(ast.content[0], AssistantContentBlock::Text(_)));
        } else {
            panic!("Expected assistant message at index 1");
        }

        // Kimi assistant message (index 3): same-model thinking blocks
        if let Message::Assistant(ref ast) = result[3] {
            // thinking 1 (retained), thinking 2 (signed, retained), thinking 3 (redacted, retained),
            // thinking 4 (empty unsigned, dropped), thinking 5 (plain non-empty, retained), text
            assert_eq!(ast.content.len(), 5);
            match &ast.content[0] {
                AssistantContentBlock::Thinking(t) => assert_eq!(t.thinking, "kimi thinking 1"),
                _ => panic!("Expected thinking block 0"),
            }
            match &ast.content[1] {
                AssistantContentBlock::Thinking(t) => {
                    assert_eq!(t.thinking, "");
                    assert_eq!(t.thinking_signature.as_deref(), Some("sig2"));
                }
                _ => panic!("Expected thinking block 1"),
            }
            match &ast.content[2] {
                AssistantContentBlock::Thinking(t) => {
                    assert_eq!(t.thinking, "");
                    assert_eq!(t.redacted, Some(true));
                }
                _ => panic!("Expected thinking block 2"),
            }
            match &ast.content[3] {
                AssistantContentBlock::Thinking(t) => assert_eq!(t.thinking, "kimi thinking plain"),
                _ => panic!("Expected thinking block 3"),
            }
            match &ast.content[4] {
                AssistantContentBlock::Text(t) => assert_eq!(t.text, "kimi answer"),
                _ => panic!("Expected text block 4"),
            }
        } else {
            panic!("Expected assistant message at index 3");
        }
    }

    #[test]
    fn cross_model_strips_signatures_and_maps_tool_call_ids() {
        let claude = make_test_model("claude-sonnet-4.6", "anthropic", "anthropic-messages", true);
        let copilot = make_test_model("gpt-5", "github-copilot", "openai-responses", true);

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("run command".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &copilot,
                vec![
                    AssistantContentBlock::Text(TextContent {
                        text: "executing command".to_string(),
                        text_signature: Some("text_signature_to_strip".to_string()),
                    }),
                    AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_123|fc_456".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: Some("thought_signature_to_strip".to_string()),
                    }),
                ],
                StopReason::ToolUse,
            )),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_123|fc_456".to_string(),
                tool_name: "bash".to_string(),
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: "command output".to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
                usage: None,
            }),
        ];

        let normalize = |id: &str, _m: &Model, _ast: &AssistantMessage| -> String {
            id.replace('|', "_")
        };

        let result = transform_messages(&messages, &claude, Some(normalize));
        assert_eq!(result.len(), 3);

        if let Message::Assistant(ref ast) = result[1] {
            assert_eq!(ast.content.len(), 2);
            match &ast.content[0] {
                AssistantContentBlock::Text(t) => {
                    assert_eq!(t.text, "executing command");
                    assert_eq!(t.text_signature, None); // Stripped
                }
                _ => panic!("Expected text block"),
            }
            match &ast.content[1] {
                AssistantContentBlock::ToolCall(tc) => {
                    assert_eq!(tc.id, "call_123_fc_456"); // Normalized
                    assert_eq!(tc.thought_signature, None); // Stripped
                }
                _ => panic!("Expected tool call block"),
            }
        } else {
            panic!("Expected assistant message at index 1");
        }

        if let Message::ToolResult(ref tr) = result[2] {
            assert_eq!(tr.tool_call_id, "call_123_fc_456"); // Rewritten to match normalized ID
        } else {
            panic!("Expected tool result message at index 2");
        }
    }

    #[test]
    fn skips_errored_and_aborted_assistant_turns() {
        let model = make_test_model("test-model", "test-provider", "test-api", true);

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("user message 1".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::Text(TextContent {
                    text: "errored text".to_string(),
                    text_signature: None,
                })],
                StopReason::Error,
            )),
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::Text(TextContent {
                    text: "aborted text".to_string(),
                    text_signature: None,
                })],
                StopReason::Aborted,
            )),
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::Text(TextContent {
                    text: "valid text".to_string(),
                    text_signature: None,
                })],
                StopReason::Stop,
            )),
        ];

        let result = transform_messages(&messages, &model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0], Message::User(_)));
        if let Message::Assistant(ref ast) = result[1] {
            assert_eq!(ast.stop_reason, StopReason::Stop);
            assert_eq!(ast.content.len(), 1);
            match &ast.content[0] {
                AssistantContentBlock::Text(t) => assert_eq!(t.text, "valid text"),
                _ => panic!("Expected text block"),
            }
        } else {
            panic!("Expected assistant message at index 1");
        }
    }

    #[test]
    fn orphaned_tool_calls_synthesize_error_results_and_defer_system_messages() {
        let model = make_test_model("test-model", "test-provider", "test-api", true);

        // Case A: End of transcript with unanswered tool call and held system message
        let messages_a = vec![
            Message::User(UserMessage {
                content: UserContent::Text("run tools".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &model,
                vec![
                    AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    }),
                    AssistantContentBlock::ToolCall(ToolCall {
                        id: "call_2".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::Map::new(),
                        arguments_error: None,
                        thought_signature: None,
                    }),
                ],
                StopReason::ToolUse,
            )),
            Message::System(SystemMessage {
                content: "mid-turn system update".to_string(),
                timestamp: 2,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1".to_string(),
                tool_name: "bash".to_string(),
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: "bash result".to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 3,
                duration_ms: None,
                usage: None,
            }),
        ];

        let result_a = transform_messages(&messages_a, &model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        // Expect: User, Assistant, ToolResult(call_1), ToolResult(synthetic call_2), System
        assert_eq!(result_a.len(), 5);
        assert!(matches!(result_a[0], Message::User(_)));
        assert!(matches!(result_a[1], Message::Assistant(_)));
        if let Message::ToolResult(ref tr) = result_a[2] {
            assert_eq!(tr.tool_call_id, "call_1");
            assert!(!tr.is_error);
        } else {
            panic!("Expected real tool result at index 2");
        }
        if let Message::ToolResult(ref tr) = result_a[3] {
            assert_eq!(tr.tool_call_id, "call_2");
            assert_eq!(tr.tool_name, "read");
            assert!(tr.is_error);
            assert_eq!(tr.content.len(), 1);
            match &tr.content[0] {
                ToolResultContentBlock::Text(t) => assert_eq!(t.text, "No result provided"),
                _ => panic!("Expected text block in synthetic result"),
            }
        } else {
            panic!("Expected synthetic tool result at index 3");
        }
        if let Message::System(ref s) = result_a[4] {
            assert_eq!(s.content, "mid-turn system update");
        } else {
            panic!("Expected deferred system message at index 4");
        }

        // Case B: New User turn interrupts orphaned tool call
        let messages_b = vec![
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_3".to_string(),
                    name: "calc".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            )),
            Message::User(UserMessage {
                content: UserContent::Text("never mind".to_string()),
                timestamp: 4,
            }),
        ];

        let result_b = transform_messages(&messages_b, &model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result_b.len(), 3);
        assert!(matches!(result_b[0], Message::Assistant(_)));
        if let Message::ToolResult(ref tr) = result_b[1] {
            assert_eq!(tr.tool_call_id, "call_3");
            assert!(tr.is_error);
        } else {
            panic!("Expected synthetic result at index 1");
        }
        assert!(matches!(result_b[2], Message::User(_)));

        // Case C: New Assistant turn interrupts orphaned tool call
        let messages_c = vec![
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_4".to_string(),
                    name: "calc".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            )),
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::Text(TextContent {
                    text: "retry answer".to_string(),
                    text_signature: None,
                })],
                StopReason::Stop,
            )),
        ];

        let result_c = transform_messages(&messages_c, &model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result_c.len(), 3);
        assert!(matches!(result_c[0], Message::Assistant(_)));
        if let Message::ToolResult(ref tr) = result_c[1] {
            assert_eq!(tr.tool_call_id, "call_4");
            assert!(tr.is_error);
        } else {
            panic!("Expected synthetic result at index 1");
        }
        assert!(matches!(result_c[2], Message::Assistant(_)));
    }

    #[test]
    fn downgrade_unsupported_images_collapses_runs() {
        let non_vision_model = make_test_model("text-only", "provider", "api", false);
        let vision_model = make_test_model("vision", "provider", "api", true);

        let img = ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    UserContentBlock::Image(img.clone()),
                    UserContentBlock::Image(img.clone()),
                    UserContentBlock::Text(TextContent {
                        text: "middle text".to_string(),
                        text_signature: None,
                    }),
                    UserContentBlock::Image(img.clone()),
                ]),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &non_vision_model,
                vec![AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: "capture".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            )),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1".to_string(),
                tool_name: "capture".to_string(),
                content: vec![
                    ToolResultContentBlock::Image(img.clone()),
                    ToolResultContentBlock::Image(img.clone()),
                ],
                details: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
                usage: None,
            }),
        ];

        // Non-vision model: images replaced with collapsed placeholders
        let result_non_vision =
            transform_messages(&messages, &non_vision_model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result_non_vision.len(), 3);

        if let Message::User(ref u) = result_non_vision[0] {
            if let UserContent::Blocks(ref blocks) = u.content {
                assert_eq!(blocks.len(), 3);
                match &blocks[0] {
                    UserContentBlock::Text(t) => {
                        assert_eq!(t.text, NON_VISION_USER_IMAGE_PLACEHOLDER);
                    }
                    _ => panic!("Expected placeholder text at block 0"),
                }
                match &blocks[1] {
                    UserContentBlock::Text(t) => assert_eq!(t.text, "middle text"),
                    _ => panic!("Expected middle text at block 1"),
                }
                match &blocks[2] {
                    UserContentBlock::Text(t) => {
                        assert_eq!(t.text, NON_VISION_USER_IMAGE_PLACEHOLDER);
                    }
                    _ => panic!("Expected placeholder text at block 2"),
                }
            } else {
                panic!("Expected user content blocks");
            }
        } else {
            panic!("Expected user message");
        }

        if let Message::ToolResult(ref tr) = result_non_vision[2] {
            assert_eq!(tr.content.len(), 1);
            match &tr.content[0] {
                ToolResultContentBlock::Text(t) => {
                    assert_eq!(t.text, NON_VISION_TOOL_IMAGE_PLACEHOLDER);
                }
                _ => panic!("Expected tool placeholder text"),
            }
        } else {
            panic!("Expected tool result message");
        }

        // Vision model: images retained
        let result_vision =
            transform_messages(&messages, &vision_model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        if let Message::User(ref u) = result_vision[0] {
            if let UserContent::Blocks(ref blocks) = u.content {
                assert_eq!(blocks.len(), 4);
                assert!(matches!(blocks[0], UserContentBlock::Image(_)));
                assert!(matches!(blocks[1], UserContentBlock::Image(_)));
            }
        }
    }

    #[test]
    fn replace_images_deduplicates_preexisting_placeholder() {
        let non_vision_model = make_test_model("text-only", "provider", "api", false);
        let img = ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent {
                    text: NON_VISION_USER_IMAGE_PLACEHOLDER.to_string(),
                    text_signature: None,
                }),
                UserContentBlock::Image(img.clone()),
                UserContentBlock::Image(img),
            ]),
            timestamp: 1,
        })];

        let result =
            transform_messages(&messages, &non_vision_model, None::<fn(&str, &Model, &AssistantMessage) -> String>);
        assert_eq!(result.len(), 1);

        if let Message::User(ref u) = result[0] {
            if let UserContent::Blocks(ref blocks) = u.content {
                // Must collapse into a single placeholder block!
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    UserContentBlock::Text(t) => {
                        assert_eq!(t.text, NON_VISION_USER_IMAGE_PLACEHOLDER);
                    }
                    _ => panic!("Expected text block"),
                }
            } else {
                panic!("Expected user blocks");
            }
        } else {
            panic!("Expected user message");
        }
    }

    #[test]
    fn split_error_tool_result_images_extracts_images_and_caption() {
        let img1 = ImageContent {
            data: "AAAA".to_string(),
            mime_type: "image/png".to_string(),
        };
        let img2 = ImageContent {
            data: "CCCC".to_string(),
            mime_type: "image/png".to_string(),
        };

        // 1. Returns None when !is_error
        let non_error_msg = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "codemode".to_string(),
            content: vec![ToolResultContentBlock::Image(img1.clone())],
            details: None,
            is_error: false,
            timestamp: 1,
            duration_ms: None,
            usage: None,
        };
        assert!(split_error_tool_result_images(&non_error_msg).is_none());

        // 2. Returns None when is_error but no images
        let text_error_msg = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "codemode".to_string(),
            content: vec![ToolResultContentBlock::Text(TextContent {
                text: "error text".to_string(),
                text_signature: None,
            })],
            details: None,
            is_error: true,
            timestamp: 1,
            duration_ms: None,
            usage: None,
        };
        assert!(split_error_tool_result_images(&text_error_msg).is_none());

        // 3. When is_error with images: extracts caption, images, and replaces image run with placeholder
        let error_msg = ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "codemode".to_string(),
            content: vec![
                ToolResultContentBlock::Text(TextContent {
                    text: "Script failed".to_string(),
                    text_signature: None,
                }),
                ToolResultContentBlock::Image(img1.clone()),
                ToolResultContentBlock::Image(img2.clone()),
                ToolResultContentBlock::Text(TextContent {
                    text: "Script error".to_string(),
                    text_signature: None,
                }),
            ],
            details: None,
            is_error: true,
            timestamp: 1,
            duration_ms: None,
            usage: None,
        };

        let split = split_error_tool_result_images(&error_msg).expect("Expected SplitErrorToolResult");
        assert_eq!(
            split.caption,
            "Images from the error result of tool call call-1 (codemode):"
        );
        assert_eq!(split.images, vec![img1, img2]);
        assert_eq!(split.content.len(), 3);
        match &split.content[0] {
            ToolResultContentBlock::Text(t) => assert_eq!(t.text, "Script failed"),
            _ => panic!("Expected text at content[0]"),
        }
        match &split.content[1] {
            ToolResultContentBlock::Text(t) => assert_eq!(t.text, ERROR_TOOL_IMAGE_PLACEHOLDER),
            _ => panic!("Expected placeholder at content[1]"),
        }
        match &split.content[2] {
            ToolResultContentBlock::Text(t) => assert_eq!(t.text, "Script error"),
            _ => panic!("Expected text at content[2]"),
        }
    }

    #[test]
    fn dropped_errored_assistant_turn_drops_orphaned_tool_result_and_skips_normalize() {
        let target = make_test_model("claude-sonnet-4.6", "anthropic", "anthropic-messages", true);
        let other = make_test_model("gpt-5", "openai", "openai-completions", true);

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("go".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &other,
                vec![AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_err".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                })],
                StopReason::Error,
            )),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_err".to_string(),
                tool_name: "bash".to_string(),
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: "stale".to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
                usage: None,
            }),
        ];

        let mut normalized_ids = Vec::new();
        let result = transform_messages(
            &messages,
            &target,
            Some(|id: &str, _m: &Model, _ast: &AssistantMessage| -> String {
                normalized_ids.push(id.to_string());
                format!("norm_{id}")
            }),
        );
        assert!(normalized_ids.is_empty());
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Message::User(_)));
    }

    #[test]
    fn normalize_tool_call_id_observes_non_empty_assistant_content() {
        let target = make_test_model("claude-sonnet-4.6", "anthropic", "anthropic-messages", true);
        let other = make_test_model("gpt-5", "openai", "openai-completions", true);

        let messages = vec![Message::Assistant(make_assistant_message(
            &other,
            vec![
                AssistantContentBlock::Text(TextContent {
                    text: "calling tool".to_string(),
                    text_signature: None,
                }),
                AssistantContentBlock::ToolCall(ToolCall {
                    id: "raw_id".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                }),
            ],
            StopReason::ToolUse,
        ))];

        let mut observed_len = 0usize;
        let _ = transform_messages(
            &messages,
            &target,
            Some(|id: &str, _m: &Model, ast: &AssistantMessage| -> String {
                observed_len = ast.content.len();
                format!("norm_{id}")
            }),
        );
        assert_eq!(observed_len, 2);
    }

    #[test]
    fn empty_cross_model_thinking_only_assistant_turn_is_skipped() {
        let target = make_test_model("claude-sonnet-4.6", "anthropic", "anthropic-messages", true);
        let other = make_test_model("gpt-5", "openai", "openai-completions", true);

        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 1,
            }),
            Message::Assistant(make_assistant_message(
                &other,
                vec![AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "internal thoughts only".to_string(),
                    thinking_signature: Some("sig".to_string()),
                    redacted: None,
                })],
                StopReason::Stop,
            )),
            Message::User(UserMessage {
                content: UserContent::Text("follow-up".to_string()),
                timestamp: 2,
            }),
        ];

        let result = transform_messages(
            &messages,
            &target,
            None::<fn(&str, &Model, &AssistantMessage) -> String>,
        );
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0], Message::User(_)));
        assert!(matches!(result[1], Message::User(_)));
    }

    #[test]
    fn duplicate_tool_results_for_same_call_id_are_deduplicated() {
        let model = make_test_model("claude-sonnet-4.6", "anthropic", "anthropic-messages", true);
        let mk_result = |text: &str| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_dup".to_string(),
                tool_name: "read".to_string(),
                content: vec![ToolResultContentBlock::Text(TextContent {
                    text: text.to_string(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 2,
                duration_ms: None,
                usage: None,
            })
        };
        let messages = vec![
            Message::Assistant(make_assistant_message(
                &model,
                vec![AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_dup".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::Map::new(),
                    arguments_error: None,
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            )),
            mk_result("first"),
            mk_result("duplicate"),
        ];
        let result = transform_messages(
            &messages,
            &model,
            None::<fn(&str, &Model, &AssistantMessage) -> String>,
        );
        assert_eq!(result.len(), 2);
        match &result[1] {
            Message::ToolResult(tr) => match &tr.content[0] {
                ToolResultContentBlock::Text(t) => assert_eq!(t.text, "first"),
                other => panic!("unexpected block: {other:?}"),
            },
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_and_cap_tool_call_id_avoids_long_prefix_collisions() {
        let prefix = "a".repeat(70);
        let id1 = format!("{prefix}_1");
        let id2 = format!("{prefix}_2");
        let n1 = sanitize_and_cap_tool_call_id(&id1, 64);
        let n2 = sanitize_and_cap_tool_call_id(&id2, 64);
        assert!(n1.len() <= 64);
        assert!(n2.len() <= 64);
        assert_ne!(n1, n2);
        assert!(n1
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }
}
