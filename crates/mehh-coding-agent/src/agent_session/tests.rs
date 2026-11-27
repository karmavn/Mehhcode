use mehh_agent::AgentTool;
use mehh_ai::{faux_model, StopReason, ThinkingLevel, UserContent, UserMessage};

use super::*;
use crate::SessionEntryKind;

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
fn test_prepare_prompt_content_inherits_aborted_prep_cancellation() {
    let temp_dir =
        std::env::temp_dir().join(format!("agent_session_handoff_{}", uuid::Uuid::now_v7()));
    let session = AgentSession::new(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings::default(),
    );

    // `start_turn_preparation` installs a preparation token in the slot.
    let prep_cancel = CancellationToken::new();
    *session.active_cancel_slot().lock().unwrap() = Some(prep_cancel.clone());
    assert!(session.is_active());

    // Ctrl+C lands after ReadyForTurn's `is_cancelled` check but before the
    // loop token is installed. abort() cancels the preparation token.
    assert!(session.abort());
    assert!(prep_cancel.is_cancelled());

    // `start_turn` -> `prepare_prompt_content` installs the loop token. It
    // must inherit the preparation cancellation instead of clearing it.
    let (_msg, _history, config) =
        session.prepare_prompt_content(UserContent::Text("retry".to_string()));
    assert!(
        config.cancel.is_cancelled(),
        "loop token must inherit cancellation from the aborted preparation token"
    );

    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn test_context_files_injection_gated_by_trust() {
    let temp_dir =
        std::env::temp_dir().join(format!("agent_session_ctx_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let marker = "UNTRUSTED_AGENTS_MD_MARKER_7F3A";
    std::fs::write(
        temp_dir.join("AGENTS.md"),
        format!("Project rule {marker} must be followed."),
    )
    .unwrap();

    let mut untrusted = AgentSession::new_with_trust(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings::default(),
        false,
    );
    assert!(
        !untrusted.system_prompt.contains(marker),
        "untrusted AGENTS.md must not be injected into the system prompt"
    );
    untrusted.rebuild_system_prompt();
    assert!(
        !untrusted.system_prompt.contains(marker),
        "rebuild must also keep untrusted AGENTS.md out of the system prompt"
    );

    let trusted = AgentSession::new_with_trust(
        temp_dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&temp_dir),
        Settings::default(),
        true,
    );
    assert!(
        trusted.system_prompt.contains(marker),
        "trusted AGENTS.md must be injected into the system prompt"
    );

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

    session.reload().unwrap();

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
                messages: Arc::new(messages.clone()),
                tools: None,
            },
            model: faux_model(),
            thinking_level: None,
        };
        let update = hook(ctx, CancellationToken::new()).await.expect("update");
        messages = Arc::unwrap_or_clone(update.context.expect("context").messages);
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
fn sync_thinking_for_model_handles_reasoning_non_reasoning_and_explicit_off() {
    let dir = crate::TempTestDir::new("sync_thinking_for_model");
    let reg = crate::models_config::ModelRegistry::builtin_only();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let o3 = reg.resolve("openai", Some("o3")).unwrap();
    let mut gpt41 = o3.clone();
    gpt41.id = "gpt-4.1".to_string();
    gpt41.reasoning = false;

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        claude.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    assert_eq!(session.thinking_level(), None);

    // Switching to reasoning model without explicit Off activates Medium default
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));

    // Switching to non-reasoning model clears active level
    session.set_model(gpt41, Arc::new(FauxProvider::new()));
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), None);

    // Switching back restores Medium when user did not explicitly turn thinking off
    session.set_model(claude.clone(), Arc::new(FauxProvider::new()));
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));

    // Explicit Off persists across disable-capable models (Claude), clamps to Medium on o3
    session.set_thinking_level_explicit(ThinkingLevel::Off);
    assert_eq!(session.thinking_level(), None);
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), None);

    session.set_model(o3, Arc::new(FauxProvider::new()));
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));
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
            messages: Arc::new(Vec::new()),
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

#[test]
fn subagent_child_tools_survive_approval_and_api_key_refresh() {
    let dir = std::env::temp_dir()
        .join(format!("agent_session_subagent_refresh_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::new_with_trust(
        dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&dir),
        Settings::default(),
        false,
    );
    session.set_tools(Vec::new());
    session.set_subagent_with_child_tools(vec![Arc::new(MockTool {
        name: "read".to_string(),
    })]);

    session.set_approval_handler(None);
    session.set_api_key(Some("updated-key".to_string()));

    let parent_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    assert_eq!(parent_names, vec!["subagent"]);
    let child_names: Option<Vec<&str>> = session
        .subagent_child_tools
        .as_ref()
        .map(|tools| tools.iter().map(|t| t.name()).collect());
    assert_eq!(child_names, Some(vec!["read"]));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_agent_session_applies_web_settings() {
    let dir = std::env::temp_dir()
        .join(format!("agent_session_web_cfg_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();

    let default_session = AgentSession::new_with_trust(
        dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&dir),
        Settings::default(),
        false,
    );
    assert!(default_session.tools().iter().any(|t| t.name() == "web_search"));
    assert!(default_session.tools().iter().any(|t| t.name() == "web_fetch"));

    let disabled_web = AgentSession::new_with_trust(
        dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(&dir),
        Settings {
            web: Some(crate::settings::WebSettings {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        },
        false,
    );
    assert!(!disabled_web.tools().iter().any(|t| t.name() == "web_search"));
    assert!(!disabled_web.tools().iter().any(|t| t.name() == "web_fetch"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn thinking_normalization_prevents_invalid_off_and_non_reasoning_leak() {
    let dir = crate::TempTestDir::new("thinking_norm");
    let reg = crate::models_config::ModelRegistry::builtin_only();
    let gemini_pro = reg.resolve("google", Some("gemini-2.5-pro")).unwrap();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let gpt4o = reg.resolve("openai", Some("gpt-4o")).unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        gemini_pro.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );

    // Always-on reasoning model (can_disable == false) must clamp Off/None to Medium
    session.set_thinking_level_explicit(ThinkingLevel::Off);
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.stream_options.reasoning, Some(ThinkingLevel::Medium));

    // RPC clear (`None`) on always-on model must also stay Medium
    session.set_thinking_level(None);
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.stream_options.reasoning, Some(ThinkingLevel::Medium));

    // RPC clear (`None`) on disable-capable model (`claude`) must send explicit Off
    session.set_model(claude.clone(), Arc::new(FauxProvider::new()));
    session.set_thinking_level(None);
    assert_eq!(session.thinking_level(), None);
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.stream_options.reasoning, Some(ThinkingLevel::Off));

    // Non-reasoning model (`gpt-4o`) must strip stale/requested reasoning levels
    session.set_model(gpt4o.clone(), Arc::new(FauxProvider::new()));
    session.set_thinking_level(Some(ThinkingLevel::High));
    assert_eq!(session.thinking_level(), None);
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.stream_options.reasoning, None);

    // Lead/worker mode switching to non-reasoning worker must normalize per effective model
    session.set_model(claude, Arc::new(FauxProvider::new()));
    session.set_thinking_level(Some(ThinkingLevel::High));
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(gpt4o);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.lead_turns = 0;
    }
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.model.id, "gpt-4o");
    assert_eq!(cfg.stream_options.reasoning, None);
}

#[test]
fn cycle_model_syncs_thinking_level() {
    let dir = crate::TempTestDir::new("cycle_model_thinking");
    let reg = crate::models_config::ModelRegistry::builtin_only();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let gpt4o = reg.resolve("openai", Some("gpt-4o")).unwrap();
    let o3 = reg.resolve("openai", Some("o3")).unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        claude.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session.set_scoped_models(vec![claude, gpt4o, o3]);
    session.sync_thinking_for_model(true);
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));

    // claude -> gpt-4o (non-reasoning): must clear thinking_level
    let next = session.cycle_model().unwrap().unwrap();
    assert_eq!(next.id, "gpt-4o");
    assert_eq!(session.thinking_level(), None);

    // gpt-4o -> o3 (always-on reasoning): must restore Medium
    let next = session.cycle_model().unwrap().unwrap();
    assert_eq!(next.id, "o3");
    assert_eq!(session.thinking_level(), Some(ThinkingLevel::Medium));
}

#[tokio::test]
async fn prepare_next_turn_normalizes_thinking_across_lead_and_worker_transitions() {
    let dir = crate::TempTestDir::new("prepare_next_turn_thinking");
    let reg = crate::models_config::ModelRegistry::builtin_only();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let gpt4o = reg.resolve("openai", Some("gpt-4o")).unwrap();
    let o3 = reg.resolve("openai", Some("o3")).unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        claude.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session.set_thinking_level(Some(ThinkingLevel::High));
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(gpt4o);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.lead_turns = 1;
    }
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    let hook = cfg.prepare_next_turn.expect("prepare_next_turn hook");
    let mut assistant_msg = mehh_ai::AssistantMessage::empty_for_model(&claude);
    assistant_msg
        .content
        .push(mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
            text: "working".into(),
            text_signature: None,
        }));
    let update = hook(
        mehh_agent::AgentTurnContext {
            turn_index: 1,
            messages: Arc::new(Vec::new()),
            assistant_message: assistant_msg.clone(),
            tool_results: Vec::new(),
        },
        CancellationToken::new(),
    )
    .await
    .expect("turn 1 should switch from claude lead to gpt-4o worker");
    assert_eq!(update.model.as_ref().unwrap().id, "gpt-4o");
    assert_eq!(
        update.thinking_level,
        Some(None),
        "switching to non-reasoning worker must explicitly clear thinking_level"
    );

    // Explicit Off on Claude lead switching to always-on o3 worker must clamp to Medium.
    session.set_thinking_level_explicit(ThinkingLevel::Off);
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(o3);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.lead_turns = 1;
    }
    let (_u2, _h2, cfg2) = session.prepare_prompt("hi");
    let hook2 = cfg2.prepare_next_turn.expect("prepare_next_turn hook");
    let update2 = hook2(
        mehh_agent::AgentTurnContext {
            turn_index: 1,
            messages: Arc::new(Vec::new()),
            assistant_message: assistant_msg,
            tool_results: Vec::new(),
        },
        CancellationToken::new(),
    )
    .await
    .expect("turn 1 should switch from claude lead to o3 worker");
    assert_eq!(update2.model.as_ref().unwrap().id, "o3");
    assert_eq!(
        update2.thinking_level,
        Some(Some(ThinkingLevel::Medium)),
        "switching with explicit Off to always-on o3 worker must send Medium"
    );
}

#[test]
fn clear_provider_api_keys_clears_matching_lead_and_worker_keys() {
    let dir = crate::TempTestDir::new("clear_prov_keys");
    let reg = crate::models_config::ModelRegistry::builtin_only();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let gpt4o = reg.resolve("openai", Some("gpt-4o")).unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        claude,
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session.set_explicit_api_key(Some("sk-ant".into()));
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(gpt4o);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.set_worker_api_key(Some("sk-openai".into()));
    }

    assert!(!session.clear_provider_api_keys("google"));
    assert_eq!(session.api_key.as_deref(), Some("sk-ant"));

    assert!(session.clear_provider_api_keys("openai"));
    assert_eq!(session.api_key.as_deref(), Some("sk-ant"));
    assert!(session.lead_worker.lock().unwrap().worker_api_key.is_none());

    assert!(session.clear_provider_api_keys("anthropic"));
    assert!(session.api_key.is_none());
    assert!(!session.has_explicit_api_key());
    assert!(session.lead_worker.lock().unwrap().lead_api_key.is_none());
}

#[tokio::test]
async fn ensure_api_key_refreshes_worker_key_and_same_provider_fallback_works() {
    let dir = crate::TempTestDir::new("ensure_worker_key");
    let auth_file = dir.path().join("auth.json");
    crate::auth::save_credential(
        "anthropic",
        &mehh_ai::StoredCredential::ApiKey {
            key: Some("sk-ant-fresh".into()),
            env: None,
        },
        Some(&auth_file),
    )
    .unwrap();
    crate::auth::save_credential(
        "openai",
        &mehh_ai::StoredCredential::ApiKey {
            key: Some("sk-openai-fresh".into()),
            env: None,
        },
        Some(&auth_file),
    )
    .unwrap();

    let reg = crate::models_config::ModelRegistry::builtin_only();
    let claude = reg.resolve("anthropic", Some("claude-sonnet-4-5")).unwrap();
    let mut haiku = claude.clone();
    haiku.id = "claude-haiku-4-5".to_string();
    let gpt4o = reg.resolve("openai", Some("gpt-4o")).unwrap();

    // Cross-provider worker gets its own key refreshed
    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        claude.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(gpt4o);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.lead_turns = 0;
    }
    session.ensure_api_key(Some(&auth_file)).await.unwrap();
    assert_eq!(session.api_key.as_deref(), Some("sk-ant-fresh"));
    assert_eq!(
        session.lead_worker.lock().unwrap().worker_api_key.as_deref(),
        Some("sk-openai-fresh")
    );

    // Same-provider worker falls back to lead key at turn zero when worker_api_key is None
    {
        let mut mgr = session.lead_worker.lock().unwrap();
        mgr.worker_model = Some(haiku);
        mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
        mgr.set_worker_api_key(None);
        mgr.lead_turns = 0;
    }
    let (_u, _h, cfg) = session.prepare_prompt("hi");
    assert_eq!(cfg.stream_options.api_key.as_deref(), Some("sk-ant-fresh"));
}

#[tokio::test]
async fn prepare_request_deduplicates_moim_blocks_across_messages() {
    let dir = crate::TempTestDir::new("moim_dedup");
    let session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session
        .todo_state
        .write()
        .await
        .add("active task".to_string());

    let (user, _h, config) = session.prepare_prompt("latest user prompt");
    let hook = config.prepare_request.expect("prepare hook");
    let stale_msg = Message::User(UserMessage {
        content: UserContent::Text(
            "old prompt\n\n<working_checklist>\nstale\n</working_checklist>".to_string(),
        ),
        timestamp: now_ms(),
    });
    let dup_latest = match user {
        Message::User(mut u) => {
            u.content = UserContent::Text(
                "latest\n\n<working_checklist>\ndup1\n</working_checklist>\n\n<working_checklist>\ndup2\n</working_checklist>".to_string(),
            );
            Message::User(u)
        }
        other => other,
    };
    let ctx = mehh_agent::types::PrepareRequestContext {
        context: mehh_agent::types::AgentContext {
            messages: Arc::new(vec![stale_msg, dup_latest]),
            tools: None,
        },
        model: faux_model(),
        thinking_level: None,
    };
    let update = hook(ctx, CancellationToken::new()).await.expect("update");
    let out_msgs = update.context.expect("context").messages;
    let total_moim_count: usize = out_msgs
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(u.content.text().matches("<working_checklist>").count()),
            _ => None,
        })
        .sum();
    assert_eq!(
        total_moim_count, 1,
        "exactly one MOIM block must remain across all messages: {out_msgs:?}"
    );
}

#[tokio::test]
async fn reload_resyncs_permissions_and_fails_closed_on_corrupt_settings() {
    let dir = crate::TempTestDir::new("reload_sync");
    let mehh_dir = dir.path().join(".mehh");
    std::fs::create_dir_all(&mehh_dir).unwrap();
    std::fs::write(mehh_dir.join("settings.json"), r#"{"permissionMode":"chat"}"#).unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        true,
    );
    session.reload().unwrap();
    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Chat
    );

    std::fs::write(mehh_dir.join("settings.json"), "{corrupt").unwrap();
    assert!(
        session.reload().is_err(),
        "reload must fail closed on corrupt project settings JSON"
    );
}

#[test]
fn reload_preserves_explicit_tool_allowlists() {
    let dir = crate::TempTestDir::new("reload_allowlist");
    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        true,
    );

    let all_tools = mehh_tools::create_coding_tools(dir.path().to_path_buf());
    let read_only: Vec<Arc<dyn AgentTool>> = all_tools
        .iter()
        .filter(|t| t.name() == "read")
        .cloned()
        .collect();
    session.set_tools(read_only.clone());
    session.set_subagent_with_child_tools(read_only);

    session.reload().unwrap();

    let parent_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    assert!(
        !parent_names.contains(&"web_search") && !parent_names.contains(&"web_fetch"),
        "reload must not re-inject excluded web tools into parent tools: {parent_names:?}"
    );
    let child_names: Vec<&str> = session
        .subagent_child_tools()
        .expect("explicit child tools")
        .iter()
        .map(|t| t.name())
        .collect();
    assert!(
        !child_names.contains(&"web_search") && !child_names.contains(&"web_fetch"),
        "reload must not re-inject excluded web tools into subagent child tools: {child_names:?}"
    );

    // Allowing only web_fetch preserves web_fetch on reload without adding web_search.
    let with_fetch: Vec<Arc<dyn AgentTool>> =
        mehh_tools::create_coding_tools_with_web(dir.path().to_path_buf(), Default::default())
            .into_iter()
            .filter(|t| matches!(t.name(), "read" | "web_fetch"))
            .collect();
    session.set_tools(with_fetch);
    session.reload().unwrap();
    let names_after: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    assert!(names_after.contains(&"web_fetch"), "{names_after:?}");
    assert!(!names_after.contains(&"web_search"), "{names_after:?}");
}

#[tokio::test]
async fn compact_and_navigate_tree_respect_active_cancel() {
    let dir = crate::TempTestDir::new("cancel_compact_nav");
    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    let u1 = session
        .session_mut()
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("u1".into()),
            timestamp: now_ms(),
        }))
        .unwrap();
    let _a1 = session
        .session_mut()
        .append_message(Message::Assistant(mehh_ai::faux_assistant_message("a1")))
        .unwrap();

    let pre_cancel = CancellationToken::new();
    pre_cancel.cancel();
    *session.active_cancel_slot().lock().unwrap() = Some(pre_cancel);
    let err = session
        .compact(None)
        .await
        .expect_err("compact must abort when active_cancel is cancelled");
    assert!(err.to_string().contains("aborted"));
    *session.active_cancel_slot().lock().unwrap() = None;

    // Branch back to u1 and add branch B, then test navigate_tree cancellation
    session.session_mut().branch_from(&u1).unwrap();
    let _u2 = session
        .session_mut()
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("u2".into()),
            timestamp: now_ms(),
        }))
        .unwrap();
    let pre_cancel_nav = CancellationToken::new();
    pre_cancel_nav.cancel();
    *session.active_cancel_slot().lock().unwrap() = Some(pre_cancel_nav);
    let err_nav = session
        .navigate_tree(&u1, true, None)
        .await
        .expect_err("navigate_tree must abort when active_cancel is cancelled");
    assert!(err_nav.to_string().contains("aborted"));
    *session.active_cancel_slot().lock().unwrap() = None;
}

#[tokio::test]
async fn reload_preserves_session_rules_and_strict_active_mode() {
    let _env_lock = crate::ENV_MUTEX.lock().await;
    struct EnvGuard {
        userprofile: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match &self.home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    let _guard = EnvGuard {
        userprofile: std::env::var_os("USERPROFILE"),
        home: std::env::var_os("HOME"),
    };

    let dir = crate::TempTestDir::new("reload_session_rules_mode");
    std::env::set_var("USERPROFILE", dir.path());
    std::env::set_var("HOME", dir.path());

    let mehh_dir = dir.path().join(".mehh");
    std::fs::create_dir_all(&mehh_dir).unwrap();
    // Settings omit `permissionMode`.
    std::fs::write(mehh_dir.join("settings.json"), "{}").unwrap();

    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        true,
    );
    session
        .inspection_manager
        .set_mode(crate::tool_inspection::PermissionMode::Chat)
        .await;

    session
        .inspection_manager
        .try_add_session_permission_rule(crate::permissions::PermissionRule {
            tool_name: "read".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: crate::permissions::PermissionAction::AlwaysAllow,
        })
        .await
        .unwrap();
    session
        .inspection_manager
        .try_add_session_permission_rule(crate::permissions::PermissionRule {
            tool_name: "web_fetch".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: crate::permissions::PermissionAction::NeverAllow,
        })
        .await
        .unwrap();

    session.reload().unwrap();

    assert_eq!(
        session.inspection_manager.mode().await,
        crate::tool_inspection::PermissionMode::Chat,
        "reload must preserve strict active PermissionMode::Chat when settings omit permissionMode"
    );
    let preserved = session.inspection_manager.try_snapshot_session_rules();
    assert_eq!(preserved.len(), 2, "reload must preserve session_rules: {preserved:?}");

    let mut fetch_args = serde_json::Map::new();
    fetch_args.insert("url".to_string(), serde_json::json!("https://example.com"));
    let fetch_ctx = crate::tool_inspection::ToolInspectionContext {
        tool_call_id: "c1".to_string(),
        tool_name: "web_fetch".to_string(),
        args: fetch_args,
        is_read_only: Some(true),
        cwd: dir.path().to_path_buf(),
        origin: None,
    };
    assert!(
        matches!(
            session.inspection_manager.inspect(&fetch_ctx).await,
            crate::tool_inspection::InspectionVerdict::Deny { .. }
        ),
        "preserved session NeverAllow rule must remain active after reload"
    );
}

#[tokio::test]
async fn prepare_request_inserts_fallback_moim_user_after_leading_system_messages() {
    let dir = crate::TempTestDir::new("moim_after_sys");
    let session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session
        .todo_state
        .write()
        .await
        .add("active task".to_string());

    let (_u, _h, config) = session.prepare_prompt("hi");
    let hook = config.prepare_request.expect("prepare hook");
    let sys = Message::System(mehh_ai::SystemMessage {
        content: "compaction summary".to_string(),
        timestamp: now_ms(),
    });
    let ast = Message::Assistant(mehh_ai::faux_assistant_message("earlier assistant"));
    let ctx = mehh_agent::types::PrepareRequestContext {
        context: mehh_agent::types::AgentContext {
            messages: Arc::new(vec![sys, ast]),
            tools: None,
        },
        model: faux_model(),
        thinking_level: None,
    };
    let update = hook(ctx, CancellationToken::new()).await.expect("update");
    let out_msgs = update.context.expect("context").messages;
    assert_eq!(out_msgs.len(), 3);
    assert!(
        matches!(out_msgs[0], Message::System(_)),
        "System summary must stay at index 0: {out_msgs:?}"
    );
    assert!(
        matches!(out_msgs[1], Message::User(_)),
        "Fallback MOIM User must be inserted after leading System message: {out_msgs:?}"
    );
    assert!(matches!(out_msgs[2], Message::Assistant(_)));
}

#[test]
fn can_retry_overflow_from_scratch_checks_clean_turn_zero() {
    let u = Message::User(UserMessage {
        content: UserContent::Text("hi".to_string()),
        timestamp: 1,
    });
    let a = Message::Assistant(mehh_ai::faux_assistant_message("err"));
    assert!(can_retry_overflow_from_scratch(&[u.clone(), a.clone()]));
    assert!(!can_retry_overflow_from_scratch(std::slice::from_ref(&a)));
    assert!(!can_retry_overflow_from_scratch(&[
        u.clone(),
        a.clone(),
        u.clone(),
        a.clone(),
    ]));
    assert!(!can_retry_overflow_from_scratch(&[a.clone(), a]));
}

#[tokio::test]
async fn prompt_content_overflow_after_non_tool_turn_preserves_prefix_via_case_b() {
    let dir = crate::TempTestDir::new("c11_non_tool_overflow");
    let mut sm = SessionManager::in_memory(dir.path());
    for i in 0..3 {
        sm.append_message(Message::User(UserMessage {
            content: UserContent::Text(format!("prior {i}: {}", "word ".repeat(600))),
            timestamp: now_ms(),
        }))
        .unwrap();
        sm.append_message(Message::Assistant(mehh_ai::faux_assistant_message(
            format!("reply {i}: {}", "word ".repeat(600)),
        )))
        .unwrap();
    }

    // Turn 0 succeeds without tool calls; follow-up drains into Turn 1 which overflows;
    // Compaction summarizes history; Case B continuation produces final reply.
    let faux = Arc::new(FauxProvider::with_responses(vec![
        mehh_ai::faux_assistant_message("Turn 0 completed without tools"),
        mehh_ai::AssistantMessage {
            content: Vec::new(),
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
        mehh_ai::faux_assistant_message("Continued after follow-up overflow"),
    ]));

    let mut model = faux_model();
    model.context_window = 200_000;
    let mut session = AgentSession::new(
        dir.path().to_path_buf(),
        model,
        faux,
        sm,
        Settings {
            auto_compact: None,
            ..Settings::default()
        },
    );
    session.follow_up("follow-up task");

    let msgs = session.prompt("initial task", None).await.unwrap();
    let texts: Vec<String> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::Assistant(a) => Some(a.text_content()),
            Message::User(u) => Some(u.content.text()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "Turn 0 completed without tools"),
        "Case B must preserve Turn 0 reply: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "follow-up task"),
        "Case B must preserve drained follow-up message: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t == "Continued after follow-up overflow"),
        "Case B continuation must complete: {texts:?}"
    );
}

#[tokio::test]
async fn before_tool_call_and_codemode_abort_on_cancelled_token() {
    struct SlowDenyHandler;
    #[async_trait::async_trait]
    impl crate::tool_inspection::ApprovalHandler for SlowDenyHandler {
        async fn request_approval(
            &self,
            _ctx: &crate::tool_inspection::ToolInspectionContext,
            _reason: &str,
            _preview: Option<&str>,
        ) -> bool {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            true
        }
    }

    let dir = crate::TempTestDir::new("i7_cancel_tools");
    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings {
            permission_mode: Some(crate::tool_inspection::PermissionMode::Approve),
            ..Default::default()
        },
        true,
    );
    session.set_approval_handler(Some(Arc::new(SlowDenyHandler)));

    // 1. before_tool_call hook must obey cancelled token passed to it
    let (_u, _h, cfg) = session.prepare_prompt("test");
    let before_hook = cfg.before_tool_call.expect("before_tool_call");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("foo.txt"));
    let bctx = mehh_agent::BeforeToolCallContext {
        assistant_message: mehh_ai::faux_assistant_message(""),
        tool_call: mehh_ai::ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: args.clone(),
            arguments_error: None,
            thought_signature: None,
        },
        args,
        context: mehh_agent::AgentContext {
            messages: Arc::new(Vec::new()),
            tools: Some(session.tools.clone()),
        },
    };
    let res = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        before_hook(bctx, cancel),
    )
    .await
    .expect("before_tool_call must abort promptly when cancelled");
    assert!(res.block);
    assert!(res
        .reason
        .as_deref()
        .is_some_and(|r| r.contains("cancelled")));

    // 2. codemode interceptor must obey active_cancel slot
    session.enable_mcp_and_codemode(Vec::new(), true);
    let codemode = session.codemode_tool().expect("codemode tool");
    let cm_cancel = CancellationToken::new();
    cm_cancel.cancel();
    *session.active_cancel_slot().lock().unwrap() = Some(cm_cancel);
    let mut cm_args = serde_json::Map::new();
    cm_args.insert(
        "code".to_string(),
        serde_json::json!(r#"await tools.write({ path: "foo.txt", content: "hi" });"#),
    );
    let cm_res = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        codemode.execute("cm_1", cm_args, CancellationToken::new(), None),
    )
    .await
    .expect("codemode must abort promptly when active_cancel is cancelled");
    let cm_text = match cm_res {
        Ok(r) => match &r.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => String::new(),
        },
        Err(e) => e,
    };
    assert!(
        cm_text.contains("cancelled"),
        "codemode nested inspection must abort with cancelled reason, got: {cm_text}"
    );
}

#[tokio::test]
async fn active_cancel_guard_cancels_and_clears_slot_when_future_dropped() {
    struct HangProvider {
        captured: Arc<Mutex<Option<CancellationToken>>>,
        started: Arc<tokio::sync::Notify>,
        senders: Arc<Mutex<Vec<mehh_ai::AssistantEventSender>>>,
    }
    impl LlmProvider for HangProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: &mehh_ai::Context,
            _options: &mehh_ai::StreamOptions,
            cancel: CancellationToken,
        ) -> mehh_ai::AssistantEventStream {
            *self.captured.lock().unwrap() = Some(cancel);
            let (tx, rx) = mehh_ai::create_event_stream();
            self.senders.lock().unwrap().push(tx);
            self.started.notify_one();
            rx
        }
    }

    let dir = crate::TempTestDir::new("i7_guard_drop");
    let captured = Arc::new(Mutex::new(None));
    let started = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(HangProvider {
        captured: captured.clone(),
        started: started.clone(),
        senders: Arc::new(Mutex::new(Vec::new())),
    });
    let mut session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        faux_model(),
        provider,
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    session
        .session_mut()
        .append_message(Message::User(UserMessage {
            content: UserContent::Text("u1".into()),
            timestamp: now_ms(),
        }))
        .unwrap();
    session
        .session_mut()
        .append_message(Message::Assistant(mehh_ai::faux_assistant_message("a1")))
        .unwrap();

    let slot = session.active_cancel_slot();
    {
        let fut = session.compact(None);
        tokio::pin!(fut);
        tokio::select! {
            _ = &mut fut => panic!("HangProvider should never finish"),
            _ = started.notified() => {}
        }
        assert!(slot.lock().unwrap().is_some());
    }
    // `fut` was dropped above; guard must have cancelled token AND cleared slot.
    assert!(
        slot.lock().unwrap().is_none(),
        "active_cancel slot must be cleared on dropped future"
    );
    let tok = captured.lock().unwrap().take().expect("captured token");
    assert!(
        tok.is_cancelled(),
        "active cancel token must be cancelled when future is dropped"
    );
}

#[test]
fn is_overflow_turn_uses_worker_context_window_for_worker_model() {
    let dir = crate::TempTestDir::new("i9_worker_overflow");
    let mut lead = faux_model();
    lead.id = "lead-big".to_string();
    lead.provider = "anthropic".to_string();
    lead.context_window = 200_000;

    let mut worker = faux_model();
    worker.id = "worker-small".to_string();
    worker.provider = "openai".to_string();
    worker.context_window = 16_000;

    let session = AgentSession::new_with_trust(
        dir.path().to_path_buf(),
        lead.clone(),
        Arc::new(FauxProvider::new()),
        SessionManager::in_memory(dir.path()),
        Settings::default(),
        false,
    );
    {
        let mut lw = session.lead_worker.lock().unwrap();
        lw.worker_model = Some(worker.clone());
        lw.worker_provider = Some(Arc::new(FauxProvider::new()));
    }

    let worker_ast = mehh_ai::AssistantMessage {
        content: Vec::new(),
        api: worker.api.clone(),
        provider: worker.provider.clone(),
        model: worker.id.clone(),
        response_id: None,
        usage: mehh_ai::Usage {
            input: 20_000,
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };
    assert!(
        session.is_overflow_turn(&[Message::Assistant(worker_ast)]),
        "worker overflow above worker 16k window must be detected even though lead window is 200k"
    );

    let lead_ast = mehh_ai::AssistantMessage {
        content: Vec::new(),
        api: lead.api.clone(),
        provider: lead.provider.clone(),
        model: lead.id.clone(),
        response_id: None,
        usage: mehh_ai::Usage {
            input: 20_000,
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };
    assert!(
        !session.is_overflow_turn(&[Message::Assistant(lead_ast)]),
        "lead usage of 20k is well within lead 200k window"
    );
}
