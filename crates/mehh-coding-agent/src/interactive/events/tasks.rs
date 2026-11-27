use super::*;
use super::signals::{ManualCompactedPayload, ModelsDiscoveredPayload, OverflowCompactedPayload, ReadyForTurnPayload};

/// Scans `text` for `@"..."`, `@'...'`, and bare `@<non-whitespace>` tokens at
/// token boundaries (start-of-string or after whitespace) and expands them via
/// `process_file_arguments`, preserving all surrounding quotes, contractions,
/// newlines, indentation, and mid-word `@` (such as email addresses) verbatim.
pub(in crate::interactive) fn expand_prompt_with_files(
    cwd: &Path,
    text: &str,
) -> anyhow::Result<(String, Vec<UserContentBlock>)> {
    let mut out = String::with_capacity(text.len());
    let mut blocks: Vec<UserContentBlock> = Vec::new();
    let mut last_copied = 0usize;
    let mut saw_at_token = false;
    let mut any_expanded = false;
    let mut any_image = false;

    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let at_boundary = i == 0
                || text[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            if at_boundary {
                let after = &text[i + 1..];
                let parsed = if let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') {
                    let rest = &after[1..];
                    rest.find(q).and_then(|end_rel| {
                        let inner = &rest[..end_rel];
                        if inner.contains('\n') || inner.contains('\r') {
                            None
                        } else {
                            Some((inner, i + 1 + 1 + end_rel + 1))
                        }
                    })
                } else {
                    None
                };
                let (path_slice, token_end) = match parsed {
                    Some(pair) => pair,
                    None => {
                        let len = after
                            .char_indices()
                            .find(|&(_, c)| c.is_whitespace())
                            .map(|(idx, _)| idx)
                            .unwrap_or(after.len());
                        (&after[..len], i + 1 + len)
                    }
                };
                if !path_slice.is_empty() {
                    saw_at_token = true;
                    out.push_str(&text[last_copied..i]);
                    let token = format!("@{path_slice}");
                    let processed = crate::process_file_arguments(cwd, std::slice::from_ref(&token))?;
                    if processed
                        .blocks
                        .iter()
                        .any(|b| matches!(b, UserContentBlock::Image(_)))
                    {
                        any_expanded = true;
                        any_image = true;
                        for b in processed.blocks {
                            if matches!(b, UserContentBlock::Image(_)) {
                                blocks.push(b);
                            }
                        }
                    } else if processed.text != token {
                        any_expanded = true;
                        out.push_str(&processed.text);
                    } else {
                        out.push_str(&text[i..token_end]);
                    }
                    last_copied = token_end;
                    i = token_end;
                    continue;
                }
            }
        }
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        i += ch_len;
    }

    if !saw_at_token {
        return Ok((text.to_string(), Vec::new()));
    }

    out.push_str(&text[last_copied..]);
    if any_image && out.trim().is_empty() {
        out.clear();
    }
    if any_expanded && !out.is_empty() {
        blocks.insert(
            0,
            UserContentBlock::Text(mehh_ai::TextContent {
                text: out.clone(),
                text_signature: None,
            }),
        );
    }
    Ok((out, blocks))
}

/// Refreshes credentials and runs optional proactive compaction off the UI thread so the TUI and Ctrl+C remain responsive.
pub(super) fn start_turn_preparation(
    session: &AgentSession,
    text: &str,
    blocks: Vec<UserContentBlock>,
    auth_file: Option<&Path>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> tokio::task::AbortHandle {
    let cancel = CancellationToken::new();
    *mehh_ai::lock_recover(&session.active_cancel_slot()) = Some(cancel.clone());

    let prompt_text = text.to_string();
    let provider_name = session.model.provider.clone();
    let model_api = session.model.api.clone();
    let has_explicit_key = session.has_explicit_api_key();
    let auth_file = auth_file.map(Path::to_path_buf);
    let existing_key = session.api_key.clone();
    let model = session.model.clone();
    let provider = session.provider.clone();
    let proactive_prep = session.prepare_proactive_compaction();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();

    let handle = tokio::spawn(async move {
        let outcome = AssertUnwindSafe(async {
            let key_res = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(AgentSignal::PrepAborted);
                    return;
                }
                res = AgentSession::refresh_resolved_api_key(
                    &provider_name,
                    &model_api,
                    has_explicit_key,
                    auth_file.as_deref(),
                ) => res,
            };
            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
                return;
            }
            let new_api_key = match key_res {
                Ok(k) => k,
                Err(e) => {
                    let _ = tx.send(AgentSignal::PrepFailed(format!(
                        "Failed to resolve provider credentials: {e}"
                    )));
                    return;
                }
            };

            let mut compaction = None;
            let mut warning = None;
            if let Some(prep) = proactive_prep {
                let effective_key = new_api_key.as_deref().or(existing_key.as_deref());
                match crate::compaction::compact_with_provider(
                    &prep,
                    &model,
                    provider.as_ref(),
                    effective_key,
                    None,
                    cancel.clone(),
                )
                .await
                {
                    Ok(summary) => {
                        compaction = Some((summary, prep.first_kept_entry_id, prep.tokens_before));
                    }
                    Err(e) => {
                        if cancel.is_cancelled() {
                            let _ = tx.send(AgentSignal::PrepAborted);
                            return;
                        }
                        warning = Some(e.to_string());
                    }
                }
            }

            if cancel.is_cancelled() {
                let _ = tx.send(AgentSignal::PrepAborted);
            } else {
                let _ = tx.send(AgentSignal::ReadyForTurn(Box::new(ReadyForTurnPayload {
                    prompt_text,
                    blocks,
                    provider_name,
                    new_api_key,
                    compaction,
                    warning,
                    cancel,
                })));
            }
        })
        .catch_unwind()
        .await;

        if outcome.is_err() {
            let _ = tx_panic.send(AgentSignal::PrepFailed(
                "turn preparation panicked".to_string(),
            ));
        }
    });
    handle.abort_handle()
}

pub(super) fn start_overflow_compaction(
    session: &AgentSession,
    prep: crate::compaction::CompactionPreparation,
    original_messages: Vec<Message>,
    prefix_persisted: bool,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    let cancel = CancellationToken::new();
    *mehh_ai::lock_recover(&session.active_cancel_slot()) = Some(cancel.clone());

    let model = session.model.clone();
    let provider = session.provider.clone();
    let api_key = session.api_key.clone();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();
    let prep_meta = (prep.first_kept_entry_id.clone(), prep.tokens_before);

    tokio::spawn(async move {
        let res = AssertUnwindSafe(crate::compaction::compact_with_provider(
            &prep,
            &model,
            provider.as_ref(),
            api_key.as_deref(),
            None,
            cancel.clone(),
        ))
        .catch_unwind()
        .await;

        match res {
            Ok(compact_res) => {
                let cancelled = cancel.is_cancelled();
                let _ = tx.send(AgentSignal::OverflowCompacted(Box::new(
                    OverflowCompactedPayload {
                        original_messages,
                        prep: Some(prep_meta),
                        result: compact_res.map_err(|e| e.to_string()),
                        cancelled,
                        prefix_persisted,
                    },
                )));
            }
            Err(_) => {
                let _ = tx_panic.send(AgentSignal::Failed(
                    "overflow compaction panicked".to_string(),
                ));
            }
        }
    });
}

pub(super) fn start_manual_compaction(
    session: &AgentSession,
    prep: crate::compaction::CompactionPreparation,
    custom_instructions: Option<String>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> tokio::task::AbortHandle {
    let cancel = CancellationToken::new();
    *mehh_ai::lock_recover(&session.active_cancel_slot()) = Some(cancel.clone());

    let model = session.model.clone();
    let provider = session.provider.clone();
    let api_key = session.api_key.clone();
    let tx = agent_tx.clone();
    let tx_panic = agent_tx.clone();
    let prep_meta = (prep.first_kept_entry_id.clone(), prep.tokens_before);

    let handle = tokio::spawn(async move {
        let res = AssertUnwindSafe(crate::compaction::compact_with_provider(
            &prep,
            &model,
            provider.as_ref(),
            api_key.as_deref(),
            custom_instructions.as_deref(),
            cancel.clone(),
        ))
        .catch_unwind()
        .await;

        match res {
            Ok(compact_res) => {
                let cancelled = cancel.is_cancelled();
                let _ = tx.send(AgentSignal::ManualCompacted(Box::new(
                    ManualCompactedPayload {
                        prep: prep_meta,
                        result: compact_res.map_err(|e| e.to_string()),
                        cancelled,
                    },
                )));
            }
            Err(_) => {
                let _ = tx_panic.send(AgentSignal::Failed(
                    "compaction panicked".to_string(),
                ));
            }
        }
    });
    handle.abort_handle()
}

/// Starts an agent turn in a background task and streams its events to the UI loop.
pub(in crate::interactive) fn start_turn(
    session: &AgentSession,
    state: &mut InteractiveState,
    text: &str,
    blocks: Vec<UserContentBlock>,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.turn_start_idx = Some(state.items.len());
    state.last_turn_error = None;
    let mut content_blocks = blocks;
    if (!text.is_empty() || content_blocks.is_empty())
        && !content_blocks
            .iter()
            .any(|b| matches!(b, UserContentBlock::Text(_)))
    {
        content_blocks.insert(
            0,
            UserContentBlock::Text(mehh_ai::TextContent {
                text: text.to_string(),
                text_signature: None,
            }),
        );
    }
    let (user_msg, history, config) =
        session.prepare_prompt_content(UserContent::Blocks(content_blocks));

    let tx_events = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
        let _ = tx_events.send(AgentSignal::Event(Box::new(ev)));
    });

    tokio::spawn(async move {
        // A panic inside the agent loop must not wedge the UI: it is reported as a
        // failed turn so the loop can return to the prompt.
        let outcome = AssertUnwindSafe(run_agent_loop(
            vec![user_msg],
            history,
            config,
            Some(on_event),
        ))
        .catch_unwind()
        .await;

        match outcome {
            Ok(messages) => {
                let _ = tx_done.send(AgentSignal::Finished(messages));
            }
            Err(_) => {
                let _ = tx_done.send(AgentSignal::Failed("the agent loop panicked".to_string()));
            }
        }
    });

    state.is_busy = true;
    state.status = Some("Working...".to_string());
}

/// Resumes an agent loop after mid-turn overflow compaction without inserting a new UserMessage.
pub(super) fn start_continuation_turn(
    session: &AgentSession,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.last_turn_error = None;
    let (_unused, history, config) =
        session.prepare_prompt_content(UserContent::Text(String::new()));

    let tx_events = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |ev| {
        let _ = tx_events.send(AgentSignal::Event(Box::new(ev)));
    });

    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(run_agent_loop(
            Vec::new(),
            history,
            config,
            Some(on_event),
        ))
        .catch_unwind()
        .await;

        match outcome {
            Ok(messages) => {
                let _ = tx_done.send(AgentSignal::Finished(messages));
            }
            Err(_) => {
                let _ = tx_done.send(AgentSignal::Failed("the agent loop panicked".to_string()));
            }
        }
    });

    state.is_busy = true;
    state.status = Some("Continuing after compaction...".to_string());
}

/// Spawns an interactive OAuth login flow off the TUI event loop so Ctrl+C and rendering stay live.
pub(super) fn spawn_oauth_login(
    provider: &str,
    auth_file: Option<PathBuf>,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> tokio::task::AbortHandle {
    let provider_owned = provider.to_string();
    state.items.push(TranscriptItem::System(format!(
        "Starting OAuth login for {provider_owned}..."
    )));
    let tx_prompt = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let handle = tokio::spawn(async move {
        let res = perform_login_with_prompt(
            &provider_owned,
            None,
            auth_file.as_deref(),
            move |msg| {
                let _ = tx_prompt.send(AgentSignal::SystemMessage(msg));
            },
        )
        .await
        .map_err(|e| e.to_string());
        let _ = tx_done.send(AgentSignal::LoginCompleted {
            provider: provider_owned,
            result: res,
        });
    });
    handle.abort_handle()
}

/// Spawns an MCP OAuth PKCE flow off the TUI event loop.
pub(super) fn spawn_mcp_auth(
    name: &str,
    mcp_manager: Option<&McpManager>,
    state: &mut InteractiveState,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> Option<tokio::task::AbortHandle> {
    let Some(mgr) = mcp_manager else {
        state.items.push(TranscriptItem::System(
            "MCP is not enabled for this session.".to_string(),
        ));
        return None;
    };
    let (url, auth_store) = match mgr.prepare_server_oauth(name) {
        Ok(v) => v,
        Err(e) => {
            state.items.push(TranscriptItem::System(format!(
                "MCP authentication failed for '{name}': {e}"
            )));
            return None;
        }
    };
    let server = name.to_string();
    state.items.push(TranscriptItem::System(format!(
        "Starting MCP OAuth authentication for '{server}'..."
    )));
    let tx_prompt = agent_tx.clone();
    let tx_done = agent_tx.clone();
    let handle = tokio::spawn(async move {
        let res = McpManager::complete_server_oauth_flow(&server, &url, &auth_store, |auth_url| {
            let _ = tx_prompt.send(AgentSignal::SystemMessage(format!(
                "Open the following URL to authenticate:\n{auth_url}"
            )));
        })
        .await
        .map_err(|e| e.to_string());
        let _ = tx_done.send(AgentSignal::McpAuthCompleted { server, result: res });
    });
    Some(handle.abort_handle())
}

pub(super) async fn finish_mcp_auth_signal(
    server: String,
    result: Result<(), String>,
    mcp_manager: Option<&mut McpManager>,
    session: &mut AgentSession,
    state: &mut InteractiveState,
    codemode_enabled: bool,
) {
    match result {
        Ok(()) => {
            if let Some(mgr) = mcp_manager {
                match mgr.reconnect_server(&server).await {
                    Ok(summary) => {
                        state.mcp_summaries = mgr.summaries().to_vec();
                        let tools = mgr.tools().to_vec();
                        sync_mcp_tools(session, tools, codemode_enabled);
                        if let Some(err) = summary.error {
                            state.items.push(TranscriptItem::System(format!(
                                "Authenticated MCP server '{server}', but reconnect reported error: {err}"
                            )));
                            return;
                        }
                    }
                    Err(e) => {
                        state.mcp_summaries = mgr.summaries().to_vec();
                        state.items.push(TranscriptItem::System(format!(
                            "Authenticated MCP server '{server}', but reconnect failed: {e}"
                        )));
                        return;
                    }
                }
            }
            state.items.push(TranscriptItem::System(format!(
                "Authenticated MCP server '{server}'"
            )));
        }
        Err(e) => state.items.push(TranscriptItem::System(format!(
            "MCP authentication failed for '{server}': {e}"
        ))),
    }
}


/// Applies one cycle step on a setting row, syncs runtime state, persists it,
/// and updates the inline status banner inside `SettingsList`.
pub(super) async fn cycle_active_setting_and_refresh(
    id: &str,
    forward: bool,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    if id == "permission_mode" {
        session.settings.permission_mode = Some(session.inspection_manager.mode().await);
    } else if id == "default_thinking_level" && session.settings.default_thinking_level.is_none() {
        session.settings.default_thinking_level = Some(
            session
                .effective_thinking_level()
                .unwrap_or(ThinkingLevel::Off),
        );
    }
    let status = match apply_setting_cycle_dir(id, forward, session, state) {
        Ok(msg) => {
            if id == "permission_mode" {
                if let Some(mode) = session.settings.permission_mode {
                    session.inspection_manager.set_mode(mode).await;
                }
            }
            match persist_settings(&session.settings, id, state.settings_file.as_deref()) {
                Ok(()) => (format!("Saved: {msg}"), false),
                Err(e) => (format!("{msg} (save failed: {e})"), true),
            }
        }
        Err(e) => (format!("Failed: {e}"), true),
    };
    let snapshot = effective_settings_snapshot(session).await;
    refresh_settings_selector_with_status(state, &snapshot, Some(id), Some(status));
}

/// Spawns background live model discovery for one provider and delivers [`AgentSignal::ModelsDiscovered`].
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_model_discovery(
    batch_id: u64,
    provider: String,
    base_url: String,
    api_override: Option<String>,
    auth_file: Option<PathBuf>,
    fallback_key: Option<String>,
    discovered_file: Option<PathBuf>,
    client: Result<reqwest::Client, String>,
    cancel: CancellationToken,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) -> tokio::task::AbortHandle {
    let tx = agent_tx.clone();
    let handle = tokio::spawn(async move {
        let resolved_key = tokio::select! {
            _ = cancel.cancelled() => return,
            res = crate::auth::resolve_api_key_async(&provider, None, auth_file.as_deref()) => {
                match res {
                    Ok(Some(k)) => Some(k),
                    Ok(None) => fallback_key,
                    Err(e) => {
                        if !cancel.is_cancelled() {
                            let _ = tx.send(AgentSignal::ModelsDiscovered(Box::new(
                                ModelsDiscoveredPayload {
                                    batch_id,
                                    provider,
                                    result: Err(format!("Failed to resolve credentials: {e}")),
                                },
                            )));
                        }
                        return;
                    }
                }
            }
        };

        let result = match client {
            Ok(client) => {
                let fetch_res = tokio::select! {
                    _ = cancel.cancelled() => return,
                    res = mehh_ai::fetch_provider_models(
                        &client,
                        &provider,
                        &base_url,
                        resolved_key.as_deref(),
                        api_override.as_deref(),
                    ) => res,
                };
                match fetch_res {
                    Ok(models) => {
                        match crate::models_config::save_discovered_provider_models_cancellable_async(
                            &provider,
                            &models,
                            discovered_file.as_deref(),
                            cancel.clone(),
                        )
                        .await
                        {
                            Ok(true) => Ok((models, None)),
                            Ok(false) => return,
                            Err(e) => {
                                let warn = format!(
                                    "Fetched {} models for {}, failed to save cache: {e}",
                                    models.len(),
                                    provider
                                );
                                Ok((models, Some(warn)))
                            }
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        if !cancel.is_cancelled() {
            let _ = tx.send(AgentSignal::ModelsDiscovered(Box::new(
                ModelsDiscoveredPayload {
                    batch_id,
                    provider,
                    result,
                },
            )));
        }
    });
    handle.abort_handle()
}

/// Starts background model discovery for `"__all__"` connected providers or a specific provider.
pub(in crate::interactive) async fn trigger_model_refresh(
    target: &str,
    session: &AgentSession,
    state: &mut InteractiveState,
    open_picker: bool,
    agent_tx: &tokio::sync::mpsc::UnboundedSender<AgentSignal>,
) {
    state.cancel_active_discovery();
    let batch_id = state.discovery_batch_id;
    let batch_cancel = CancellationToken::new();

    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let providers: Vec<String> = if target == "__all__" {
        let (connected, err) = connected_providers_with_session(session, state, &reg);
        if let Some(e) = err {
            state.items.push(TranscriptItem::System(e));
        }
        connected
            .into_iter()
            .filter(|c| !c.provider_id.eq_ignore_ascii_case("faux"))
            .map(|c| c.provider_id)
            .collect()
    } else {
        vec![target.trim().to_ascii_lowercase()]
    };

    if providers.is_empty() {
        if open_picker && !state.has_tool_approval() {
            open_connect_provider_selector(session, state);
        }
        return;
    }

    let client_res: Result<reqwest::Client, String> = match state.discovery_client.as_ref() {
        Some(c) => Ok(c.clone()),
        None => match mehh_ai::build_safe_http_client(
            mehh_ai::DnsPolicy::AllowPrivate,
            Duration::from_secs(10),
            Some(Duration::from_secs(10)),
        ) {
            Ok(c) => {
                state.discovery_client = Some(c.clone());
                Ok(c)
            }
            Err(e) => Err(e.to_string()),
        },
    };

    let mut launched_providers = Vec::new();
    for prov in providers {
        let cfg = reg.provider_config(&prov);
        let base_url = cfg
            .and_then(|c| c.base_url.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| mehh_ai::find_provider_spec(&prov).map(|s| s.default_base_url.to_string()))
            .or_else(|| mehh_ai::default_base_url(&prov).map(str::to_string));

        let Some(base_url) = base_url else {
            state.items.push(TranscriptItem::System(format!(
                "No base URL known for provider '{prov}'."
            )));
            continue;
        };

        let api_override = cfg
            .and_then(|c| c.api.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let fallback_key = reg.provider_api_key(&prov).map(str::to_string).or_else(|| {
            session
                .model
                .provider
                .eq_ignore_ascii_case(&prov)
                .then(|| session.api_key.clone())
                .flatten()
        });

        let handle = spawn_model_discovery(
            batch_id,
            prov.clone(),
            base_url,
            api_override,
            state.auth_file.clone(),
            fallback_key,
            state.discovered_models_file.clone(),
            client_res.clone(),
            batch_cancel.clone(),
            agent_tx,
        );
        state.discovery_handles.push(handle);
        launched_providers.push(prov);
    }

    let launched = launched_providers.len();
    if launched > 0 {
        state.discovery_cancel = Some(batch_cancel);
        state.discovery_pending = launched;
        state.discovery_open_picker = open_picker;
        state.discovery_status = Some(if launched == 1 {
            format!("Fetching models for {}...", launched_providers[0])
        } else {
            format!("Fetching models for {launched} providers...")
        });
    }
}
