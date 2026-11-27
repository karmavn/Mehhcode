use std::path::{Path, PathBuf};
use std::sync::Arc;

use mehh_ai::{faux, LlmProvider, Model, ThinkingLevel};
use mehh_cli::{CliArgs, CliMode};
use mehh_coding_agent::auth::resolve_api_key_async;
use mehh_coding_agent::models_config::ModelRegistry;
use mehh_coding_agent::trust::resolve_project_trust_with_override;
use mehh_coding_agent::{AgentSession, McpManager, PermissionMode, Settings, SettingsManager};

use super::{
    apply_cli_thinking_override, configure_cli_subagent, resolve_cli_tools_with_web,
    resolve_session_manager, select_startup_model,
};

pub(crate) struct BootstrappedSession {
    pub(crate) session: AgentSession,
    pub(crate) mcp_manager: Option<McpManager>,
    pub(crate) is_interactive: bool,
    pub(crate) cli_mode: CliMode,
}

pub(crate) fn resolve_trust_and_settings(
    args: &CliArgs,
    cli_mode: CliMode,
    cwd: &Path,
    is_interactive: bool,
) -> anyhow::Result<(bool, Settings)> {
    let trusted = resolve_project_trust_with_override(cwd, args.trust_override(), is_interactive)?;
    let mut settings = SettingsManager::try_load_with_trust(cwd, trusted)?;
    if let Some(ref m) = args.permission_mode {
        let mode = m
            .parse::<PermissionMode>()
            .map_err(|e| anyhow::anyhow!(e))?;
        settings.permission_mode = Some(mode);
    } else if args.approve && !args.no_approve {
        // Keep --approve useful: it auto-approves tools without granting trust to project-local config.
        settings.permission_mode = Some(PermissionMode::Auto);
    } else if cli_mode == CliMode::Rpc && settings.permission_mode.is_none() {
        // RPC has no approval channel; preserve explicit settings like Chat/Approve when present.
        settings.permission_mode = Some(PermissionMode::Auto);
    }
    if let Some(ref wm) = args.worker_model {
        settings.worker_model = Some(wm.clone());
    }
    if let Some(lt) = args.lead_turns {
        settings.lead_turns = Some(lt);
    }
    Ok((trusted, settings))
}

pub(crate) fn resolve_startup_model_and_provider(
    args: &CliArgs,
    settings: &Settings,
    model_registry: &ModelRegistry,
) -> anyhow::Result<(Model, Arc<dyn LlmProvider>)> {
    let is_faux = args
        .provider
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("faux"))
        || args
            .model
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("faux") || m.eq_ignore_ascii_case("faux-1"));

    if is_faux {
        let model = faux::faux_model();
        let faux_prov = faux::FauxProvider::new();
        match std::env::var("MEHH_FAUX_SCRIPT") {
            Ok(raw) if !raw.trim().is_empty() => {
                for msg in mehh_cli::parse_faux_script_json(&raw)? {
                    faux_prov.add_response(msg);
                }
            }
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                faux_prov.add_response(faux::faux_assistant_message(
                    "Hello from mehh Rust faux provider!",
                ));
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("MEHH_FAUX_SCRIPT must be valid UTF-8");
            }
        }
        return Ok((model, Arc::new(faux_prov)));
    }

    let needs_connected_scan = args.model.is_none()
        && args.provider.is_none()
        && settings.default_model.is_none()
        && settings.default_provider.is_none();
    let connected = if needs_connected_scan {
        match mehh_coding_agent::list_connected_providers(None, model_registry) {
            Ok(list) => list,
            Err(e) => {
                eprintln!("Warning: could not inspect auth credentials: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let model = select_startup_model(model_registry, args, settings, &connected).ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to resolve model (model: {:?}, provider: {:?})",
            args.model,
            args.provider
        )
    })?;
    let provider = AgentSession::default_provider_for_model(&model);
    Ok((model, provider))
}

fn configure_cli_tools_and_flags(
    args: &CliArgs,
    cwd: &Path,
    agent_session: &mut AgentSession,
    model: &Model,
    model_registry: &ModelRegistry,
) -> anyhow::Result<bool> {
    if args.no_context_files {
        agent_session.set_no_context_files(true);
    }
    if args.verbose {
        eprintln!(
            "model={} provider={} api={} base_url={}",
            model.id, model.provider, model.api, model.base_url
        );
    }
    if args.skills && agent_session.skills.is_empty() {
        eprintln!("note: --skills is enabled by default; no project skills were found");
    }
    if args.prompt_templates && agent_session.prompt_templates.is_empty() {
        eprintln!("note: --prompt-templates is enabled by default; no templates were found");
    }

    let exclude_contains = |name: &str| {
        args.exclude_tools
            .as_deref()
            .unwrap_or("")
            .split(',')
            .any(|s| s.trim() == name)
    };
    let include_subagent = if args.no_tools || args.no_builtin_tools || exclude_contains("subagent") {
        false
    } else {
        match args.tools.as_deref() {
            Some(inc) => inc.split(',').any(|s| s.trim() == "subagent"),
            None => true,
        }
    };
    if args.no_tools
        || args.no_builtin_tools
        || args.tools.is_some()
        || args.exclude_tools.is_some()
    {
        let web_cfg = agent_session
            .settings
            .to_web_config()
            .with_url_policy_checker(agent_session.inspection_manager.clone());
        let mut cli_tools = resolve_cli_tools_with_web(args, cwd.to_path_buf(), web_cfg)?;
        let should_include_todo = if args.no_tools || args.no_builtin_tools || exclude_contains("todo") {
            false
        } else if let Some(ref inc) = args.tools {
            inc.split(',').any(|s| s.trim() == "todo")
        } else {
            true
        };
        if should_include_todo {
            cli_tools.push(Arc::new(mehh_coding_agent::todo::TodoTool::new(
                agent_session.todo_state.clone(),
            )));
        }
        agent_session.set_tools(cli_tools);
    }

    if let Some(ref models_str) = args.models {
        let mut resolved = Vec::new();
        for token in models_str
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
        {
            let model_opt = if let Some((provider, id)) = token.split_once('/') {
                model_registry
                    .resolve_exact(provider, id)
                    .or_else(|| model_registry.resolve(token, None))
            } else {
                model_registry.resolve(token, None)
            };
            match model_opt {
                Some(m) => resolved.push(m),
                None => eprintln!("warning: unknown model '{token}' in --models; skipped"),
            }
        }
        agent_session.set_scoped_models(resolved);
    }

    if args.no_skills && !agent_session.skills.is_empty() {
        agent_session.skills.clear();
        agent_session.rebuild_system_prompt();
    }
    if args.no_prompt_templates {
        agent_session.prompt_templates.clear();
    }

    if args.no_themes {
        agent_session.settings.theme = Some("dark".to_string());
    } else if let Some(ref th) = args.use_theme {
        agent_session.settings.theme = Some(th.clone());
    }

    Ok(include_subagent)
}

/// Assembles a ready-to-run `AgentSession` and optional `McpManager` uniformly across all `CliMode`s.
pub(crate) async fn bootstrap_session(
    args: &CliArgs,
    cli_mode: CliMode,
    cli_thinking: Option<ThinkingLevel>,
    cwd: PathBuf,
    model_registry: &ModelRegistry,
    is_interactive: bool,
) -> anyhow::Result<BootstrappedSession> {
    let (trusted, settings) = resolve_trust_and_settings(args, cli_mode, &cwd, is_interactive)?;
    let (model, provider) = resolve_startup_model_and_provider(args, &settings, model_registry)?;
    let session_mgr = resolve_session_manager(args, &cwd)?;

    let skip_default_tools = args.no_tools
        || args.no_builtin_tools
        || args.tools.is_some()
        || args.exclude_tools.is_some();
    let init_opts = mehh_coding_agent::SessionInitOptions {
        skip_default_tools,
        skip_skills: args.no_skills,
        skip_prompt_templates: args.no_prompt_templates,
        no_context_files: args.no_context_files,
    };
    let mut agent_session = AgentSession::new_with_options(
        cwd.clone(),
        model.clone(),
        provider,
        session_mgr,
        settings,
        trusted,
        init_opts,
    );

    let include_subagent =
        configure_cli_tools_and_flags(args, &cwd, &mut agent_session, &model, model_registry)?;

    let has_explicit_api_key = args
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    let is_faux = model.provider == "faux" || model.api == "faux";
    // join! (not try_join!) so a failing key refresh cannot drop an MCP
    // connect future that may already have spawned child processes.
    let (api_key_resolved, mcp_resolved) = tokio::join!(
        async {
            if is_faux && !has_explicit_api_key {
                Ok(None)
            } else {
                resolve_api_key_async(&model.provider, args.api_key.as_deref(), None::<&Path>).await
            }
        },
        async {
            if !args.no_tools && !args.no_mcp {
                McpManager::connect_from_configs_with_trust(&cwd, trusted)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            }
        }
    );
    let mcp_manager = mcp_resolved?;
    let api_key_base = match api_key_resolved {
        Ok(key) => key,
        Err(err) => {
            close_mcp(mcp_manager.as_ref()).await;
            return Err(err);
        }
    };
    let api_key = api_key_base.or_else(|| {
        model_registry
            .provider_api_key(&model.provider)
            .map(str::to_string)
    });
    if has_explicit_api_key {
        agent_session.set_explicit_api_key(api_key);
    } else {
        agent_session.set_api_key(api_key);
    }
    apply_cli_thinking_override(&mut agent_session, cli_thinking);
    agent_session.sync_thinking_for_model(true);

    let mcp_tools = if let Some(ref mgr) = mcp_manager {
        if !is_interactive {
            for summary in mgr.summaries() {
                if let Some(ref err) = summary.error {
                    eprintln!(
                        "Warning: MCP server '{}' failed to connect: {}",
                        summary.server, err
                    );
                }
            }
        }
        mgr.tools().to_vec()
    } else {
        Vec::new()
    };

    if !args.no_tools && (!mcp_tools.is_empty() || args.codemode) {
        agent_session.enable_mcp_and_codemode(mcp_tools, args.codemode);
    }

    if include_subagent {
        if let Err(err) = configure_cli_subagent(args, &cwd, &mut agent_session) {
            close_mcp(mcp_manager.as_ref()).await;
            return Err(err);
        }
    }

    if args.system_prompt.is_some() || args.append_system_prompt.is_some() {
        agent_session.set_custom_system_prompt(
            args.system_prompt.clone(),
            args.append_system_prompt.clone(),
        );
    }

    Ok(BootstrappedSession {
        session: agent_session,
        mcp_manager,
        is_interactive,
        cli_mode,
    })
}

pub(crate) async fn close_mcp(mcp_manager: Option<&McpManager>) {
    if let Some(mgr) = mcp_manager {
        mgr.close_all().await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct TempEnv {
        root: PathBuf,
        pub(crate) cwd: PathBuf,
        saved: Vec<(String, Option<OsString>)>,
    }

    impl TempEnv {
        pub(crate) fn new(prefix: &str, faux_script: Option<&str>) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "mehh_boot_{prefix}_{}_{}",
                std::process::id(),
                seq
            ));
            let cwd = root.join("ws");
            let home = root.join("home");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&home).unwrap();

            let mut keys = vec![
                "HOME".to_string(),
                "USERPROFILE".to_string(),
                "MEHH_FAUX_SCRIPT".to_string(),
                "GOOGLE_API_KEY".to_string(),
                "AWS_BEARER_TOKEN_BEDROCK".to_string(),
                "API_KEY".to_string(),
            ];
            for spec in mehh_ai::KNOWN_PROVIDERS {
                keys.push(spec.env_var.to_string());
            }

            let mut saved = Vec::with_capacity(keys.len());
            for key in keys {
                saved.push((key.clone(), std::env::var_os(&key)));
                unsafe {
                    if key == "HOME" || key == "USERPROFILE" {
                        std::env::set_var(&key, &home);
                    } else if key == "MEHH_FAUX_SCRIPT" {
                        match faux_script {
                            Some(s) => std::env::set_var(&key, s),
                            None => std::env::remove_var(&key),
                        }
                    } else {
                        std::env::remove_var(&key);
                    }
                }
            }
            Self { root, cwd, saved }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(&k, val),
                        None => std::env::remove_var(&k),
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn resolve_trust_and_settings_respects_explicit_permission_mode_in_rpc() {
        let _lock = ENV_LOCK.lock().await;
        let env = TempEnv::new("rpc_perm", None);

        let args_default_rpc = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        let (_, settings_default) =
            resolve_trust_and_settings(&args_default_rpc, CliMode::Rpc, &env.cwd, false).unwrap();
        assert_eq!(settings_default.permission_mode, Some(PermissionMode::Auto));

        let args_chat_rpc = CliArgs::try_parse_from([
            "mehh",
            "--mode",
            "rpc",
            "--permission-mode",
            "chat",
        ])
        .unwrap();
        let (_, settings_chat) =
            resolve_trust_and_settings(&args_chat_rpc, CliMode::Rpc, &env.cwd, false).unwrap();
        assert_eq!(settings_chat.permission_mode, Some(PermissionMode::Chat));
    }

    #[tokio::test]
    async fn bootstrap_session_uniformly_configures_all_four_cli_modes() {
        let _lock = ENV_LOCK.lock().await;
        let env = TempEnv::new("all_modes", Some(r#"[{"text":"BOOTSTRAP_SCRIPT_OK"}]"#));
        let registry = ModelRegistry::builtin_only();

        let cases = [
            (CliMode::Interactive, true, "interactive"),
            (CliMode::Rpc, false, "rpc"),
            (CliMode::Text, false, "text"),
            (CliMode::Json, false, "json"),
        ];

        for (mode, is_interactive, mode_flag) in cases {
            let args = CliArgs::try_parse_from([
                "mehh",
                "--provider",
                "faux",
                "--no-mcp",
                "--no-session",
                "--mode",
                mode_flag,
                "--permission-mode",
                "chat",
                "--thinking",
                "high",
            ])
            .unwrap();

            let mut boot = bootstrap_session(
                &args,
                mode,
                Some(ThinkingLevel::High),
                env.cwd.clone(),
                &registry,
                is_interactive,
            )
            .await
            .unwrap();

            assert_eq!(boot.is_interactive, is_interactive);
            assert_eq!(boot.cli_mode, mode);
            assert!(boot.mcp_manager.is_none());
            assert_eq!(
                boot.session.inspection_manager.mode().await,
                mehh_coding_agent::PermissionMode::Chat
            );
            assert_eq!(
                boot.session.effective_thinking_level(),
                Some(ThinkingLevel::High)
            );
            let tool_names: Vec<&str> = boot.session.tools().iter().map(|t| t.name()).collect();
            assert!(tool_names.contains(&"read"));
            assert!(tool_names.contains(&"subagent"));
            assert!(tool_names.contains(&"todo"));

            let reply = boot.session.prompt("ping", None).await.unwrap();
            let assistant_text: String = reply
                .iter()
                .filter_map(|m| match m {
                    mehh_ai::Message::Assistant(a) => Some(a.text_content()),
                    _ => None,
                })
                .collect();
            assert!(
                assistant_text.contains("BOOTSTRAP_SCRIPT_OK"),
                "mode {mode_flag} must use MEHH_FAUX_SCRIPT response, got: {assistant_text:?}"
            );
        }
    }
}
