use super::*;

/// Logs into a provider and stores the resulting credential, delivering any interactive
/// OAuth URL or device code through `on_prompt` instead of writing directly to stdout.
pub async fn perform_login_with_prompt<F>(
    provider: &str,
    api_key: Option<&str>,
    auth_file: Option<&Path>,
    on_prompt: F,
) -> anyhow::Result<String>
where
    F: FnOnce(String) + Send,
{
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        let cred = StoredCredential::ApiKey {
            key: Some(key.to_string()),
            env: None,
        };
        crate::auth::save_credential_async(provider, &cred, auth_file).await?;
        return Ok(format!("Saved API key for {provider}"));
    }

    let kind = crate::auth::oauth_kind_for_provider(provider).ok_or_else(|| {
        anyhow::anyhow!("Provider '{provider}' does not support OAuth. Provide an API key instead.")
    })?;

    let client = mehh_ai::default_http_client();
    let cred = match kind {
        OAuthProviderKind::Anthropic | OAuthProviderKind::OpenAiChatGpt => {
            login_via_pkce_with_prompt(&client, kind, on_prompt).await?
        }
        OAuthProviderKind::GitHubCopilot => {
            login_via_device_flow_with_prompt(&client, on_prompt).await?
        }
    };

    crate::auth::save_credential_async(provider, &cred, auth_file).await?;
    Ok(format!("Logged in to {provider}"))
}

/// Logs into a provider and stores the resulting credential.
///
/// An explicit API key is stored verbatim; otherwise the provider's OAuth flow is
/// used (PKCE for Anthropic and ChatGPT, device code for GitHub Copilot).
pub async fn perform_login(
    provider: &str,
    api_key: Option<&str>,
    auth_file: Option<&Path>,
) -> anyhow::Result<String> {
    perform_login_with_prompt(provider, api_key, auth_file, |msg| println!("{msg}")).await
}

/// Removes the stored credential for a provider. Returns true when one was removed.
pub fn perform_logout(provider: &str, auth_file: Option<&Path>) -> anyhow::Result<bool> {
    crate::auth::remove_credential(provider, auth_file)
}

/// Refreshes the session API key after a login so the running agent uses the new
/// credential without a restart.
///
/// Only the credential of the session's own provider can replace the session key:
/// a key stored for another provider would be sent to the wrong endpoint.
pub async fn refresh_session_api_key(
    session: &mut AgentSession,
    provider: &str,
    auth_file: Option<&Path>,
) -> anyhow::Result<()> {
    if !provider.eq_ignore_ascii_case(&session.model.provider) {
        return Ok(());
    }

    // The credential that was just stored wins over the environment variable.
    let entries = crate::auth::load_auth_entries(auth_file)?;
    let key = entries
        .get(provider)
        .or_else(|| {
            entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(provider))
                .map(|(_, v)| v)
        })
        .and_then(|cred| match cred {
            StoredCredential::ApiKey { key, .. } => key.clone(),
            StoredCredential::OAuth { access, .. } => Some(access.clone()),
        });

    if let Some(key) = key {
        session.set_api_key(Some(key));
    }
    Ok(())
}

/// Runs the OAuth PKCE flow against a loopback callback listener.
pub(super) async fn login_via_pkce_with_prompt<F>(
    client: &reqwest::Client,
    kind: OAuthProviderKind,
    on_prompt: F,
) -> anyhow::Result<StoredCredential>
where
    F: FnOnce(String),
{
    // Keep the listener bound until the callback helper takes ownership;
    // dropping it here would let another process steal the port (TOCTOU).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind loopback listener for OAuth callback")?;
    let port = listener
        .local_addr()
        .context("Failed to read loopback listener address")?
        .port();

    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let pkce = oauth::generate_pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::build_authorize_url(kind, &pkce.challenge, &state, &redirect_uri)?;

    on_prompt(format!("Open the following URL to authenticate:\n{url}"));

    let code =
        oauth::wait_for_loopback_callback(listener, &state, Duration::from_secs(300)).await?;
    oauth::exchange_code_for_tokens(client, kind, &code, &pkce.verifier, &redirect_uri).await
}

/// Runs the GitHub Copilot device code flow until the user approves or the code expires.
pub(super) async fn login_via_device_flow_with_prompt<F>(
    client: &reqwest::Client,
    on_prompt: F,
) -> anyhow::Result<StoredCredential>
where
    F: FnOnce(String),
{
    let challenge = oauth::start_copilot_device_flow(client).await?;
    on_prompt(format!(
        "Enter code {} at {}",
        challenge.user_code, challenge.verification_uri
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(challenge.expires_in_secs);
    let interval = Duration::from_secs(challenge.interval_secs.max(1));

    let mut poll_interval = interval;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        match oauth::poll_copilot_device_token(client, &challenge.device_code).await? {
            oauth::CopilotDevicePoll::Complete(cred) => return Ok(cred),
            oauth::CopilotDevicePoll::SlowDown => {
                poll_interval += Duration::from_secs(5);
            }
            oauth::CopilotDevicePoll::Pending => {}
        }
    }

    anyhow::bail!("Device code login timed out before it was approved")
}

/// Loads `ModelRegistry` with optional discovered-models cache, surfacing non-fatal warnings.
pub(in crate::interactive) fn load_interactive_registry(
    state: &InteractiveState,
) -> (crate::models_config::ModelRegistry, Option<String>) {
    match crate::models_config::ModelRegistry::load_with_discovered(
        state.models_file.as_deref(),
        state.discovered_models_file.as_deref(),
    ) {
        Ok(reg) => (reg, None),
        Err(disc_err) => {
            let base_res = match state.models_file.as_deref() {
                Some(p) => crate::models_config::ModelRegistry::load_with_path(p),
                None => crate::models_config::ModelRegistry::load_default(),
            };
            match base_res {
                Ok(reg) => (
                    reg,
                    Some(format!(
                        "Discovered models cache could not be read: {disc_err}"
                    )),
                ),
                Err(base_err) => (
                    crate::models_config::ModelRegistry::builtin_only(),
                    Some(format!("Failed to load models registry: {base_err}")),
                ),
            }
        }
    }
}

/// Computes connected providers from disk/env/config plus any active session credential.
pub(in crate::interactive) fn connected_providers_with_session(
    session: &AgentSession,
    state: &InteractiveState,
    reg: &crate::models_config::ModelRegistry,
) -> (Vec<crate::auth::ConnectedProvider>, Option<String>) {
    let mut warn = None;
    let mut connected = match crate::auth::list_connected_providers(state.auth_file.as_deref(), reg) {
        Ok(list) => list,
        Err(e) => {
            warn = Some(format!("Failed to read stored credentials: {e}"));
            Vec::new()
        }
    };

    let active_provider = session.model.provider.trim().to_ascii_lowercase();
    let has_active_key = session
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    if (active_provider == "faux" || has_active_key)
        && !active_provider.is_empty()
        && !connected
            .iter()
            .any(|p| p.provider_id.eq_ignore_ascii_case(&active_provider))
    {
        connected.push(crate::auth::ConnectedProvider {
            provider_id: active_provider,
            source: crate::auth::CredentialSource::AuthFile,
            has_oauth: false,
        });
    }

    (connected, warn)
}

/// Refreshes `state.connected_provider_count` from auth/env/models configuration.
pub(in crate::interactive) fn refresh_connected_state(session: &AgentSession, state: &mut InteractiveState) {
    let (reg, _warn) = load_interactive_registry(state);
    let (connected, _auth_warn) = connected_providers_with_session(session, state, &reg);
    state.connected_provider_count = connected
        .iter()
        .filter(|p| !p.provider_id.eq_ignore_ascii_case("faux"))
        .count();
}

pub(in crate::interactive) fn open_connect_provider_selector_with(
    session: &AgentSession,
    state: &mut InteractiveState,
    reg: &crate::models_config::ModelRegistry,
    connected: &[crate::auth::ConnectedProvider],
    auth_err: Option<String>,
) {
    if let Some(err) = auth_err {
        state.items.push(TranscriptItem::System(err));
    }

    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut selected_idx = 0usize;

    for spec in mehh_ai::KNOWN_PROVIDERS {
        let lower_id = spec.id.to_ascii_lowercase();
        seen.insert(lower_id.clone());
        let conn = connected
            .iter()
            .find(|c| c.provider_id.eq_ignore_ascii_case(spec.id));
        let auth_methods = if spec
            .auth_methods
            .contains(&mehh_ai::ProviderAuthMethod::OAuth)
            && spec
                .auth_methods
                .contains(&mehh_ai::ProviderAuthMethod::ApiKey)
        {
            "OAuth · API key"
        } else if spec
            .auth_methods
            .contains(&mehh_ai::ProviderAuthMethod::OAuth)
        {
            "OAuth"
        } else {
            "API key"
        };

        let is_active_provider = session.model.provider.eq_ignore_ascii_case(spec.id);
        let (label, desc) = if let Some(c) = conn {
            let src = if c.has_oauth {
                "OAuth"
            } else {
                auth_methods
            };
            (
                format!("{} ✓ connected", spec.display_name),
                format!("{} · {} · connected", spec.id, src),
            )
        } else {
            (
                spec.display_name.to_string(),
                format!("{} · {}", spec.id, auth_methods),
            )
        };
        let mut item = SelectItem::new(spec.id, label).with_description(desc);
        if conn.is_some() {
            item = item.with_label_color(state.theme.success.clone());
        }
        if is_active_provider && conn.is_some() {
            selected_idx = items.len();
        }
        items.push(item);
    }

    let mut custom_ids: Vec<String> = reg
        .provider_configs()
        .keys()
        .filter(|k| !seen.contains(&k.trim().to_ascii_lowercase()))
        .cloned()
        .collect();
    custom_ids.sort();
    for id in custom_ids {
        let base_desc = reg
            .provider_config(&id)
            .and_then(|c| c.base_url.as_deref())
            .unwrap_or("custom endpoint");
        let is_conn = connected
            .iter()
            .any(|c| c.provider_id.eq_ignore_ascii_case(&id));
        let label = if is_conn {
            format!("{id} ✓ connected")
        } else {
            id.clone()
        };
        let mut item =
            SelectItem::new(id.clone(), label).with_description(format!("Custom · {base_desc}"));
        if is_conn {
            item = item.with_label_color(state.theme.success.clone());
        }
        items.push(item);
    }

    items.push(
        SelectItem::new(
            "__custom__",
            "+ Custom OpenAI-compatible (Ollama / vLLM / Proxy)",
        )
        .with_description("Configure custom base URL and optional API key")
        .with_label_color(state.theme.accent.clone()),
    );

    state.open_modal(Selector {
        kind: SelectorKind::Login,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
}

/// Opens the unified `/connect` provider selector modal.
pub(in crate::interactive) fn open_connect_provider_selector(
    session: &AgentSession,
    state: &mut InteractiveState,
) {
    let (reg, _warn) = load_interactive_registry(state);
    let (connected, auth_err) = connected_providers_with_session(session, state, &reg);
    open_connect_provider_selector_with(session, state, &reg, &connected, auth_err);
}

/// Opens the appropriate `/connect` flow directly for `provider` (`__custom__`, OAuth/connected methods, or API key input).
pub(in crate::interactive) fn open_connect_for_provider(
    provider: &str,
    session: &AgentSession,
    state: &mut InteractiveState,
) {
    if state.has_tool_approval() {
        return;
    }
    let trimmed = provider.trim();
    if trimmed.is_empty() {
        open_connect_provider_selector(session, state);
        return;
    }
    if trimmed.eq_ignore_ascii_case("__custom__") {
        state.connect_input = Some(crate::interactive::ConnectInputState {
            kind: crate::interactive::ConnectInputKind::CustomProviderId,
            value: String::new(),
            masked: false,
            error: None,
        });
        state.open_modal(Selector {
            kind: SelectorKind::ConnectInput,
            list: SelectList::new(Vec::new(), 1),
            settings_list: None,
        });
        return;
    }

    let prov_id = trimmed.to_ascii_lowercase();
    let supports_oauth = crate::auth::oauth_kind_for_provider(&prov_id).is_some();
    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let (connected, auth_err) = connected_providers_with_session(session, state, &reg);
    if let Some(err) = auth_err {
        state.items.push(TranscriptItem::System(err));
    }
    let is_connected = connected
        .iter()
        .any(|c| c.provider_id.eq_ignore_ascii_case(&prov_id));

    if supports_oauth || is_connected {
        let mut methods = Vec::new();
        if supports_oauth {
            methods.push(
                SelectItem::new("oauth", "OAuth Login")
                    .with_description("Sign in via browser or device code"),
            );
        }
        methods.push(
            SelectItem::new("api_key", "Paste API Key")
                .with_description("Store an API key in ~/.mehh/agent/auth.json"),
        );
        if is_connected {
            methods.push(
                SelectItem::new("refresh_models", "↻ Refresh Models")
                    .with_description("Fetch live models for this provider"),
            );
            methods.push(
                SelectItem::new("disconnect", "Disconnect Provider")
                    .with_description("Remove stored credential and cached models")
                    .with_label_color(state.theme.error.clone()),
            );
        }
        state.connect_provider_target = Some(prov_id);
        state.open_modal(Selector {
            kind: SelectorKind::ConnectMethod,
            list: SelectList::new(methods, 6).with_theme(state.theme.clone()),
            settings_list: None,
        });
    } else {
        let display = mehh_ai::find_provider_spec(&prov_id)
            .map(|s| s.display_name.to_string())
            .unwrap_or_else(|| prov_id.clone());
        state.connect_input = Some(crate::interactive::ConnectInputState {
            kind: crate::interactive::ConnectInputKind::ApiKey {
                provider: prov_id,
                display_name: display,
            },
            value: String::new(),
            masked: true,
            error: None,
        });
        state.open_modal(Selector {
            kind: SelectorKind::ConnectInput,
            list: SelectList::new(Vec::new(), 1),
            settings_list: None,
        });
    }
}

/// Disconnects a provider uniformly across `auth.json`, custom `models.json`, `discovered-models.json`, and active session key.
pub(in crate::interactive) async fn disconnect_provider(
    provider: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    let target = provider.trim().to_ascii_lowercase();
    if target.is_empty() {
        return;
    }

    state.cancel_active_discovery();

    let cred_removed = match crate::auth::remove_credential_async(&target, state.auth_file.as_deref())
        .await
    {
        Ok(removed) => removed,
        Err(e) => {
            state.items.push(TranscriptItem::System(format!(
                "Failed to disconnect {target}: {e}"
            )));
            return;
        }
    };

    let custom_removed = match crate::models_config::disconnect_provider_in_models_file_async(
        &target,
        state.models_file.as_deref(),
    )
    .await
    {
        Ok(removed) => removed,
        Err(e) => {
            state.items.push(TranscriptItem::System(format!(
                "Warning: failed to update models config while disconnecting {target}: {e}"
            )));
            false
        }
    };

    if let Err(e) = crate::models_config::remove_discovered_provider_models_async(
        &target,
        state.discovered_models_file.as_deref(),
    )
    .await
    {
        state.items.push(TranscriptItem::System(format!(
            "Warning: could not clear cached models for {target}: {e}"
        )));
    }

    let cleared_session_key = session.clear_provider_api_keys(&target);

    refresh_connected_state(session, state);
    state.items.push(TranscriptItem::System(
        if cred_removed || custom_removed {
            format!("Disconnected {target}.")
        } else if cleared_session_key {
            format!("Cleared active session key for {target}.")
        } else {
            format!("No stored credential or custom endpoint for {target}.")
        },
    ));
}

/// Opens `/model` listing ONLY models from connected providers, or redirects to `/connect` when none are connected.
pub(in crate::interactive) fn open_model_selector(session: &AgentSession, state: &mut InteractiveState) {
    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let (connected, auth_err) = connected_providers_with_session(session, state, &reg);
    if let Some(err) = auth_err {
        state.items.push(TranscriptItem::System(err));
    }

    if connected.is_empty() {
        state.items.push(TranscriptItem::System(
            "No provider connected yet. Connect a provider to load its models.".to_string(),
        ));
        open_connect_provider_selector_with(session, state, &reg, &connected, None);
        return;
    }

    let models = reg.models_for_connected_providers(&connected);
    if models.is_empty() {
        state.items.push(TranscriptItem::System(
            "No models available yet. Connect a provider or refresh models.".to_string(),
        ));
        open_connect_provider_selector_with(session, state, &reg, &connected, None);
        return;
    }

    let mut items: Vec<SelectItem> = Vec::with_capacity(models.len() + 2);
    let mut selected_idx = 0usize;
    for m in &models {
        let is_current = m.provider.eq_ignore_ascii_case(&session.model.provider)
            && m.id.eq_ignore_ascii_case(&session.model.id);
        if is_current {
            selected_idx = items.len();
        }
        let ctx_k = m.context_window / 1000;
        let prof = mehh_ai::model_thinking_profile(m);
        let mut desc = if m.reasoning {
            if ctx_k > 0 {
                format!(
                    "{} · {}k ctx · thinking ({})",
                    m.id,
                    ctx_k,
                    thinking_level_label(prof.default_level)
                )
            } else {
                format!(
                    "{} · thinking ({})",
                    m.id,
                    thinking_level_label(prof.default_level)
                )
            }
        } else if ctx_k > 0 {
            format!("{} · {}k ctx", m.id, ctx_k)
        } else {
            m.id.clone()
        };
        if is_current {
            desc.push_str(" (current)");
        }
        let label = if is_current {
            format!("{} ({}) (current)", m.name, m.provider)
        } else {
            format!("{} ({})", m.name, m.provider)
        };
        let mut item =
            SelectItem::new(format!("{}/{}", m.provider, m.id), label).with_description(desc);
        if is_current {
            item = item.with_label_color(state.theme.accent.clone());
        }
        items.push(item);
    }

    items.push(
        SelectItem::new(
            "__action:refresh_models",
            "↻ Refresh models from connected providers",
        )
        .with_description("Fetch latest models from provider APIs")
        .with_label_color(state.theme.accent.clone()),
    );
    items.push(
        SelectItem::new("__action:connect_provider", "+ Connect another provider...")
            .with_description("Add OAuth, API key, or custom endpoint")
            .with_label_color(state.theme.accent.clone()),
    );

    state.open_modal(Selector {
        kind: SelectorKind::Model,
        list: select_list_preselected(items, 10, selected_idx),
        settings_list: None,
    });
}

/// Switches the session's model via [`ModelRegistry`] and asynchronously refreshes credentials.
pub(in crate::interactive) async fn apply_interactive_model_switch(
    target: &str,
    session: &mut AgentSession,
    state: &mut InteractiveState,
) {
    let (reg, warn) = load_interactive_registry(state);
    if let Some(w) = warn {
        state.items.push(TranscriptItem::System(w));
    }
    let resolved = reg.resolve(target, None).or_else(|| {
        reg.models()
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(target.trim()))
            .cloned()
    });
    let Some(m) = resolved else {
        state
            .items
            .push(TranscriptItem::System(format!("Unknown model: '{target}'")));
        return;
    };

    let resolved_key = if m.provider != "faux" && m.api != "faux" {
        match crate::auth::resolve_api_key_async(&m.provider, None, state.auth_file.as_deref())
            .await
        {
            Ok(key) => key.or_else(|| reg.provider_api_key(&m.provider).map(str::to_string)),
            Err(e) => {
                state.items.push(TranscriptItem::System(format!(
                    "Failed to resolve credentials for '{}': {e}",
                    m.provider
                )));
                return;
            }
        }
    } else {
        None
    };

    let provider_changed = !session.model.provider.eq_ignore_ascii_case(&m.provider);
    let prov = AgentSession::default_provider_for_model(&m);
    session.set_model(m.clone(), prov);
    if let Some(key) = resolved_key {
        if provider_changed || !session.has_explicit_api_key() {
            session.set_api_key(Some(key));
        }
    }
    session.sync_thinking_for_model(true);
    refresh_connected_state(session, state);

    if !m.provider.eq_ignore_ascii_case("faux") {
        session.settings.default_provider = Some(m.provider.clone());
        session.settings.default_model = Some(m.id.clone());
        if let Err(e) = persist_settings_keys(
            &session.settings,
            &["default_provider", "default_model"],
            state.settings_file.as_deref(),
        ) {
            state.items.push(TranscriptItem::System(format!(
                "Warning: could not persist default provider/model: {e}"
            )));
        }
    }

    let profile = mehh_ai::model_thinking_profile(&session.model);
    let thinking_note = if profile.supported {
        format!(
            "thinking: {}",
            thinking_level_label(session.thinking_level().unwrap_or(ThinkingLevel::Off))
        )
    } else {
        "thinking: n/a".to_string()
    };
    state.items.push(TranscriptItem::System(format!(
        "Switched model to {} ({}) · {thinking_note}",
        m.name, m.provider
    )));
}
