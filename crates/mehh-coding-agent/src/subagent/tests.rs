use mehh_agent::AgentTool;
use mehh_ai::{Context, Message, StopReason, UserContent};
use crate::subagent_lifecycle::SubagentFailureClass;

use super::*;
use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};

#[test]
fn finalize_wait_is_bounded_by_remaining_wall_clock_budget() {
    assert_eq!(
        finalize_wait_budget(Duration::from_secs(10)),
        Duration::from_secs(10)
    );
    assert_eq!(
        finalize_wait_budget(Duration::from_secs(120)),
        Duration::from_secs(SUBAGENT_FINALIZE_TIMEOUT_SECS)
    );
    assert_eq!(finalize_wait_budget(Duration::ZERO), Duration::ZERO);
}

#[tokio::test]
async fn resolved_api_key_reuses_cached_credential() {
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd,
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        Vec::new(),
        0,
    );
    sub.api_key_cache
        .lock()
        .unwrap()
        .insert("anthropic".to_string(), Some("cached-key".to_string()));
    assert_eq!(
        sub.resolved_api_key("anthropic")
            .await
            .unwrap()
            .as_deref(),
        Some("cached-key"),
        "a batch sibling must reuse the resolved credential instead of reading it again"
    );
}

#[tokio::test]
async fn test_nested_subagent_tool_is_rebuilt_at_child_depth() {
    let cwd = std::env::temp_dir();
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("nested ok"));
    // The inherited arc sits at the depth ceiling and would refuse to run.
    let stale = Arc::new(SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux.clone(),
        None,
        mehh_tools::create_coding_tools(cwd.clone()),
        MAX_SUBAGENT_DEPTH,
    ));
    let mut tools: Vec<Arc<dyn AgentTool>> = mehh_tools::create_coding_tools(cwd.clone());
    tools.push(stale);

    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux.clone(),
        None,
        tools.clone(),
        0,
    );
    let corrected = sub.depth_corrected_tools(tools.clone());
    let nested = corrected
        .iter()
        .find(|t| t.name() == "subagent")
        .expect("depth-0 child may expose one nesting level");
    let mut params = Map::new();
    params.insert(
        "prompt".to_string(),
        Value::String("nested task".to_string()),
    );
    let res = nested
        .execute("tc_nested", params, CancellationToken::new(), None)
        .await
        .expect("rebuilt nested tool must run at depth 1, not the stale ceiling depth");
    let text = match &res.content[0] {
        mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
        _ => panic!("Expected text output"),
    };
    assert!(text.starts_with("nested ok"));

    // At the ceiling, the nested tool must be hidden entirely so the LLM
    // never wastes a turn calling it only to receive a depth error.
    let deep = SubagentTool::new(
        cwd,
        faux_model(),
        faux,
        None,
        tools.clone(),
        MAX_SUBAGENT_DEPTH - 1,
    );
    let corrected_deep = deep.depth_corrected_tools(tools);
    assert!(corrected_deep.iter().all(|t| t.name() != "subagent"));
}

#[tokio::test]
async fn lead_failure_during_rescue_does_not_trigger_another_rescue() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let faux = Arc::new(FauxProvider::new());
    for _ in 0..8 {
        let counter = calls.clone();
        faux.add_handler(
            move |model: &mehh_ai::Model,
                  _context: &mehh_ai::Context,
                  _options: &mehh_ai::StreamOptions| {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut msg = faux_assistant_message(format!("failed on {}", model.id));
                msg.stop_reason = StopReason::Error;
                msg.error_message = Some(format!("simulated {} failure", model.id));
                msg
            },
        );
    }

    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    // Lead/Worker routing replaces the tool's provider, so both roles must
    // point at the faux provider that records the calls.
    mgr.lead_provider = faux.clone();
    mgr.worker_provider = Some(faux.clone());
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux,
        None,
        mehh_tools::create_coding_tools(cwd),
        0,
    )
    .with_lead_worker(Arc::new(Mutex::new(mgr)));

    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("do work".into()));
    params.insert("mode".into(), Value::String("coding".into()));
    params.insert("max_turns".into(), Value::from(6));
    let result = sub
        .execute("tc_rescue", params, CancellationToken::new(), None)
        .await
        .expect("the tool reports failures through AgentToolResult.is_error");
    assert_eq!(result.is_error, Some(true), "the run must fail");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one Worker failure plus one Lead rescue only; got {:?}",
        result.content
    );
}

#[tokio::test]
async fn rescue_resolves_the_lead_key_when_providers_differ() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let lead_keys: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let faux = Arc::new(FauxProvider::new());
    for _ in 0..4 {
        let calls = calls.clone();
        let lead_keys = lead_keys.clone();
        faux.add_handler(
            move |model: &mehh_ai::Model,
                  _context: &mehh_ai::Context,
                  options: &mehh_ai::StreamOptions| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if model.id == "lead-model" {
                    lead_keys.lock().unwrap_or_else(PoisonError::into_inner).push(options.api_key.clone());
                    faux_assistant_message("lead recovered")
                } else {
                    let mut msg = faux_assistant_message(format!("worker failed {n}"));
                    msg.stop_reason = StopReason::Error;
                    msg.error_message = Some("simulated worker failure".into());
                    msg
                }
            },
        );
    }

    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    mgr.lead_model.provider = "lead-provider".into();
    mgr.worker_model.as_mut().unwrap().provider = "worker-provider".into();
    mgr.lead_provider = faux.clone();
    mgr.worker_provider = Some(faux.clone());
    // The Lead credential lives only in the shared cache, not on the manager.
    assert_eq!(mgr.lead_api_key, None);

    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux,
        None,
        mehh_tools::create_coding_tools(cwd),
        0,
    )
    .with_lead_worker(Arc::new(Mutex::new(mgr)));
    sub.api_key_cache.lock().unwrap_or_else(PoisonError::into_inner).insert(
        "lead-provider".to_string(),
        Some("lead-cached-key".to_string()),
    );

    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("do work".into()));
    params.insert("mode".into(), Value::String("coding".into()));
    let result = sub
        .execute("tc_key", params, CancellationToken::new(), None)
        .await
        .expect("the Lead rescue must complete");

    assert_ne!(result.is_error, Some(true), "the Lead rescue must succeed");
    assert_eq!(
        lead_keys.lock().unwrap_or_else(PoisonError::into_inner).as_slice(),
        &[Some("lead-cached-key".to_string())],
        "switching to the Lead provider must resolve its cached credential"
    );
}

#[derive(Default)]
struct PendingProvider {
    senders: std::sync::Mutex<Vec<mehh_ai::AssistantEventSender>>,
}

#[async_trait::async_trait]
impl LlmProvider for PendingProvider {
    fn stream(
        &self,
        _model: &mehh_ai::Model,
        _context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
        _cancel: CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        let (sender, stream) = mehh_ai::create_event_stream();
        self.senders.lock().unwrap().push(sender);
        stream
    }
}

#[tokio::test]
async fn cancelled_run_does_not_wait_for_a_concurrency_permit() {
    let control = Arc::new(SubagentControl::new(0));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        mehh_tools::create_coding_tools(cwd),
        0,
    )
    .with_control(control);
    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("never start".into()));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sub.execute("tc_cancel", params, cancel, None),
    )
    .await
    .expect("a cancelled run must not block waiting for a permit");
    assert!(result.is_err(), "the cancelled run must fail fast: {result:?}");
}

#[tokio::test]
async fn dropping_a_run_unregisters_its_child() {
    let control = Arc::new(SubagentControl::new(2));
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        Arc::new(PendingProvider::default()),
        None,
        mehh_tools::create_coding_tools(cwd),
        0,
    )
    .with_control(control.clone());
    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("never finish".into()));

    let task = tokio::spawn(async move {
        sub.execute("tc_drop", params, CancellationToken::new(), None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while control.list().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the child must be registered while its run is in flight");
    assert_eq!(control.list().len(), 1);

    task.abort();
    let _ = task.await;
    assert!(
        control.list().is_empty(),
        "a dropped execute() future must not leak a child handle"
    );
}

#[tokio::test]
async fn api_key_cache_stays_usable_after_a_poisoned_lock() {
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd,
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        Vec::new(),
        0,
    );
    sub.api_key_cache
        .lock()
        .unwrap()
        .insert("anthropic".into(), Some("cached-key".into()));
    let victim = sub.api_key_cache.clone();
    let _ = std::thread::spawn(move || {
        let _guard = victim.lock().unwrap_or_else(PoisonError::into_inner);
        panic!("poison the credential cache");
    })
    .join();
    assert_eq!(
        sub.resolved_api_key("anthropic")
            .await
            .unwrap()
            .as_deref(),
        Some("cached-key")
    );
    assert_eq!(
        sub.resolved_api_key(" Anthropic ")
            .await
            .unwrap()
            .as_deref(),
        Some("cached-key")
    );
}

#[test]
fn finalize_terminal_event_keeps_the_batch_task_prefix() {
    let dir = std::env::temp_dir().join(format!("mehh_fin_prefix_{}", uuid::Uuid::now_v7()));
    let mut manifest = new_manifest("sub-prefix".into(), "coding".into(), "m".into(), 1000);
    let events = Mutex::new(manifest.events.clone());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let on_update: AgentToolUpdateCallback = Arc::new(move |result| {
        if let Some(mehh_ai::ToolResultContentBlock::Text(t)) = result.content.first() {
            sink.lock().unwrap_or_else(PoisonError::into_inner).push(t.text.clone());
        }
    });
    SubagentTool::finalize_subagent_run(
        &dir,
        &mut manifest,
        &events,
        Ok("done".into()),
        &CancellationToken::new(),
        Some(on_update),
        "#2:",
    )
    .unwrap();
    assert!(
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .first()
            .is_some_and(|text| text.starts_with("[subagent:#2:")),
        "the terminal event must keep the batch task prefix: {seen:?}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn finalize_classifies_typed_timeout_but_not_a_tool_timeout_message() {
    let dir = std::env::temp_dir().join(format!("mehh_fin_typed_{}", uuid::Uuid::now_v7()));
    let mut manifest = new_manifest("sub-typed".into(), "research".into(), "m".into(), 1000);
    let events = Mutex::new(manifest.events.clone());
    let cancel = CancellationToken::new();
    let err = SubagentTool::finalize_subagent_run(
        &dir,
        &mut manifest,
        &events,
        Err(ChildRunError::ToolFailure("bash timed out after 5s".into())),
        &cancel,
        None,
        "",
    )
    .unwrap_err();
    assert_eq!(manifest.failure_class, Some(SubagentFailureClass::ToolRuntime));
    assert!(
        err.contains("bash timed out"),
        "the original tool error must survive: {err}"
    );

    let mut manifest2 = new_manifest("sub-typed-2".into(), "research".into(), "m".into(), 1000);
    let events2 = Mutex::new(manifest2.events.clone());
    let _ = SubagentTool::finalize_subagent_run(
        &dir,
        &mut manifest2,
        &events2,
        Err(ChildRunError::TimedOut(std::time::Duration::from_secs(1))),
        &cancel,
        None,
        "",
    );
    assert_eq!(manifest2.failure_class, Some(SubagentFailureClass::Timeout));
    assert_eq!(
        manifest2.events.last().unwrap().kind,
        SubagentEventKind::TimedOut
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn finalize_subagent_run_keeps_original_error_when_manifest_write_fails() {
    let bad_cwd = std::env::temp_dir().join(format!("mehh_fin_bad_{}", uuid::Uuid::now_v7()));
    std::fs::write(&bad_cwd, "not a directory").unwrap();
    let mut manifest = new_manifest("sub-fin-err".into(), "research".into(), "m".into(), 1000);
    let events = Mutex::new(manifest.events.clone());
    let cancel = CancellationToken::new();
    let err = SubagentTool::finalize_subagent_run(
        &bad_cwd,
        &mut manifest,
        &events,
        Err(ChildRunError::ModelError("child exploded".into())),
        &cancel,
        None,
        "",
    )
    .unwrap_err();
    let _ = std::fs::remove_file(&bad_cwd);
    assert!(
        err.contains("child exploded"),
        "the original outcome error must survive a write failure, got: {err}"
    );
    assert!(
        err.contains("manifest write failed"),
        "the write failure must also be reported, got: {err}"
    );
}

#[test]
fn finalize_subagent_run_reports_manifest_write_failure_after_success() {
    let bad_cwd = std::env::temp_dir().join(format!("mehh_fin_ok_{}", uuid::Uuid::now_v7()));
    std::fs::write(&bad_cwd, "not a directory").unwrap();
    let mut manifest = new_manifest("sub-fin-ok".into(), "research".into(), "m".into(), 1000);
    let events = Mutex::new(manifest.events.clone());
    let cancel = CancellationToken::new();
    let err = SubagentTool::finalize_subagent_run(
        &bad_cwd,
        &mut manifest,
        &events,
        Ok("done".to_string()),
        &cancel,
        None,
        "",
    )
    .unwrap_err();
    let _ = std::fs::remove_file(&bad_cwd);
    assert!(
        err.contains("manifest write failed"),
        "a write failure after success must be an error, got: {err}"
    );
    assert!(
        err.contains("completed"),
        "the error must state the run itself completed, got: {err}"
    );
}

#[tokio::test]
async fn test_subagent_runs_isolated_prompt_and_respects_max_depth() {
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("Found 3 matching files."));

    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux.clone(),
        None,
        tools.clone(),
        0,
    );
    assert!(sub
        .filter_tools_for_mode("research")
        .iter()
        .all(|t| matches!(
            t.name(),
            "read" | "grep" | "find" | "ls" | "analyze" | "web_search" | "web_fetch"
        )));

    let mut params = Map::new();
    params.insert(
        "prompt".to_string(),
        Value::String("Inspect crates".to_string()),
    );
    let res = sub
        .execute("tc_sub_1", params.clone(), CancellationToken::new(), None)
        .await
        .unwrap();
    let text = match &res.content[0] {
        mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
        _ => panic!("Expected text output"),
    };
    assert!(text.starts_with("Found 3 matching files."));
    assert!(text.contains("[subagent-meta] status=completed"));

    // Depth at MAX_SUBAGENT_DEPTH fails closed
    let deep = SubagentTool::new(cwd, faux_model(), faux, None, tools, MAX_SUBAGENT_DEPTH);
    let err = deep
        .execute("tc_sub_2", params, CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(err.contains("Maximum subagent nesting depth"));
}

#[tokio::test]
async fn batch_details_lists_every_child_with_its_own_terminal_status() {
    // Route by task text so each child's outcome is deterministic even
    // though batch children are spawned concurrently.
    fn route(
        _model: &mehh_ai::Model,
        context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
    ) -> mehh_ai::AssistantMessage {
        let is_fail = context
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(t) => Some(t.contains("FAIL")),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(false);
        if is_fail {
            let mut msg = faux_assistant_message("child failed");
            msg.stop_reason = StopReason::Error;
            msg.error_message = Some("simulated failure".to_string());
            msg
        } else {
            faux_assistant_message("child ok")
        }
    }

    let faux = Arc::new(FauxProvider::new());
    faux.add_handler(route);
    faux.add_handler(route);
    faux.add_handler(route);

    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let sub = SubagentTool::new(cwd, faux_model(), faux, None, tools, 0);

    let mut params = Map::new();
    params.insert(
        "tasks".to_string(),
        Value::Array(vec![
            Value::String("OK one".into()),
            Value::String("FAIL two".into()),
            Value::String("OK three".into()),
        ]),
    );
    let res = sub
        .execute("tc_batch", params, CancellationToken::new(), None)
        .await
        .unwrap();

    let details = res.details.expect("batch result must carry details");
    let sub_details = details.get("subagent").expect("subagent details");
    assert_eq!(
        sub_details.get("status").and_then(|v| v.as_str()),
        Some("failed"),
        "aggregate status reflects any failure"
    );
    // Legacy keys still point at the last child.
    assert_eq!(
        sub_details.get("child_id").and_then(|v| v.as_str()),
        sub_details
            .get("children")
            .and_then(|v| v.as_array())
            .and_then(|a| a.last())
            .and_then(|c| c.get("child_id"))
            .and_then(|v| v.as_str()),
        "legacy child_id stays the last child"
    );

    let children = sub_details
        .get("children")
        .and_then(|v| v.as_array())
        .expect("children array");
    assert_eq!(children.len(), 3, "one entry per task");

    let mut statuses = std::collections::HashMap::new();
    for child in children {
        let id = child
            .get("child_id")
            .and_then(|v| v.as_str())
            .expect("child_id")
            .to_string();
        let status = child
            .get("status")
            .and_then(|v| v.as_str())
            .expect("status")
            .to_string();
        assert!(
            child.get("manifest").and_then(|v| v.as_str()).is_some(),
            "each child carries its own manifest path"
        );
        assert!(
            statuses.insert(id, status).is_none(),
            "child ids are unique"
        );
    }
    let failed = statuses
        .iter()
        .filter(|(_, s)| s.as_str() == "failed")
        .count();
    let completed = statuses
        .iter()
        .filter(|(_, s)| s.as_str() == "completed")
        .count();
    assert_eq!(failed, 1, "the FAIL task is failed: {statuses:?}");
    assert_eq!(completed, 2, "both OK tasks are completed: {statuses:?}");
}

#[tokio::test]
async fn batch_rejects_oversized_task_arrays_before_spawning() {
    let cwd = std::env::temp_dir();
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        mehh_tools::create_coding_tools(cwd),
        0,
    );

    let tasks: Vec<Value> = (0..100)
        .map(|i| Value::String(format!("task {i}")))
        .collect();
    let mut params = Map::new();
    params.insert("tasks".to_string(), Value::Array(tasks));

    let err = sub
        .execute("tc_batch_cap", params, CancellationToken::new(), None)
        .await
        .expect_err("an oversized batch must be rejected before any work is spawned");
    assert!(
        err.contains(&MAX_BATCH_TASKS.to_string()),
        "the error should state the batch limit: {err}"
    );
}

#[test]
fn append_web_tool_guidance_tracks_attached_web_tools() {
    let cwd = std::env::temp_dir();
    let full_tools = mehh_tools::create_coding_tools(cwd.clone());
    let mut both = String::from("Base.");
    append_web_tool_guidance(&mut both, &full_tools);
    assert!(both.contains("web_search"));
    assert!(both.contains("web_fetch"));

    let search_only: Vec<Arc<dyn AgentTool>> = full_tools
        .iter()
        .filter(|t| t.name() == "web_search")
        .cloned()
        .collect();
    let mut s_only = String::from("Base.");
    append_web_tool_guidance(&mut s_only, &search_only);
    assert!(s_only.contains("web_search"));
    assert!(!s_only.contains("web_fetch"));

    let fetch_only: Vec<Arc<dyn AgentTool>> = full_tools
        .iter()
        .filter(|t| t.name() == "web_fetch")
        .cloned()
        .collect();
    let mut f_only = String::from("Base.");
    append_web_tool_guidance(&mut f_only, &fetch_only);
    assert!(f_only.contains("web_fetch"));
    assert!(!f_only.contains("web_search"));

    let offline: Vec<Arc<dyn AgentTool>> = full_tools
        .iter()
        .filter(|t| t.name() == "read")
        .cloned()
        .collect();
    let mut none = String::from("Base.");
    append_web_tool_guidance(&mut none, &offline);
    assert_eq!(none, "Base.");
}

#[test]
fn filter_tools_for_mode_excludes_tool_search() {
    let cwd = std::env::temp_dir();
    let mut tools = mehh_tools::create_coding_tools(cwd.clone());
    let catalog = Arc::new(crate::tool_search::DeferredToolCatalog::new(Vec::new()));
    tools.push(Arc::new(crate::tool_search::ToolSearchTool::new(catalog)));

    let sub = SubagentTool::new(cwd, faux_model(), Arc::new(FauxProvider::new()), None, tools, 0);
    for mode in ["coding", "research", "review"] {
        let filtered = sub.filter_tools_for_mode(mode);
        assert!(
            filtered.iter().all(|t| t.name() != "tool_search"),
            "mode {mode} must exclude tool_search"
        );
    }
}

#[tokio::test]
async fn lead_rescue_strips_trailing_empty_error_assistant_message() {
    let faux = Arc::new(FauxProvider::new());
    faux.add_handler(|model, _ctx, _opts| {
        if model.id == "worker-model" {
            let mut err_msg = faux_assistant_message("");
            err_msg.stop_reason = StopReason::Error;
            err_msg.error_message = Some("worker exploded".to_string());
            err_msg
        } else {
            faux_assistant_message("lead rescued")
        }
    });
    faux.add_handler(|_model, _ctx, _opts| faux_assistant_message("lead rescued"));

    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    mgr.lead_provider = faux.clone();
    mgr.worker_provider = Some(faux.clone());

    let cwd = std::env::temp_dir().join(format!("mehh_sub_strip_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(
        cwd.clone(),
        faux_model(),
        faux.clone(),
        None,
        Vec::new(),
        0,
    )
    .with_lead_worker(Arc::new(Mutex::new(mgr)));

    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("fix bug".into()));
    params.insert("mode".into(), Value::String("coding".into()));
    let res = sub
        .execute("tc_strip", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true));

    let reqs = faux.recorded_requests();
    assert_eq!(reqs.len(), 2);
    let lead_msgs = &reqs[1].context.messages;
    assert!(
        lead_msgs.iter().all(|m| !matches!(
            m,
            Message::Assistant(a) if a.stop_reason == StopReason::Error && a.text_content().trim().is_empty()
        )),
        "trailing empty error assistant message must be stripped before lead rescue: {lead_msgs:?}"
    );
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn force_finalize_preserves_aborted_and_timed_out_statuses() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_fin_abort_{}", uuid::Uuid::now_v7()));
    let cancel = CancellationToken::new();
    let cancel_on_finalize = cancel.clone();
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(mehh_ai::faux_tool_call_message(vec![mehh_ai::faux_tool_call(
        "c1",
        "ls",
        json!({ "path": dir.to_string_lossy() }),
    )]));
    faux.add_handler(move |_m, _ctx, _opts| {
        cancel_on_finalize.cancel();
        let mut msg = faux_assistant_message("");
        msg.stop_reason = StopReason::Aborted;
        msg
    });

    std::fs::create_dir_all(&dir).unwrap();
    let tools = mehh_tools::create_coding_tools(dir.clone());
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, tools, 0);
    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("inspect".into()));
    params.insert("max_turns".into(), json!(1));
    let res = sub
        .execute("tc_fin_abort", params, cancel, None)
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "aborted force-finalize must not overwrite status with completed"
    );
    assert_eq!(
        res.details.as_ref().and_then(|d| d["subagent"]["status"].as_str()),
        Some("failed")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn batch_pre_permit_cancel_writes_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_preperm_{}", uuid::Uuid::now_v7()));
    let control = Arc::new(SubagentControl::new(0));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(FauxProvider::new()),
        None,
        Vec::new(),
        0,
    )
    .with_control(control);

    let mut params = Map::new();
    params.insert("tasks".into(), json!(["task one"]));
    let res = sub
        .execute("tc_preperm", params, cancel, None)
        .await
        .unwrap();
    assert_eq!(res.is_error, Some(true));
    let manifest_path = res.details.as_ref().unwrap()["subagent"]["children"][0]["manifest"]
        .as_str()
        .expect("manifest path in batch details");
    assert!(
        std::path::Path::new(manifest_path).exists(),
        "pre-permit aborted batch child must persist its manifest: {manifest_path}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn run_child_prompt_preserves_typed_child_error_when_manifest_writes_ok() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_typed_err_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(PendingProvider::default()),
        None,
        Vec::new(),
        0,
    );
    let mut manifest = new_manifest("sub-typed-abort".into(), "research".into(), "m".into(), 1000);
    let events = Arc::new(Mutex::new(manifest.events.clone()));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = match sub
        .run_child_prompt(
            ChildRunParams {
                prompt: "cancelled task",
                mode: "research",
                custom_sys: None,
                max_turns: 2,
                task_id: None,
                model_tier: Some("auto"),
                timeout: std::time::Duration::from_secs(5),
                steering_queue: None,
            },
            cancel,
            None,
            &mut manifest,
            events,
        )
        .await
    {
        Ok(_) => panic!("cancelled run must return an error"),
        Err(err) => err,
    };

    assert!(
        matches!(err, ChildRunError::Aborted),
        "expected ChildRunError::Aborted, got {err:?}"
    );
    assert_eq!(manifest.failure_class, Some(SubagentFailureClass::Aborted));
    assert!(agent_store_dir(&dir).join("sub-typed-abort.json").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn i10_force_finalize_error_returns_model_error_and_uses_active_lead_worker_model() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_fin_err_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();

    let faux = Arc::new(FauxProvider::new());
    // Turn 1: worker produces a tool call, hitting max_turns=1 and escalating / keeping active model.
    faux.add_handler(|model, _ctx, _opts| {
        let mut msg = mehh_ai::faux_tool_call_message(vec![mehh_ai::faux_tool_call(
            "c1",
            "ls",
            json!({ "path": "." }),
        )]);
        msg.model = model.id.clone();
        msg
    });
    // Force-finalize call: fails with StopReason::Error
    faux.add_handler(|model, _ctx, _opts| {
        let mut err = faux_assistant_message("");
        err.model = model.id.clone();
        err.stop_reason = StopReason::Error;
        err.error_message = Some(format!("finalize failed on {}", model.id));
        err
    });

    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    mgr.lead_provider = faux.clone();
    mgr.worker_provider = Some(faux.clone());

    let tools = mehh_tools::create_coding_tools(dir.clone());
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux.clone(), None, tools, 0)
        .with_lead_worker(Arc::new(Mutex::new(mgr)));
    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("inspect".into()));
    params.insert("mode".into(), Value::String("coding".into()));
    params.insert("max_turns".into(), json!(1));
    let res = sub
        .execute("tc_fin_err", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "force-finalize StopReason::Error must fail the subagent run: {:?}",
        res.content
    );
    let manifest_path = res.details.as_ref().unwrap()["subagent"]["manifest"]
        .as_str()
        .unwrap();
    let manifest_raw = std::fs::read_to_string(manifest_path).unwrap();
    let manifest: SubagentManifest = serde_json::from_str(&manifest_raw).unwrap();
    assert_eq!(
        manifest.model_id, "worker-model",
        "manifest.model_id must reflect active lead_worker model"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn i10_force_finalize_stream_err_returns_model_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct StreamErrOnFinalizeProvider(AtomicUsize);
    #[async_trait::async_trait]
    impl LlmProvider for StreamErrOnFinalizeProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: &mehh_ai::StreamOptions,
            _cancel: CancellationToken,
        ) -> mehh_ai::AssistantEventStream {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            let (sender, stream) = mehh_ai::create_event_stream();
            if n == 0 {
                let msg = mehh_ai::faux_tool_call_message(vec![mehh_ai::faux_tool_call(
                    "c1",
                    "ls",
                    json!({ "path": "." }),
                )]);
                sender.push(mehh_ai::AssistantMessageEvent::Done {
                    reason: StopReason::ToolUse,
                    message: msg,
                });
            } else {
                let mut err_msg = faux_assistant_message("");
                err_msg.stop_reason = StopReason::Error;
                err_msg.error_message = Some("finalize stream failed".to_string());
                sender.push(mehh_ai::AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: err_msg,
                });
            }
            stream
        }
    }

    let dir = std::env::temp_dir().join(format!("mehh_sub_fin_stream_err_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let tools = mehh_tools::create_coding_tools(dir.clone());
    let sub = SubagentTool::new(
        dir.clone(),
        faux_model(),
        Arc::new(StreamErrOnFinalizeProvider(AtomicUsize::new(0))),
        None,
        tools,
        0,
    );
    let mut params = Map::new();
    params.insert("prompt".into(), Value::String("inspect".into()));
    params.insert("max_turns".into(), json!(1));
    let res = sub
        .execute("tc_fin_serr", params, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(res.is_error, Some(true));
    let _ = std::fs::remove_dir_all(dir);
}
