//! Session management, settings, auth resolution, system prompt assembly,
//! agent session orchestrator, and execution modes for mehh coding agent.

pub mod agent_session;
pub mod auth;
pub mod branch_summarization;
pub mod clipboard;
pub mod compaction;
pub mod file_args;
pub mod file_complete;
pub mod interactive;
pub mod lead_worker;
pub mod mcp_manager;
pub mod models_config;
pub mod modes;
pub mod permissions;
pub mod prompt_templates;
pub mod rpc;
pub mod session;
pub mod settings;
pub mod skills;
pub mod subagent;
pub mod subagent_control;
pub mod subagent_lifecycle;
pub mod system_prompt;
pub mod todo;
pub mod tool_inspection;
pub mod tool_search;
pub mod trust;

pub use agent_session::*;
pub use auth::*;
pub use branch_summarization::*;
pub use clipboard::*;
pub use compaction::*;
pub use file_args::*;
pub use file_complete::*;
pub use interactive::*;
pub use lead_worker::*;
pub use mcp_manager::*;
pub use models_config::*;
pub use modes::*;
pub use permissions::*;
pub use prompt_templates::*;
pub use rpc::*;
pub use session::*;
pub use settings::*;
pub use skills::*;
pub use subagent::*;
pub use subagent_control::*;
pub use subagent_lifecycle::*;
pub use system_prompt::*;
pub use todo::*;
pub use tool_inspection::*;
pub use tool_search::*;
pub use trust::*;

/// Serializes tests that mutate process-global environment variables.
#[cfg(test)]
pub(crate) static ENV_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
pub(crate) struct TempTestDir(std::path::PathBuf);

#[cfg(test)]
impl TempTestDir {
    pub(crate) fn new(prefix: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mehh_ca_test_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use uuid::Uuid;

    use mehh_ai::{
        now_ms, AssistantContentBlock, AssistantMessage, Message, Model, ModelCost, StopReason,
        TextContent, ThinkingLevel, ToolCall, Usage, UserContent, UserMessage,
    };

    use crate::agent_session::AgentSession;
    use crate::auth::resolve_api_key;
    use crate::modes::{run_print_mode, OutputMode};
    use crate::session::{SessionEntryKind, SessionManager};
    use crate::settings::{Settings, SettingsManager};
    use crate::system_prompt::{build_system_prompt, discover_context_files};

    fn make_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mehh_test_{name}_{}", Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_session_jsonl_v3_roundtrip_and_tree() {
        let temp_dir = make_test_dir("session_v3");
        let file_path = temp_dir.join("test_session.jsonl");

        // 1. Create file session and append messages
        let mut session = SessionManager::open_file(&file_path, &temp_dir).unwrap();
        assert_eq!(session.header().version, 3);
        assert_eq!(session.header().kind, "session");
        assert!(session.leaf_id().is_none());

        let _m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Initial prompt".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let m2 = session
            .append_message(Message::Assistant(AssistantMessage {
                content: vec![AssistantContentBlock::Text(TextContent {
                    text: "Assistant response 1".to_string(),
                    text_signature: None,
                })],
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now_ms(),
            }))
            .unwrap();

        let m3 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Followup on branch A".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        assert_eq!(session.leaf_id(), Some(m3.as_str()));
        let ctx = session.build_context_messages();
        assert_eq!(ctx.len(), 3);

        // 2. Test tree branching from m2
        session.branch_from(&m2).unwrap();
        assert_eq!(session.leaf_id(), Some(m2.as_str()));

        let m4 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Alternative path on branch B".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let _m5 = session
            .append_message(Message::Assistant(AssistantMessage {
                content: vec![AssistantContentBlock::Text(TextContent {
                    text: "Assistant reply on branch B".to_string(),
                    text_signature: None,
                })],
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: now_ms(),
            }))
            .unwrap();

        let branch_b_ctx = session.build_context_messages();
        assert_eq!(branch_b_ctx.len(), 4); // m1, m2, m4, m5

        // 3. Test Compaction on active path
        let _comp_id = session
            .append_entry(SessionEntryKind::Compaction {
                summary: "Compacted history summary prior to branch B work".to_string(),
                first_kept_entry_id: m4.clone(),
                tokens_before: 2500,
            })
            .unwrap();

        let m6 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Query after compaction".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let compacted_ctx = session.build_context_messages();
        // Should have: System(summary), m4, m5, m6
        assert_eq!(compacted_ctx.len(), 4);
        match &compacted_ctx[0] {
            Message::System(sys) => {
                assert!(sys.content.contains("Compacted history summary"));
            }
            _ => panic!("Expected first message to be System compaction summary"),
        }
        match &compacted_ctx[1] {
            Message::User(u) => assert_eq!(u.content.text(), "Alternative path on branch B"),
            _ => panic!("Expected m4 User message"),
        }
        match &compacted_ctx[2] {
            Message::Assistant(a) => assert_eq!(a.text_content(), "Assistant reply on branch B"),
            _ => panic!("Expected m5 Assistant message"),
        }
        match &compacted_ctx[3] {
            Message::User(u) => assert_eq!(u.content.text(), "Query after compaction"),
            _ => panic!("Expected m6 User message"),
        }

        // 4. Reload from disk and verify exact match
        let reloaded = SessionManager::open_file(&file_path, &temp_dir).unwrap();
        assert_eq!(reloaded.session_id(), session.session_id());
        assert_eq!(reloaded.entries().len(), session.entries().len());
        assert_eq!(reloaded.leaf_id(), Some(m6.as_str()));

        let reloaded_ctx = reloaded.build_context_messages();
        assert_eq!(reloaded_ctx.len(), 4);
        match &reloaded_ctx[0] {
            Message::System(s) => assert!(s.content.contains("Compacted history summary")),
            _ => panic!("Expected system message"),
        }

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_settings_merge() {
        let base = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-3-7-sonnet".to_string()),
            default_thinking_level: Some(ThinkingLevel::Low),
            theme: Some("dark".to_string()),
            auto_compact: Some(true),
            max_tokens: Some(4096),
            temperature: Some(0.3),
            ..Default::default()
        };

        let overlay = Settings {
            default_provider: Some("openai".to_string()),
            default_model: None,
            default_thinking_level: Some(ThinkingLevel::High),
            theme: None,
            auto_compact: None,
            max_tokens: Some(8192),
            temperature: None,
            ..Default::default()
        };

        let merged = SettingsManager::merge(base, overlay);
        assert_eq!(merged.default_provider, Some("openai".to_string()));
        assert_eq!(merged.default_model, Some("claude-3-7-sonnet".to_string()));
        assert_eq!(merged.default_thinking_level, Some(ThinkingLevel::High));
        assert_eq!(merged.theme, Some("dark".to_string()));
        assert_eq!(merged.auto_compact, Some(true));
        assert_eq!(merged.max_tokens, Some(8192));
        assert_eq!(merged.temperature, Some(0.3));
    }

    #[test]
    fn test_auth_resolve_api_key() {
        // Explicit key takes highest precedence
        let resolved = resolve_api_key("anthropic", Some("explicit-key-123"), None);
        assert_eq!(resolved, Some("explicit-key-123".to_string()));

        // Auth file parsing
        let temp_dir = make_test_dir("auth_test");
        let auth_path = temp_dir.join("auth.json");

        let auth_json = serde_json::json!({
            "testprovider": {
                "apiKey": "test-key-abc"
            },
            "otherprovider": "direct-string-key"
        });
        fs::write(&auth_path, auth_json.to_string()).unwrap();

        let resolved_obj = resolve_api_key("testprovider", None, Some(&auth_path));
        assert_eq!(resolved_obj, Some("test-key-abc".to_string()));

        let resolved_direct = resolve_api_key("otherprovider", None, Some(&auth_path));
        assert_eq!(resolved_direct, Some("direct-string-key".to_string()));

        let resolved_missing = resolve_api_key("nonexistent", None, Some(&auth_path));
        assert_eq!(resolved_missing, None);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_system_prompt_and_context_discovery() {
        let temp_dir = make_test_dir("prompt_test");
        let agents_md = temp_dir.join("AGENTS.md");
        fs::write(&agents_md, "# Test Rules\nFollow standard protocols.").unwrap();

        let sub_dir = temp_dir.join("subdir");
        fs::create_dir_all(&sub_dir).unwrap();

        let discovered = discover_context_files(&sub_dir);
        assert_eq!(discovered.len(), 1);
        assert!(discovered[0].1.contains("Follow standard protocols."));

        let prompt =
            build_system_prompt(&sub_dir, None, Some("Custom addendum"), &["read", "write"]);
        assert!(prompt.contains("<tools>"));
        assert!(prompt.contains("- read"));
        assert!(prompt.contains("- write"));
        assert!(prompt.contains("<rules>"));
        assert!(prompt.contains("<project_context>"));
        assert!(prompt.contains("<addendum>\nCustom addendum\n</addendum>"));
        assert!(prompt.contains("<cwd>"));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_agent_session_faux_write_read_roundtrip() {
        let temp_dir = make_test_dir("agent_session_e2e");
        let session_file = temp_dir.join("session.jsonl");

        let model = Model {
            id: "faux-model".to_string(),
            name: "Faux Model".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };

        // Turn 1: Assistant calls write tool
        let mut write_args = serde_json::Map::new();
        write_args.insert(
            "path".to_string(),
            serde_json::Value::String("hello.txt".to_string()),
        );
        write_args.insert(
            "content".to_string(),
            serde_json::Value::String("persisted content from faux".to_string()),
        );

        let turn1 = AssistantMessage {
            content: vec![AssistantContentBlock::ToolCall(ToolCall {
                id: "call_write_1".to_string(),
                name: "write".to_string(),
                arguments: write_args,
                arguments_error: None,
                thought_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: now_ms(),
        };

        // Turn 2: Assistant calls read tool
        let mut read_args = serde_json::Map::new();
        read_args.insert(
            "path".to_string(),
            serde_json::Value::String("hello.txt".to_string()),
        );

        let turn2 = AssistantMessage {
            content: vec![AssistantContentBlock::ToolCall(ToolCall {
                id: "call_read_1".to_string(),
                name: "read".to_string(),
                arguments: read_args,
                arguments_error: None,
                thought_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: Some("resp_2".to_string()),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: now_ms(),
        };

        // Turn 3: Assistant finishes with confirmation text
        let turn3 = AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "Successfully verified hello.txt content!".to_string(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: Some("resp_3".to_string()),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };

        let faux = mehh_ai::FauxProvider::new();
        faux.add_response(turn1);
        faux.add_response(turn2);
        faux.add_response(turn3);

        let session_mgr = SessionManager::open_file(&session_file, &temp_dir).unwrap();
        let mut session = AgentSession::new(
            temp_dir.clone(),
            model,
            Arc::new(faux),
            session_mgr,
            Settings::default(),
        );

        let new_messages = session
            .prompt("Write and then read hello.txt", None)
            .await
            .unwrap();

        // Check messages count:
        // 1. UserMessage
        // 2. AssistantMessage (call write)
        // 3. ToolResultMessage (write)
        // 4. AssistantMessage (call read)
        // 5. ToolResultMessage (read)
        // 6. AssistantMessage (final text)
        assert_eq!(new_messages.len(), 6);

        // Verify write tool wrote the file to disk
        let written_file = temp_dir.join("hello.txt");
        assert!(written_file.exists());
        let content = fs::read_to_string(&written_file).unwrap();
        assert_eq!(content, "persisted content from faux");

        // Verify session file was persisted to disk
        assert!(session_file.exists());
        let reloaded = SessionManager::open_file(&session_file, &temp_dir).unwrap();
        assert_eq!(reloaded.entries().len(), 6);

        // Verify print mode with text output
        let faux2 = mehh_ai::FauxProvider::new();
        faux2.add_response(AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "Print mode response text".to_string(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-model".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        });
        session.provider = Arc::new(faux2);

        let final_text = run_print_mode(&mut session, "Say print mode", OutputMode::Text)
            .await
            .unwrap();
        assert_eq!(final_text, "Print mode response text");

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_agent_session_enable_mcp_and_codemode() {
        let temp_dir = make_test_dir("mcp_codemode");
        let session_mgr = SessionManager::in_memory(&temp_dir);
        let faux = mehh_ai::FauxProvider::new();
        let model = Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        };
        let mut session = AgentSession::new(
            temp_dir.clone(),
            model,
            Arc::new(faux),
            session_mgr,
            Settings::default(),
        );

        let initial_count = session.tools().len();
        assert!(initial_count > 0);

        struct DummyTool;
        #[async_trait::async_trait]
        impl mehh_agent::AgentTool for DummyTool {
            fn name(&self) -> &str {
                "mcp__test__dummy"
            }
            fn description(&self) -> &str {
                "Dummy MCP tool"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(
                &self,
                _id: &str,
                _params: serde_json::Map<String, serde_json::Value>,
                _cancel: tokio_util::sync::CancellationToken,
                _on_update: Option<mehh_agent::AgentToolUpdateCallback>,
            ) -> Result<mehh_agent::AgentToolResult, String> {
                Ok(mehh_agent::AgentToolResult::text("ok"))
            }
        }

        session.enable_mcp_and_codemode(vec![Arc::new(DummyTool)], true);

        // Initial tools + dummy tool + codemode tool
        assert_eq!(session.tools().len(), initial_count + 2);
        let tool_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
        assert!(tool_names.contains(&"mcp__test__dummy"));
        assert!(tool_names.contains(&"codemode"));

        // Verify system prompt was updated to include mcp and codemode tools
        assert!(session.system_prompt.contains("mcp__test__dummy"));
        assert!(session.system_prompt.contains("codemode"));

        let _ = fs::remove_dir_all(temp_dir);
    }
}
