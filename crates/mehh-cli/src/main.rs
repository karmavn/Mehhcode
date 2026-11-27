use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use clap::Parser;
use mehh_ai::{
    catalog, Model, ModelCost, ThinkingLevel,
};
use mehh_coding_agent::models_config::ModelRegistry;
use mehh_coding_agent::{
    process_file_arguments, ProcessedPromptArgs, list_sessions, perform_login, perform_logout, rpc, run_interactive_mode, run_print_mode_content,
    AgentSession, OutputMode, SessionManager, SessionSummary, Settings,
};
pub use mehh_cli::{parse_cli_mode, should_run_interactive, CliArgs, CliMode};

mod bootstrap;

/// `todo` and `subagent` are added by main.rs after tool selection, so they
/// must never reach `select_coding_tools` (which only knows built-in tools).
fn parse_tool_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && t != "todo" && t != "subagent")
        .collect()
}

pub fn resolve_cli_tools(
    args: &CliArgs,
    cwd: PathBuf,
) -> anyhow::Result<Vec<Arc<dyn mehh_agent::AgentTool>>> {
    resolve_cli_tools_with_web(args, cwd, mehh_tools::WebConfig::default())
}

pub fn resolve_cli_tools_with_web(
    args: &CliArgs,
    cwd: PathBuf,
    web: mehh_tools::WebConfig,
) -> anyhow::Result<Vec<Arc<dyn mehh_agent::AgentTool>>> {
    if args.no_tools || args.no_builtin_tools {
        return Ok(Vec::new());
    }
    let include = args.tools.as_deref().map(parse_tool_list);
    let exclude = args
        .exclude_tools
        .as_deref()
        .map(parse_tool_list)
        .unwrap_or_default();
    mehh_tools::select_coding_tools_with_web(cwd, include.as_deref(), &exclude, web)
}

/// Attaches `subagent` without widening the parent session's `--tools` or `--exclude-tools` filter.
pub fn configure_cli_subagent(
    args: &CliArgs,
    cwd: &Path,
    agent_session: &mut AgentSession,
) -> anyhow::Result<()> {
    let exclude = args
        .exclude_tools
        .as_deref()
        .map(parse_tool_list)
        .unwrap_or_default();
    let parsed_include = args.tools.as_deref().map(parse_tool_list);
    // With `--tools subagent`, `parsed_include` is `Some([])`; fall back to
    // non-excluded built-in tools for child subagents while keeping parent empty.
    let child_include = parsed_include.as_deref().filter(|v| !v.is_empty());

    let mut child_tools: Vec<Arc<dyn mehh_agent::AgentTool>> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tool in agent_session
        .tools()
        .iter()
        .filter(|t| t.name() != "subagent" && t.name() != "codemode")
    {
        if seen.insert(tool.name().to_string()) {
            child_tools.push(tool.clone());
        }
    }
    if !args.no_tools
        && !args.no_builtin_tools
        && args.tools.is_some()
        && child_include.is_none()
    {
        let web_cfg = agent_session
            .settings
            .to_web_config()
            .with_url_policy_checker(agent_session.inspection_manager.clone());
        for tool in mehh_tools::select_coding_tools_with_web(
            cwd.to_path_buf(),
            None,
            &exclude,
            web_cfg,
        )? {
            if seen.insert(tool.name().to_string()) {
                child_tools.push(tool);
            }
        }
    }
    agent_session.set_subagent_with_child_tools(child_tools);
    Ok(())
}


pub fn handle_export(
    input_file: &Path,
    output_file: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<()> {
    mehh_coding_agent::session::export_session(input_file, output_file, cwd)
}

pub fn parse_thinking_level(s: &str) -> anyhow::Result<ThinkingLevel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" | "x-high" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => anyhow::bail!(
            "Invalid thinking level '{}'. Allowed: off, minimal, low, medium, high, xhigh, max",
            other
        ),
    }
}

/// Returns the request format used for a provider's API.
pub fn api_for_provider(provider: &str) -> &'static str {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => "anthropic-messages",
        "google" | "gemini" => "google-generative-ai",
        "google-vertex" => "google-vertex",
        "openai-responses" => "openai-responses",
        "mistral" => "mistral-conversations",
        "azure" | "azure-openai-responses" => "azure-openai-responses",
        "amazon-bedrock" | "bedrock" => "bedrock-converse-stream",
        _ => "openai-completions",
    }
}

/// Returns the default base URL for a provider.
///
/// Azure deployments carry the resource name in their URL and therefore have no
/// generic default; they fall back to the OpenAI endpoint.
pub fn base_url_for_provider(provider: &str) -> String {
    let provider = provider.to_ascii_lowercase();
    if let Some(url) = catalog::default_base_url(&provider) {
        return url.to_string();
    }
    match provider.as_str() {
        "gemini" => "https://generativelanguage.googleapis.com/v1beta".to_string(),
        "bedrock" => "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        "azure" | "azure-openai-responses" => std::env::var("AZURE_OPENAI_ENDPOINT")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        _ => "https://api.openai.com/v1".to_string(),
    }
}

/// Selects effective `(model_arg, provider_arg)` so an explicit `--model` is never overridden by `settings.default_provider`.
pub fn select_cli_model_and_provider<'a>(
    args_model: Option<&'a str>,
    args_provider: Option<&'a str>,
    settings: &'a Settings,
) -> (Option<&'a str>, Option<&'a str>) {
    if let Some(m) = args_model {
        (Some(m), args_provider)
    } else if let Some(p) = args_provider {
        let model = if settings
            .default_provider
            .as_deref()
            .is_some_and(|dp| dp.eq_ignore_ascii_case(p))
        {
            settings.default_model.as_deref()
        } else {
            None
        };
        (model, Some(p))
    } else {
        (
            settings.default_model.as_deref(),
            settings.default_provider.as_deref(),
        )
    }
}

/// Applies `--thinking` only when explicitly provided so `settings.default_thinking_level` is preserved otherwise.
pub fn apply_cli_thinking_override(
    session: &mut AgentSession,
    cli_thinking: Option<ThinkingLevel>,
) {
    if let Some(level) = cli_thinking {
        session.set_thinking_level_explicit(level);
    }
}

/// Resolves the initial startup model cleanly, preferring connected providers on zero-config launch.
pub fn select_startup_model(
    registry: &ModelRegistry,
    args: &CliArgs,
    settings: &Settings,
    connected: &[mehh_coding_agent::ConnectedProvider],
) -> Option<Model> {
    if args.model.is_some()
        || args.provider.is_some()
        || settings.default_model.is_some()
        || settings.default_provider.is_some()
    {
        let (model_arg, provider_arg) = select_cli_model_and_provider(
            args.model.as_deref(),
            args.provider.as_deref(),
            settings,
        );
        return resolve_or_custom_model_with_registry(
            registry,
            model_arg,
            provider_arg,
            settings.default_provider.as_deref(),
        );
    }

    if let Some(first_connected) = registry
        .models_for_connected_providers(connected)
        .into_iter()
        .next()
    {
        return Some(first_connected);
    }

    registry
        .resolve("claude-sonnet-4-5", None)
        .or_else(|| registry.models().first().cloned())
}

/// Resolves a model from the built-in catalog or constructs a fallback custom `Model`.
pub fn resolve_or_custom_model(
    model_arg: Option<&str>,
    provider_arg: Option<&str>,
    default_provider: Option<&str>,
) -> Option<Model> {
    resolve_or_custom_model_with_registry(
        &ModelRegistry::builtin_only(),
        model_arg,
        provider_arg,
        default_provider,
    )
}

/// Resolves a model from `registry` (built-in catalog + `models.json`) or constructs a fallback custom `Model`.
pub fn resolve_or_custom_model_with_registry(
    registry: &ModelRegistry,
    model_arg: Option<&str>,
    provider_arg: Option<&str>,
    default_provider: Option<&str>,
) -> Option<Model> {
    if let Some(m) = model_arg {
        let resolved = if let Some(p) = provider_arg {
            registry.resolve(p, Some(m))
        } else {
            registry.resolve(m, None)
        };

        if let Some(model) = resolved {
            return Some(model);
        }

        // Custom / unlisted fallback model: infer provider from explicit flag, slash prefix, or model prefix
        // before falling back to `default_provider`.
        let lower_m = m.to_ascii_lowercase();
        let (provider, id) = if let Some(p) = provider_arg {
            (p.to_ascii_lowercase(), m.to_string())
        } else if let Some((p, rest)) = m.split_once('/') {
            (p.to_ascii_lowercase(), rest.to_string())
        } else if lower_m.starts_with("claude-") {
            ("anthropic".to_string(), m.to_string())
        } else if lower_m.starts_with("gpt-")
            || lower_m.starts_with("chatgpt-")
            || lower_m == "o1"
            || lower_m.starts_with("o1-")
            || lower_m == "o3"
            || lower_m.starts_with("o3-")
            || lower_m == "o4"
            || lower_m.starts_with("o4-")
        {
            ("openai".to_string(), m.to_string())
        } else if lower_m.starts_with("gemini-") {
            ("google".to_string(), m.to_string())
        } else if lower_m.starts_with("mistral-")
            || lower_m.starts_with("codestral-")
            || lower_m.starts_with("pixtral-")
            || lower_m.starts_with("magistral-")
        {
            ("mistral".to_string(), m.to_string())
        } else if lower_m.starts_with("grok-") {
            ("xai".to_string(), m.to_string())
        } else if lower_m.starts_with("deepseek-") {
            ("deepseek".to_string(), m.to_string())
        } else if let Some(dp) = default_provider {
            (dp.to_ascii_lowercase(), m.to_string())
        } else {
            ("openai".to_string(), m.to_string())
        };

        let custom_cfg = registry.provider_config(&provider);
        let api = custom_cfg
            .and_then(|c| c.api.clone())
            .unwrap_or_else(|| api_for_provider(&provider).to_string());
        let base_url = custom_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| base_url_for_provider(&provider));
        let headers = custom_cfg.and_then(|c| c.headers.clone());
        if base_url.trim().is_empty() {
            return None;
        }

        let lower_id = id.to_ascii_lowercase();
        let reasoning = lower_id.starts_with("o1")
            || lower_id.starts_with("o3")
            || lower_id.starts_with("o4")
            || lower_id.contains("claude")
            || lower_id.contains("gemini-2.5")
            || lower_id.contains("gemini-3")
            || lower_id.starts_with("gpt-5")
            || lower_id.starts_with("deepseek-r1")
            || lower_id.starts_with("deepseek-reasoner")
            || lower_id.starts_with("grok-3")
            || lower_id.contains("magistral");

        Some(Model {
            id: id.clone(),
            name: id,
            api,
            provider,
            base_url,
            reasoning,
            input: vec!["text".to_string(), "image".to_string()],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers,
            ..Default::default()
        })
    } else if let Some(p) = provider_arg {
        registry.resolve(p, None)
    } else if let Some(dp) = default_provider {
        registry.resolve(dp, None)
    } else {
        registry.resolve("claude-sonnet-4-5", None)
    }
}

/// Selects and creates a SessionManager based on CLI flags and default persistence.
pub fn create_session_manager(
    no_session: bool,
    session_path: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<SessionManager> {
    if no_session {
        Ok(SessionManager::in_memory(cwd))
    } else if let Some(path) = session_path {
        SessionManager::open_file(path, cwd)
    } else {
        SessionManager::create_default_file(cwd)
    }
}

/// Returns the display label of a stored session.
pub fn session_label(summary: &SessionSummary) -> String {
    summary
        .name
        .clone()
        .or_else(|| summary.first_prompt.clone())
        .unwrap_or_else(|| summary.id.clone())
}

/// Renders a numbered list of stored sessions.
pub fn format_session_list(sessions: &[SessionSummary]) -> String {
    sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            format!(
                "  {}) {} — {} ({})",
                idx + 1,
                s.timestamp,
                session_label(s),
                s.id
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolves a session file from a path or a session id (exact or prefix).
pub fn resolve_resume_path(
    sessions: &[SessionSummary],
    spec: &str,
) -> anyhow::Result<PathBuf> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Session identifier cannot be empty");
    }

    let candidate = PathBuf::from(trimmed);
    if candidate.is_file() {
        return Ok(candidate);
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(found) = sessions
        .iter()
        .find(|s| s.id.to_ascii_lowercase() == lower)
    {
        return Ok(found.path.clone());
    }

    let prefixed: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| s.id.to_ascii_lowercase().starts_with(&lower))
        .collect();

    match prefixed.len() {
        1 => Ok(prefixed[0].path.clone()),
        0 => anyhow::bail!("No session found matching '{trimmed}'"),
        _ => anyhow::bail!(
            "Session id '{trimmed}' is ambiguous ({} sessions match)",
            prefixed.len()
        ),
    }
}

/// Parses a 1-based session selection typed by the user.
pub fn parse_session_choice(input: &str, count: usize) -> anyhow::Result<usize> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("No session selected (input closed)");
    }
    let idx: usize = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid session selection: '{trimmed}'"))?;
    if idx == 0 || idx > count {
        anyhow::bail!("Session selection out of range: {idx} (expected 1-{count})");
    }
    Ok(idx)
}

/// Builds the system prompt for a session from its final tool list (used by unit tests).
#[cfg(test)]
pub fn assemble_system_prompt(
    session: &AgentSession,
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
) -> String {
    let tool_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    mehh_coding_agent::build_system_prompt_with_skills(
        cwd,
        custom_prompt,
        append_prompt,
        &tool_names,
        session.skills(),
    )
}

/// Prints the stored sessions and reads a 1-based selection from stdin.
pub fn prompt_session_selection(sessions: &[SessionSummary]) -> anyhow::Result<PathBuf> {
    if sessions.is_empty() {
        anyhow::bail!("No saved sessions found for this directory.");
    }

    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "--resume needs a session id when stdin is not a terminal.\nAvailable sessions:\n{}",
            format_session_list(sessions)
        );
    }

    println!("Available sessions:");
    for (idx, s) in sessions.iter().enumerate() {
        println!("  {}) {} — {}", idx + 1, s.timestamp, session_label(s));
    }
    print!("Select session [1-{}]: ", sessions.len());
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = parse_session_choice(&line, sessions.len())?;
    Ok(sessions[choice - 1].path.clone())
}

/// Selects the session manager for this run, honouring the session related flags.
pub fn resolve_session_manager(args: &CliArgs, cwd: &Path) -> anyhow::Result<SessionManager> {
    let mut sm = if args.no_session {
        create_session_manager(true, None, cwd)?
    } else if let Some(path) = args.session.as_deref() {
        create_session_manager(false, Some(path), cwd)?
    } else if args.continue_session {
        let latest = mehh_coding_agent::latest_session_path(cwd)?.ok_or_else(|| {
            anyhow::anyhow!("No previous session found for {}", cwd.display())
        })?;
        create_session_manager(false, Some(&latest), cwd)?
    } else if let Some(spec) = args.resume.as_ref() {
        let sessions = list_sessions(cwd)?;
        let path = match spec {
            Some(target) => resolve_resume_path(&sessions, target)?,
            None => prompt_session_selection(&sessions)?,
        };
        create_session_manager(false, Some(&path), cwd)?
    } else if let Some(id) = args.session_id.as_deref() {
        let trimmed_id = id.trim();
        if trimmed_id.is_empty() {
            anyhow::bail!("--session-id cannot be empty");
        }
        let dir = match args.session_dir.as_deref() {
            Some(d) => d.to_path_buf(),
            None => mehh_coding_agent::session_dir_for_cwd(cwd)?,
        };
        let safe_id: String = trimmed_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let ts = mehh_ai::now_ms();
        let path = dir.join(format!("{ts}_{safe_id}.jsonl"));
        SessionManager::create_file_with_id(path, cwd, trimmed_id)?
    } else if let Some(dir) = args.session_dir.as_deref() {
        let ts = mehh_ai::now_ms();
        let path = dir.join(format!("{ts}_default.jsonl"));
        SessionManager::open_file(path, cwd)?
    } else {
        create_session_manager(false, None, cwd)?
    };

    if let Some(ref fork_entry_id) = args.fork {
        sm = sm.fork_at(Some(fork_entry_id.as_str()))?;
    }
    if let Some(ref name) = args.name {
        sm.set_session_name(Some(name.clone()))?;
    }
    Ok(sm)
}

fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();
    args.validate()?;

    // Validate CLI mode and thinking level early before loading sessions, credentials, or MCP.
    let cli_mode = parse_cli_mode(&args.mode)?;
    let cli_thinking = args
        .thinking
        .as_deref()
        .map(parse_thinking_level)
        .transpose()?;

    // Credential management short-circuits before a session or trust check is created.
    if let Some(provider) = args.login.as_deref() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let message = rt.block_on(perform_login(provider, args.api_key.as_deref(), None))?;
        println!("{message}");
        return Ok(());
    }
    if let Some(provider) = args.logout.as_deref() {
        let removed = perform_logout(provider, None)?;
        if removed {
            println!("Removed stored credential for {provider}");
        } else {
            println!("No stored credential for {provider}");
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;

    if let Some(ref export_input) = args.export {
        let export_output = args.prompt.first().map(Path::new);
        return handle_export(export_input, export_output, &cwd);
    }

    let is_faux_fast = !args.list_models
        && args.models.is_none()
        && (args
            .provider
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("faux"))
            || args
                .model
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case("faux") || m.eq_ignore_ascii_case("faux-1")));
    let model_registry = if is_faux_fast {
        ModelRegistry::builtin_only()
    } else {
        match ModelRegistry::load_with_discovered(None, None) {
            Ok(reg) => reg,
            Err(disc_err) => {
                let base = ModelRegistry::load_default()?;
                eprintln!("Warning: could not load discovered models cache: {disc_err}");
                base
            }
        }
    };
    if args.list_models {
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        for m in model_registry.models() {
            writeln!(
                out,
                "{}/{} - {} ({} tokens)",
                m.provider, m.id, m.name, m.context_window
            )?;
        }
        out.flush()?;
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async_cli(
        args,
        cli_mode,
        cli_thinking,
        cwd,
        model_registry,
    ))
}

async fn run_async_cli(
    args: CliArgs,
    cli_mode: CliMode,
    cli_thinking: Option<ThinkingLevel>,
    cwd: PathBuf,
    model_registry: ModelRegistry,
) -> anyhow::Result<()> {
    let is_interactive = should_run_interactive(
        &args,
        cli_mode,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    );
    let mut boot = bootstrap::bootstrap_session(
        &args,
        cli_mode,
        cli_thinking,
        cwd,
        &model_registry,
        is_interactive,
    )
    .await?;

    let run_res = if boot.is_interactive {
        let summaries = boot
            .mcp_manager
            .as_ref()
            .map(|mgr| mgr.summaries().to_vec())
            .unwrap_or_default();
        Box::pin(run_interactive_mode(
            &mut boot.session,
            &summaries,
            boot.mcp_manager.as_mut(),
            args.codemode,
        ))
        .await
    } else {
        match boot.cli_mode {
            CliMode::Rpc => {
                rpc::run_rpc_mode(
                    &mut boot.session,
                    tokio::io::BufReader::new(tokio::io::stdin()),
                    tokio::io::stdout(),
                )
                .await
            }
            CliMode::Text | CliMode::Interactive => {
                run_print_flow(&mut boot.session, &args, OutputMode::Text).await
            }
            CliMode::Json => {
                run_print_flow(&mut boot.session, &args, OutputMode::Json).await
            }
        }
    };

    bootstrap::close_mcp(boot.mcp_manager.as_ref()).await;
    run_res
}

async fn run_print_flow(
    agent_session: &mut AgentSession,
    args: &CliArgs,
    output_mode: OutputMode,
) -> anyhow::Result<()> {
    let processed = if args.no_context_files {
        ProcessedPromptArgs {
            text: args.prompt.join(" "),
            blocks: Vec::new(),
        }
    } else {
        process_file_arguments(&agent_session.cwd, &args.prompt)?
    };
    // Piped stdin is added to the prompt argument, never discarded. For
    // example `git diff | mehh -p "review"` must include both pieces.
    let mut piped = String::new();
    if !io::stdin().is_terminal() {
        piped = mehh_cli::read_piped_stdin_for_prompt(io::stdin(), &args.prompt).await?;
    }
    let user_content = mehh_cli::build_print_user_content(processed, &piped)?;

    run_print_mode_content(agent_session, user_content, output_mode).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::faux;

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn make_session_summary(id: &str, path: &str) -> SessionSummary {
        SessionSummary {
            path: PathBuf::from(path),
            id: id.to_string(),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            entry_count: 3,
            name: None,
            first_prompt: Some("first prompt".to_string()),
        }
    }

    #[test]
    fn test_cli_args_print_and_prompt() {
        let args = CliArgs::try_parse_from(["mehh", "-p", "write a function"]).unwrap();
        assert!(args.print);
        assert_eq!(args.prompt, vec!["write a function"]);
        assert_eq!(args.mode, "text");
        assert!(!args.list_models);
        assert!(!args.no_mcp);
        assert!(!args.codemode);
    }

    #[test]
    fn test_cli_args_no_mcp_and_codemode_flags() {
        let args_default = CliArgs::try_parse_from(["mehh", "test query"]).unwrap();
        assert!(!args_default.no_mcp);
        assert!(!args_default.codemode);

        let args_no_mcp = CliArgs::try_parse_from(["mehh", "--no-mcp", "test query"]).unwrap();
        assert!(args_no_mcp.no_mcp);
        assert!(!args_no_mcp.codemode);

        let args_codemode = CliArgs::try_parse_from(["mehh", "--codemode", "test query"]).unwrap();
        assert!(!args_codemode.no_mcp);
        assert!(args_codemode.codemode);

        let args_both = CliArgs::try_parse_from(["mehh", "--no-mcp", "--codemode", "test query"]).unwrap();
        assert!(args_both.no_mcp);
        assert!(args_both.codemode);
    }

    #[test]
    fn test_cli_args_mode_interactive_and_tui() {
        let args_inter = CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert_eq!(args_inter.mode, "interactive");
        assert_eq!(parse_cli_mode(&args_inter.mode).unwrap(), CliMode::Interactive);

        let args_tui = CliArgs::try_parse_from(["mehh", "--mode", "tui"]).unwrap();
        assert_eq!(args_tui.mode, "tui");
        assert_eq!(parse_cli_mode(&args_tui.mode).unwrap(), CliMode::Interactive);
    }

    #[test]
    fn test_cli_args_mode_json() {
        let args = CliArgs::try_parse_from(["mehh", "--mode", "json", "-p", "explain code"]).unwrap();
        assert_eq!(args.mode, "json");
        assert!(args.print);
        assert_eq!(args.prompt, vec!["explain code"]);
    }

    #[test]
    fn test_cli_args_mode_rpc() {
        let args = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        assert_eq!(args.mode, "rpc");
        assert!(args.prompt.is_empty());
    }

    #[test]
    fn test_cli_args_model_and_provider() {
        let args = CliArgs::try_parse_from(["mehh", "-m", "gpt-4o", "--provider", "openai", "hello"]).unwrap();
        assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert_eq!(args.prompt, vec!["hello"]);
    }

    #[test]
    fn test_cli_args_list_models() {
        let args = CliArgs::try_parse_from(["mehh", "--list-models"]).unwrap();
        assert!(args.list_models);
        assert!(args.prompt.is_empty());
    }

    #[test]
    fn test_cli_args_session_and_tools() {
        let args = CliArgs::try_parse_from([
            "mehh",
            "--session",
            "session.json",
            "--no-tools",
            "test query",
        ])
        .unwrap();
        assert_eq!(args.session, Some(PathBuf::from("session.json")));
        assert!(args.no_tools);
        assert!(!args.no_session);
        assert_eq!(args.prompt, vec!["test query"]);
    }

    #[test]
    fn test_cli_args_no_session() {
        let args = CliArgs::try_parse_from(["mehh", "--no-session", "test query"]).unwrap();
        assert!(args.no_session);
        assert!(args.session.is_none());
    }

    #[test]
    fn test_cli_args_no_session_vs_default() {
        let args_default = CliArgs::try_parse_from(["mehh", "test query"]).unwrap();
        assert!(!args_default.no_session);
        assert!(args_default.session.is_none());

        let args_no_session = CliArgs::try_parse_from(["mehh", "--no-session", "test query"]).unwrap();
        assert!(args_no_session.no_session);
        assert!(args_no_session.session.is_none());

        let args_session = CliArgs::try_parse_from(["mehh", "--session", "my_session.jsonl", "test query"]).unwrap();
        assert!(!args_session.no_session);
        assert_eq!(args_session.session, Some(PathBuf::from("my_session.jsonl")));
    }

    #[test]
    fn test_cli_args_extended_options() {
        let args = CliArgs::try_parse_from([
            "mehh",
            "--thinking",
            "high",
            "--system-prompt",
            "custom prompt",
            "--append-system-prompt",
            "extra rules",
            "--api-key",
            "test-key",
        ])
        .unwrap();
        assert_eq!(args.thinking.as_deref(), Some("high"));
        assert_eq!(args.system_prompt.as_deref(), Some("custom prompt"));
        assert_eq!(args.append_system_prompt.as_deref(), Some("extra rules"));
        assert_eq!(args.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn test_cli_args_continue_flag() {
        let args = CliArgs::try_parse_from(["mehh", "-c"]).unwrap();
        assert!(args.continue_session);
        assert!(args.resume.is_none());
        assert!(args.login.is_none());
        assert!(args.logout.is_none());

        let args_long = CliArgs::try_parse_from(["mehh", "--continue"]).unwrap();
        assert!(args_long.continue_session);
    }

    #[test]
    fn test_cli_args_resume_flag() {
        let without_value = CliArgs::try_parse_from(["mehh", "--resume"]).unwrap();
        assert_eq!(without_value.resume, Some(None));

        let short_without_value = CliArgs::try_parse_from(["mehh", "-r"]).unwrap();
        assert_eq!(short_without_value.resume, Some(None));

        let with_value = CliArgs::try_parse_from(["mehh", "--resume=abc123"]).unwrap();
        assert_eq!(with_value.resume, Some(Some("abc123".to_string())));

        let short_with_value = CliArgs::try_parse_from(["mehh", "-r=abc123"]).unwrap();
        assert_eq!(short_with_value.resume, Some(Some("abc123".to_string())));

        // The session id needs `=`, so a following word stays a prompt.
        let spaced = CliArgs::try_parse_from(["mehh", "--resume", "abc123"]).unwrap();
        assert_eq!(spaced.resume, Some(None));
        assert_eq!(spaced.prompt, vec!["abc123"]);
    }

    #[test]
    fn test_cli_args_login_and_logout_flags() {
        let login = CliArgs::try_parse_from(["mehh", "--login", "anthropic"]).unwrap();
        assert_eq!(login.login.as_deref(), Some("anthropic"));
        assert!(login.logout.is_none());

        let login_with_key = CliArgs::try_parse_from([
            "mehh",
            "--login",
            "anthropic",
            "--api-key",
            "sk-test",
        ])
        .unwrap();
        assert_eq!(login_with_key.login.as_deref(), Some("anthropic"));
        assert_eq!(login_with_key.api_key.as_deref(), Some("sk-test"));

        let logout = CliArgs::try_parse_from(["mehh", "--logout", "openai"]).unwrap();
        assert_eq!(logout.logout.as_deref(), Some("openai"));
        assert!(logout.login.is_none());
    }

    #[test]
    fn test_parse_cli_mode() {
        assert_eq!(parse_cli_mode("text").unwrap(), CliMode::Text);
        assert_eq!(parse_cli_mode("TEXT").unwrap(), CliMode::Text);
        assert_eq!(parse_cli_mode("json").unwrap(), CliMode::Json);
        assert_eq!(parse_cli_mode("JSON").unwrap(), CliMode::Json);
        assert_eq!(parse_cli_mode("rpc").unwrap(), CliMode::Rpc);
        assert_eq!(parse_cli_mode("RPC").unwrap(), CliMode::Rpc);
        assert_eq!(parse_cli_mode("interactive").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("INTERACTIVE").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("tui").unwrap(), CliMode::Interactive);
        assert_eq!(parse_cli_mode("TUI").unwrap(), CliMode::Interactive);
        assert!(parse_cli_mode("yaml").is_err());
    }

    #[test]
    fn test_should_run_interactive_default_text_mode_with_tty() {
        // When mode is text, no prompt, print is false, and stdin+stdout are terminals -> true
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        assert_eq!(args.mode, "text");
        assert!(args.prompt.is_empty());
        assert!(!args.print);

        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(should_run_interactive(&args, cli_mode, true, true));
        assert!(!should_run_interactive(&args, cli_mode, true, false));
    }

    #[test]
    fn test_should_run_interactive_piped_stdin() {
        // When mode is text and no prompt, but stdin is NOT a terminal (piped) -> false
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, false, true));
        assert!(!should_run_interactive(&args, cli_mode, false, false));
    }

    #[test]
    fn test_should_run_interactive_with_prompt_arg() {
        // When prompt is supplied in text mode, even on TTY -> false
        let args = CliArgs::try_parse_from(["mehh", "do something"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true, true));
        assert!(!should_run_interactive(&args, cli_mode, false, true));
        assert!(!should_run_interactive(&args, cli_mode, true, false));
    }

    #[test]
    fn test_should_run_interactive_print_flag() {
        // When print flag -p is set in text mode, even on TTY with no prompt -> false
        let args = CliArgs::try_parse_from(["mehh", "-p"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true, true));
        assert!(!should_run_interactive(&args, cli_mode, false, true));
        assert!(!should_run_interactive(&args, cli_mode, true, false));
    }

    #[test]
    fn test_should_run_interactive_explicit_interactive_mode() {
        // Explicit interactive mode requires stdout to be a terminal so redirected stdout
        // does not enter TUI raw mode.
        let args1 = CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert!(should_run_interactive(&args1, CliMode::Interactive, true, true));
        assert!(should_run_interactive(&args1, CliMode::Interactive, false, true));
        assert!(!should_run_interactive(&args1, CliMode::Interactive, true, false));
        assert!(!should_run_interactive(&args1, CliMode::Interactive, false, false));

        let args2 = CliArgs::try_parse_from(["mehh", "--mode", "interactive", "-p", "some prompt"]).unwrap();
        assert!(should_run_interactive(&args2, CliMode::Interactive, true, true));
        assert!(should_run_interactive(&args2, CliMode::Interactive, false, true));
        assert!(!should_run_interactive(&args2, CliMode::Interactive, true, false));

        let args3 = CliArgs::try_parse_from(["mehh", "--mode", "tui"]).unwrap();
        let mode3 = parse_cli_mode(&args3.mode).unwrap();
        assert_eq!(mode3, CliMode::Interactive);
        assert!(should_run_interactive(&args3, mode3, true, true));
        assert!(should_run_interactive(&args3, mode3, false, true));
        assert!(!should_run_interactive(&args3, mode3, true, false));
    }

    #[test]
    fn test_should_run_interactive_json_and_rpc_modes() {
        // Json mode never runs interactive mode
        let args_json = CliArgs::try_parse_from(["mehh", "--mode", "json"]).unwrap();
        let mode_json = parse_cli_mode(&args_json.mode).unwrap();
        assert_eq!(mode_json, CliMode::Json);
        assert!(!should_run_interactive(&args_json, mode_json, true, true));
        assert!(!should_run_interactive(&args_json, mode_json, false, true));
        assert!(!should_run_interactive(&args_json, mode_json, true, false));

        // Rpc mode never runs interactive mode
        let args_rpc = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        let mode_rpc = parse_cli_mode(&args_rpc.mode).unwrap();
        assert_eq!(mode_rpc, CliMode::Rpc);
        assert!(!should_run_interactive(&args_rpc, mode_rpc, true, true));
        assert!(!should_run_interactive(&args_rpc, mode_rpc, false, true));
        assert!(!should_run_interactive(&args_rpc, mode_rpc, true, false));
    }

    #[test]
    fn test_parse_thinking_level() {
        assert_eq!(parse_thinking_level("off").unwrap(), ThinkingLevel::Off);
        assert_eq!(parse_thinking_level("minimal").unwrap(), ThinkingLevel::Minimal);
        assert_eq!(parse_thinking_level("low").unwrap(), ThinkingLevel::Low);
        assert_eq!(parse_thinking_level("medium").unwrap(), ThinkingLevel::Medium);
        assert_eq!(parse_thinking_level("high").unwrap(), ThinkingLevel::High);
        assert_eq!(parse_thinking_level("xhigh").unwrap(), ThinkingLevel::Xhigh);
        assert_eq!(parse_thinking_level("x-high").unwrap(), ThinkingLevel::Xhigh);
        assert_eq!(parse_thinking_level("max").unwrap(), ThinkingLevel::Max);
        assert!(parse_thinking_level("unknown").is_err());
    }

    #[test]
    fn test_resolve_or_custom_model_builtin() {
        let m = resolve_or_custom_model(Some("claude-sonnet-4-5"), None, None).unwrap();
        assert_eq!(m.id, "claude-sonnet-4-5");
        assert_eq!(m.provider, "anthropic");

        let m_openai = resolve_or_custom_model(Some("gpt-4o"), Some("openai"), None).unwrap();
        assert_eq!(m_openai.id, "gpt-4o");
        assert_eq!(m_openai.provider, "openai");
    }

    #[test]
    fn test_resolve_or_custom_model_fallback_with_explicit_provider() {
        let m = resolve_or_custom_model(Some("my-custom-model"), Some("anthropic"), None).unwrap();
        assert_eq!(m.id, "my-custom-model");
        assert_eq!(m.name, "my-custom-model");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.api, "anthropic-messages");
        assert_eq!(m.base_url, "https://api.anthropic.com/v1");
        assert_eq!(m.context_window, 128_000);
        assert_eq!(m.max_tokens, 16_384);
        assert!(!m.reasoning);

        let m_google = resolve_or_custom_model(Some("custom-gemini"), Some("google"), None).unwrap();
        assert_eq!(m_google.provider, "google");
        assert_eq!(m_google.api, "google-generative-ai");
        assert_eq!(m_google.base_url, "https://generativelanguage.googleapis.com/v1beta");

        let m_resp = resolve_or_custom_model(Some("my-model"), Some("openai-responses"), None).unwrap();
        assert_eq!(m_resp.provider, "openai-responses");
        assert_eq!(m_resp.api, "openai-responses");
    }

    #[test]
    fn test_resolve_or_custom_model_fallback_inferred_provider_and_reasoning() {
        // Claude prefix
        let m_claude = resolve_or_custom_model(Some("claude-future-v1"), None, None).unwrap();
        assert_eq!(m_claude.provider, "anthropic");
        assert_eq!(m_claude.api, "anthropic-messages");
        assert_eq!(m_claude.base_url, "https://api.anthropic.com/v1");
        assert!(m_claude.reasoning);

        // Gemini prefix
        let m_gemini = resolve_or_custom_model(Some("gemini-3.0-pro"), None, None).unwrap();
        assert_eq!(m_gemini.provider, "google");
        assert_eq!(m_gemini.api, "google-generative-ai");
        assert_eq!(m_gemini.base_url, "https://generativelanguage.googleapis.com/v1beta");
        assert!(m_gemini.reasoning);

        let m_gemini_25 = resolve_or_custom_model(Some("gemini-2.5-custom"), None, None).unwrap();
        assert!(m_gemini_25.reasoning);

        // Reasoning prefixes: o1, o3, o4
        let m_o1 = resolve_or_custom_model(Some("o1-custom"), None, None).unwrap();
        assert_eq!(m_o1.provider, "openai");
        assert_eq!(m_o1.api, "openai-completions");
        assert_eq!(m_o1.base_url, "https://api.openai.com/v1");
        assert!(m_o1.reasoning);

        let m_o3 = resolve_or_custom_model(Some("o3-custom"), None, None).unwrap();
        assert!(m_o3.reasoning);

        let m_o4 = resolve_or_custom_model(Some("o4-custom"), None, None).unwrap();
        assert!(m_o4.reasoning);

        // Inferred from settings default_provider
        let m_default_prov = resolve_or_custom_model(Some("custom-llm"), None, Some("gemini")).unwrap();
        assert_eq!(m_default_prov.provider, "gemini");
        assert_eq!(m_default_prov.api, "google-generative-ai");

        // Fallback to openai when no inference applies
        let m_openai_default = resolve_or_custom_model(Some("unlisted-model"), None, None).unwrap();
        assert_eq!(m_openai_default.provider, "openai");
        assert_eq!(m_openai_default.api, "openai-completions");
        assert_eq!(m_openai_default.base_url, "https://api.openai.com/v1");
        assert!(!m_openai_default.reasoning);

        // Slash notation
        let m_slash = resolve_or_custom_model(Some("custom-org/llama-70b"), None, None).unwrap();
        assert_eq!(m_slash.provider, "custom-org");
        assert_eq!(m_slash.id, "llama-70b");
        assert_eq!(m_slash.api, "openai-completions");
    }

    #[test]
    fn test_resolve_or_custom_model_none_model() {
        let m_default = resolve_or_custom_model(None, None, None).unwrap();
        assert_eq!(m_default.id, "claude-sonnet-4-5");

        let m_prov = resolve_or_custom_model(None, Some("openai"), None).unwrap();
        assert_eq!(m_prov.id, "gpt-4o");

        let m_dp = resolve_or_custom_model(None, None, Some("google")).unwrap();
        assert_eq!(m_dp.id, "gemini-2.5-pro");
    }

    #[test]
    fn test_api_for_provider_covers_every_request_format() {
        assert_eq!(api_for_provider("anthropic"), "anthropic-messages");
        assert_eq!(api_for_provider("google"), "google-generative-ai");
        assert_eq!(api_for_provider("gemini"), "google-generative-ai");
        assert_eq!(api_for_provider("google-vertex"), "google-vertex");
        assert_eq!(api_for_provider("openai"), "openai-completions");
        assert_eq!(api_for_provider("openai-responses"), "openai-responses");
        assert_eq!(api_for_provider("mistral"), "mistral-conversations");
        assert_eq!(
            api_for_provider("azure-openai-responses"),
            "azure-openai-responses"
        );
        assert_eq!(
            api_for_provider("amazon-bedrock"),
            "bedrock-converse-stream"
        );
        assert_eq!(api_for_provider("openrouter"), "openai-completions");
    }

    #[test]
    fn test_resolve_or_custom_model_maps_every_provider_to_its_api_and_base_url() {
        // (provider, expected api, expected base url)
        let expectations: &[(&str, &str, &str)] = &[
            (
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com/v1",
            ),
            ("openai", "openai-completions", "https://api.openai.com/v1"),
            (
                "openai-responses",
                "openai-responses",
                "https://api.openai.com/v1",
            ),
            (
                "google",
                "google-generative-ai",
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                "google-vertex",
                "google-vertex",
                "https://us-central1-aiplatform.googleapis.com/v1",
            ),
            (
                "amazon-bedrock",
                "bedrock-converse-stream",
                "https://bedrock-runtime.us-east-1.amazonaws.com",
            ),
            (
                "mistral",
                "mistral-conversations",
                "https://api.mistral.ai/v1",
            ),
            ("openrouter", "openai-completions", "https://openrouter.ai/api/v1"),
            ("groq", "openai-completions", "https://api.groq.com/openai/v1"),
            (
                "deepseek",
                "openai-completions",
                "https://api.deepseek.com/v1",
            ),
            ("xai", "openai-completions", "https://api.x.ai/v1"),
            (
                "cerebras",
                "openai-completions",
                "https://api.cerebras.ai/v1",
            ),
            (
                "together",
                "openai-completions",
                "https://api.together.xyz/v1",
            ),
            (
                "fireworks",
                "openai-completions",
                "https://api.fireworks.ai/inference/v1",
            ),
            (
                "github-copilot",
                "openai-completions",
                "https://api.githubcopilot.com",
            ),
        ];

        for (provider, api, base_url) in expectations {
            let m = resolve_or_custom_model(Some("some-custom-model"), Some(provider), None).unwrap();
            assert_eq!(m.provider, *provider);
            assert_eq!(m.api, *api, "wrong api for {provider}");
            assert_eq!(m.base_url, *base_url, "wrong base url for {provider}");
            if let Some(catalog_url) = catalog::default_base_url(provider) {
                assert_eq!(m.base_url, catalog_url);
            }
        }
    }

    #[test]
    fn test_resolve_or_custom_model_normalizes_provider_case() {
        let m = resolve_or_custom_model(Some("my-model"), Some("Anthropic"), None).unwrap();
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.api, "anthropic-messages");
        assert_eq!(m.base_url, "https://api.anthropic.com/v1");
    }

    #[test]
    fn test_base_url_for_provider_aliases() {
        assert_eq!(
            base_url_for_provider("gemini"),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(
            base_url_for_provider("bedrock"),
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert_eq!(
            base_url_for_provider("unknown-provider"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn test_resolve_resume_path_by_id_and_prefix() {
        let sessions = vec![
            make_session_summary("abcdef-1234", "session-alpha.jsonl"),
            make_session_summary("zzzz-9999", "session-beta.jsonl"),
        ];

        assert_eq!(
            resolve_resume_path(&sessions, "abcdef-1234").unwrap(),
            PathBuf::from("session-alpha.jsonl")
        );
        assert_eq!(
            resolve_resume_path(&sessions, "ABCD").unwrap(),
            PathBuf::from("session-alpha.jsonl")
        );
        assert_eq!(
            resolve_resume_path(&sessions, "zzzz").unwrap(),
            PathBuf::from("session-beta.jsonl")
        );

        assert!(resolve_resume_path(&sessions, "qqqq").is_err());
        assert!(resolve_resume_path(&sessions, "   ").is_err());
    }

    #[test]
    fn test_parse_session_choice() {
        assert_eq!(parse_session_choice("1", 3).unwrap(), 1);
        assert_eq!(parse_session_choice(" 3 ", 3).unwrap(), 3);
        assert!(parse_session_choice("0", 3).is_err());
        assert!(parse_session_choice("4", 3).is_err());
        assert!(parse_session_choice("not-a-number", 3).is_err());
        assert!(parse_session_choice("", 3).is_err());
    }

    #[test]
    fn test_assemble_system_prompt_keeps_custom_prompt_and_addendum() {
        let dir = make_temp_dir("mehh_cli_prompt");
        let session_mgr = SessionManager::in_memory(&dir);
        let mut session = AgentSession::new(
            dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            session_mgr,
            mehh_coding_agent::Settings::default(),
        );
        session.set_tools(Vec::new());

        let prompt = assemble_system_prompt(
            &session,
            &dir,
            Some("ALWAYS ANSWER IN FRENCH"),
            Some("EXTRA RULES"),
        );

        assert!(
            prompt.starts_with("ALWAYS ANSWER IN FRENCH"),
            "the custom prompt must lead the system prompt: {prompt}"
        );
        assert!(prompt.contains("<addendum>\nEXTRA RULES\n</addendum>"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_format_session_list_uses_labels() {
        let sessions = vec![make_session_summary("abc-1", "session-alpha.jsonl")];
        let rendered = format_session_list(&sessions);
        assert!(rendered.contains("1)"));
        assert!(rendered.contains("first prompt"));
        assert!(rendered.contains("abc-1"));
    }

    #[test]
    fn test_resolve_session_manager_no_session_is_in_memory() {
        let dir = make_temp_dir("mehh_cli_resolve");
        let args = CliArgs::try_parse_from(["mehh", "--no-session"]).unwrap();
        let sm = resolve_session_manager(&args, &dir).unwrap();
        assert!(sm.file_path().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_resolve_session_manager_continue_without_sessions_fails() {
        let _lock = crate::bootstrap::tests::ENV_LOCK.blocking_lock();
        let env = crate::bootstrap::tests::TempEnv::new("cli_continue", None);
        let args = CliArgs::try_parse_from(["mehh", "-c"]).unwrap();
        let err = match resolve_session_manager(&args, &env.cwd) {
            Ok(_) => panic!("expected --continue to fail without a previous session"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("No previous session"));
    }

    #[test]
    fn test_resolve_session_manager_uses_explicit_session_path() {
        let dir = make_temp_dir("mehh_cli_explicit");
        let session_file = dir.join("explicit.jsonl");
        let args = CliArgs::try_parse_from(["mehh", "--session", "placeholder.jsonl"]).unwrap();
        let args = CliArgs {
            session: Some(session_file.clone()),
            ..args
        };
        let sm = resolve_session_manager(&args, &dir).unwrap();
        assert_eq!(sm.file_path(), Some(session_file.as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_create_session_manager_no_session() {
        let temp_dir = std::env::temp_dir().join(format!(
            "mehh_cli_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);

        let sm = create_session_manager(true, None, &temp_dir).unwrap();
        assert!(sm.file_path().is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_create_session_manager_explicit_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "mehh_cli_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);
        let explicit_file = temp_dir.join("test_session.jsonl");

        let sm = create_session_manager(false, Some(&explicit_file), &temp_dir).unwrap();
        assert_eq!(sm.file_path(), Some(explicit_file.as_path()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cli_model_precedence_and_thinking_preservation() {
        let dir = make_temp_dir("mehh_cli_prec");
        let settings = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-sonnet-4-5".to_string()),
            default_thinking_level: Some(ThinkingLevel::High),
            ..Settings::default()
        };

        // Explicit --model gpt-4o without --provider ignores settings.default_provider ("anthropic")
        let (model_arg, prov_arg) =
            select_cli_model_and_provider(Some("gpt-4o"), None, &settings);
        assert_eq!(model_arg, Some("gpt-4o"));
        assert_eq!(prov_arg, None);
        let resolved = resolve_or_custom_model(
            model_arg,
            prov_arg,
            settings.default_provider.as_deref(),
        )
        .unwrap();
        assert_eq!(resolved.provider, "openai");
        assert_eq!(resolved.id, "gpt-4o");

        // Unlisted gpt model also infers openai before falling back to settings.default_provider
        let unlisted_gpt = resolve_or_custom_model(
            Some("gpt-5-future"),
            None,
            settings.default_provider.as_deref(),
        )
        .unwrap();
        assert_eq!(unlisted_gpt.provider, "openai");

        // Omitting --thinking preserves settings.default_thinking_level
        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            SessionManager::in_memory(&dir),
            settings,
            false,
        );
        apply_cli_thinking_override(&mut session, None);
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::High));

        apply_cli_thinking_override(&mut session, Some(ThinkingLevel::Low));
        assert_eq!(session.thinking_level(), Some(ThinkingLevel::Low));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_select_startup_model_prefers_connected_provider_over_hardcoded_default() {
        let dir = std::env::temp_dir().join(format!(
            "mehh_cli_startup_model_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let disc_path = dir.join("discovered-models.json");
        let models_path = dir.join("models.json");
        mehh_coding_agent::models_config::save_discovered_provider_models(
            "google",
            &[Model {
                id: "gemini-2.5-pro".to_string(),
                name: "Gemini 2.5 Pro".to_string(),
                api: "google-generative-ai".to_string(),
                provider: "google".to_string(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                reasoning: true,
                ..Default::default()
            }],
            Some(&disc_path),
        )
        .unwrap();
        let registry =
            ModelRegistry::load_with_discovered(Some(&models_path), Some(&disc_path)).unwrap();
        let args = CliArgs::parse_from(["mehh"]);
        let settings = Settings::default();
        let connected = vec![mehh_coding_agent::ConnectedProvider {
            provider_id: "google".to_string(),
            source: mehh_coding_agent::CredentialSource::AuthFile,
            has_oauth: false,
        }];

        let chosen = select_startup_model(&registry, &args, &settings, &connected).unwrap();
        assert_eq!(chosen.provider, "google");
        assert_eq!(chosen.id, "gemini-2.5-pro");

        let explicit_args = CliArgs::parse_from(["mehh", "--model", "gpt-4o"]);
        let explicit =
            select_startup_model(&registry, &explicit_args, &settings, &connected).unwrap();
        assert_eq!(explicit.provider, "openai");
        assert_eq!(explicit.id, "gpt-4o");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cli_rejects_conflicting_approve_and_no_approve() {
        let args1 = CliArgs::try_parse_from(["mehh", "--approve", "--no-approve"]).unwrap();
        assert!(args1.validate().is_err());

        let args2 = CliArgs::try_parse_from(["mehh", "--trust", "--no-approve"]).unwrap();
        assert!(args2.validate().is_err());

        let args3 = CliArgs::try_parse_from(["mehh", "--approve"]).unwrap();
        assert!(args3.validate().is_ok());
        // --approve only changes tool approvals, never project trust.
        assert_eq!(args3.trust_override(), mehh_coding_agent::TrustOverride::None);

        let args4 = CliArgs::try_parse_from(["mehh", "--no-approve"]).unwrap();
        assert!(args4.validate().is_ok());
        assert_eq!(args4.trust_override(), mehh_coding_agent::TrustOverride::NoApprove);
    }

    #[test]
    fn cli_parses_export_input_and_optional_output() {
        let args = CliArgs::try_parse_from(["mehh", "--export", "in_session.jsonl", "out.jsonl"]).unwrap();
        assert_eq!(args.export, Some(PathBuf::from("in_session.jsonl")));
        assert_eq!(args.prompt, vec!["out.jsonl"]);

        let args_no_out = CliArgs::try_parse_from(["mehh", "--export", "in_session.jsonl"]).unwrap();
        assert_eq!(args_no_out.export, Some(PathBuf::from("in_session.jsonl")));
        assert!(args_no_out.prompt.is_empty());
    }

    #[test]
    fn tool_selection_flags_respect_tools_exclude_tools_and_no_builtin_tools() {
        let temp_dir = make_temp_dir("mehh_tools_test");

        // 1. Default tools (11 tools including web_search and web_fetch; powershell is opt-in)
        let args_def = CliArgs::try_parse_from(["mehh"]).unwrap();
        let tools_def = resolve_cli_tools(&args_def, temp_dir.clone()).unwrap();
        assert_eq!(tools_def.len(), 11);
        assert!(!tools_def.iter().any(|t| t.name() == "powershell"));

        // 2. Opt-in powershell via --tools
        let args_ps = CliArgs::try_parse_from(["mehh", "--tools", "powershell"]).unwrap();
        let tools_ps = resolve_cli_tools(&args_ps, temp_dir.clone()).unwrap();
        assert_eq!(tools_ps.len(), 1);
        assert_eq!(tools_ps[0].name(), "powershell");

        // 3. Exclude tools
        let args_ex = CliArgs::try_parse_from(["mehh", "--exclude-tools", "bash,edit"]).unwrap();
        let tools_ex = resolve_cli_tools(&args_ex, temp_dir.clone()).unwrap();
        assert_eq!(tools_ex.len(), 9);
        assert!(!tools_ex.iter().any(|t| t.name() == "bash" || t.name() == "edit"));

        // 4. --no-builtin-tools
        let args_nobuiltin = CliArgs::try_parse_from(["mehh", "--no-builtin-tools"]).unwrap();
        let tools_nobuiltin = resolve_cli_tools(&args_nobuiltin, temp_dir.clone()).unwrap();
        assert!(tools_nobuiltin.is_empty());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cli_tools_flag_accepts_todo_and_subagent_in_include_and_exclude() {
        let temp_dir = make_temp_dir("mehh_tools_todo");

        // todo/subagent are toggled by main.rs itself, not by select_coding_tools.
        // They must be accepted and filtered before that call.
        let inc = CliArgs::try_parse_from(["mehh", "--tools", "read,todo,subagent"]).unwrap();
        let tools = resolve_cli_tools(&inc, temp_dir.clone()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "read");

        let exc = CliArgs::try_parse_from(["mehh", "--exclude-tools", "subagent,todo"]).unwrap();
        let tools = resolve_cli_tools(&exc, temp_dir.clone()).unwrap();
        assert!(!tools
            .iter()
            .any(|t| t.name() == "subagent" || t.name() == "todo"));
        assert_eq!(tools.len(), 11);

        // Unknown names must still be rejected.
        let bogus = CliArgs::try_parse_from(["mehh", "--tools", "bogus"]).unwrap();
        assert!(resolve_cli_tools(&bogus, temp_dir.clone()).is_err());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_resolve_cli_tools_with_web_respects_disabled_settings() {
        let temp_dir = make_temp_dir("mehh_web_cli_cfg");
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();

        let web_disabled = mehh_tools::WebConfig {
            enabled: false,
            ..Default::default()
        };
        let tools = resolve_cli_tools_with_web(&args, temp_dir.clone(), web_disabled).unwrap();
        assert_eq!(tools.len(), 9);
        assert!(!tools
            .iter()
            .any(|t| t.name() == "web_search" || t.name() == "web_fetch"));

        let args_ex = CliArgs::try_parse_from(["mehh", "--exclude-tools", "web_search,web_fetch"]).unwrap();
        let tools_ex =
            resolve_cli_tools_with_web(&args_ex, temp_dir.clone(), mehh_tools::WebConfig::default())
                .unwrap();
        assert_eq!(tools_ex.len(), 9);
        assert!(!tools_ex
            .iter()
            .any(|t| t.name() == "web_search" || t.name() == "web_fetch"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn process_file_arguments_expands_at_text_with_bom_strip_and_at_images() {
        let temp_dir = make_temp_dir("mehh_file_args");

        let notes_path = temp_dir.join("notes.txt");
        let bom_notes = "\u{feff}notes content".to_string();
        std::fs::write(&notes_path, bom_notes.as_bytes()).unwrap();

        let img_path = temp_dir.join("sample.png");
        let fake_png_bytes = b"\x89PNG\r\n\x1a\nfakeimagebytes";
        std::fs::write(&img_path, fake_png_bytes).unwrap();

        let args = vec![
            "summarize".to_string(),
            "@notes.txt".to_string(),
            "with".to_string(),
            "@sample.png".to_string(),
        ];

        let processed = process_file_arguments(&temp_dir, &args).unwrap();
        assert!(processed.text.contains("summarize"));
        assert!(processed.text.contains("<file name=\"notes.txt\">"));
        assert!(processed.text.contains("notes content"));
        assert!(processed.text.contains("</file>"));
        assert!(processed.text.contains("with"));
        assert!(!processed.text.contains('\u{feff}'));

        let img_block = processed.blocks.iter().find(|b| matches!(b, mehh_ai::UserContentBlock::Image(_)));
        assert!(img_block.is_some());
        if let Some(mehh_ai::UserContentBlock::Image(img)) = img_block {
            assert_eq!(img.mime_type, "image/png");
            assert_eq!(img.data, mehh_coding_agent::base64_encode(fake_png_bytes));
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cli_rejects_conflicting_session_and_feature_flags() {
        let args_no_sess_cont = CliArgs::try_parse_from(["mehh", "--no-session", "-c"]).unwrap();
        assert!(args_no_sess_cont.validate().is_err());

        let args_no_sess_resume = CliArgs::try_parse_from(["mehh", "--no-session", "-r"]).unwrap();
        assert!(args_no_sess_resume.validate().is_err());

        let args_no_sess_sess = CliArgs::try_parse_from(["mehh", "--no-session", "--session", "foo.jsonl"]).unwrap();
        assert!(args_no_sess_sess.validate().is_err());

        let args_cont_sess = CliArgs::try_parse_from(["mehh", "-c", "--session", "foo.jsonl"]).unwrap();
        assert!(args_cont_sess.validate().is_err());

        let args_skills = CliArgs::try_parse_from(["mehh", "--skills", "--no-skills"]).unwrap();
        assert!(args_skills.validate().is_err());

        let args_prompt_tpl = CliArgs::try_parse_from(["mehh", "--prompt-templates", "--no-prompt-templates"]).unwrap();
        assert!(args_prompt_tpl.validate().is_err());

        let args_theme = CliArgs::try_parse_from(["mehh", "--use-theme", "dark", "--no-themes"]).unwrap();
        assert!(args_theme.validate().is_err());
    }

    #[test]
    fn resolve_resume_path_rejects_directories() {
        let temp_dir = make_temp_dir("mehh_resume_dir");
        let sub_dir = temp_dir.join("not_a_session_dir");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let sessions = vec![];
        let res = resolve_resume_path(&sessions, sub_dir.to_str().unwrap());
        assert!(res.is_err(), "resolve_resume_path must not return a directory: {:?}", res);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn resolve_or_custom_model_reasoning_case_insensitive() {
        let m = resolve_or_custom_model(Some("O1-preview"), Some("openai"), None).unwrap();
        assert!(m.reasoning, "O1-preview must have reasoning: true regardless of casing");

        let m_claude = resolve_or_custom_model(Some("Claude-3-7-sonnet"), Some("anthropic"), None).unwrap();
        assert!(m_claude.reasoning, "Claude-3-7-sonnet must have reasoning: true regardless of casing");
    }

    #[test]
    fn configure_cli_subagent_preserves_parent_exclude_tools() {
        let temp_dir = make_temp_dir("mehh_subagent_exclude");
        let args = CliArgs::try_parse_from(["mehh", "--exclude-tools", "bash,write"]).unwrap();
        let cli_tools = resolve_cli_tools(&args, temp_dir.clone()).unwrap();
        let model = faux::faux_model();
        let provider = Arc::new(faux::FauxProvider::new());
        let session_mgr = SessionManager::in_memory(&temp_dir);
        let mut session = AgentSession::new(
            temp_dir.clone(),
            model,
            provider,
            session_mgr,
            Settings::default(),
        );
        session.set_tools(cli_tools);

        configure_cli_subagent(&args, &temp_dir, &mut session).unwrap();

        assert!(
            !session.tools().iter().any(|t| t.name() == "bash" || t.name() == "write"),
            "parent tools must not regain excluded tools when subagent is configured"
        );
        assert!(
            session.tools().iter().any(|t| t.name() == "subagent"),
            "subagent tool must be attached"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn configure_cli_subagent_shares_parent_tool_instances_and_supports_subagent_only() {
        let temp_dir = make_temp_dir("mehh_subagent_share");
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        let cli_tools = resolve_cli_tools(&args, temp_dir.clone()).unwrap();
        let mut session = AgentSession::new(
            temp_dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );
        session.set_tools(cli_tools);

        configure_cli_subagent(&args, &temp_dir, &mut session).unwrap();
        let child_tools = session.subagent_child_tools().unwrap();
        for tool_name in ["web_search", "web_fetch", "undo_edit"] {
            let parent_tool = session.tools().iter().find(|t| t.name() == tool_name).unwrap();
            let child_tool = child_tools.iter().find(|t| t.name() == tool_name).unwrap();
            assert!(
                Arc::ptr_eq(parent_tool, child_tool),
                "subagent must reuse parent's Arc<{tool_name}> (shared WebRuntime/EditUndoStore)"
            );
        }

        // `--tools subagent` still populates built-in tools for child subagents
        let sub_only_args = CliArgs::try_parse_from(["mehh", "--tools", "subagent"]).unwrap();
        let sub_only_parent_tools = resolve_cli_tools(&sub_only_args, temp_dir.clone()).unwrap();
        assert!(sub_only_parent_tools.is_empty());
        let mut sub_only_session = AgentSession::new(
            temp_dir.clone(),
            faux::faux_model(),
            Arc::new(faux::FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );
        sub_only_session.set_tools(sub_only_parent_tools);
        configure_cli_subagent(&sub_only_args, &temp_dir, &mut sub_only_session).unwrap();
        let sub_only_children = sub_only_session.subagent_child_tools().unwrap();
        assert!(sub_only_children.iter().any(|t| t.name() == "read"));
        assert!(sub_only_children.iter().any(|t| t.name() == "web_search"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn i24_provider_without_model_does_not_inherit_mismatched_default_model() {
        let settings = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-sonnet-4-5".to_string()),
            ..Settings::default()
        };

        // Explicit --provider openai without --model must NOT inherit anthropic's default_model
        let (model_arg, prov_arg) =
            select_cli_model_and_provider(None, Some("openai"), &settings);
        assert_eq!(model_arg, None);
        assert_eq!(prov_arg, Some("openai"));

        // Matching --provider (case-insensitive) preserves settings.default_model
        let (model_same, prov_same) =
            select_cli_model_and_provider(None, Some("Anthropic"), &settings);
        assert_eq!(model_same, Some("claude-sonnet-4-5"));
        assert_eq!(prov_same, Some("Anthropic"));

        // Neither flag specified uses both defaults
        let (model_def, prov_def) = select_cli_model_and_provider(None, None, &settings);
        assert_eq!(model_def, Some("claude-sonnet-4-5"));
        assert_eq!(prov_def, Some("anthropic"));
    }

    #[test]
    fn i25_session_id_creates_session_with_exact_header_id_and_preserves_name() {
        let _lock = crate::bootstrap::tests::ENV_LOCK.blocking_lock();
        let env = crate::bootstrap::tests::TempEnv::new("cli_session_id", None);
        let dir = &env.cwd;
        let sess_dir = dir.join("sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();

        let args = CliArgs::try_parse_from([
            "mehh",
            "--session-id",
            "custom-run-42",
            "--name",
            "Human Label",
            "--session-dir",
            sess_dir.to_str().unwrap(),
        ])
        .unwrap();
        let sm = resolve_session_manager(&args, dir).unwrap();
        assert_eq!(sm.session_id(), "custom-run-42");
        assert_eq!(sm.session_name().as_deref(), Some("Human Label"));

        // Without --session-dir, --session-id still creates a file in session_dir_for_cwd(cwd)
        // with header.id == "custom-run-standalone" so list_sessions / -r resolves it.
        let cwd_dir = dir.join("project_cwd");
        std::fs::create_dir_all(&cwd_dir).unwrap();
        let args_standalone = CliArgs::try_parse_from([
            "mehh",
            "--session-id",
            "custom-run-standalone",
            "--name",
            "Standalone Name",
        ])
        .unwrap();
        let sm_standalone = resolve_session_manager(&args_standalone, &cwd_dir).unwrap();
        let created_path = sm_standalone
            .file_path()
            .expect("standalone --session-id must be file-backed")
            .to_path_buf();
        assert_eq!(sm_standalone.session_id(), "custom-run-standalone");
        assert_eq!(sm_standalone.session_name().as_deref(), Some("Standalone Name"));
        let found = list_sessions(&cwd_dir).unwrap();
        assert!(
            found.iter().any(|s| s.id == "custom-run-standalone"),
            "list_sessions(cwd) must find standalone --session-id session: {found:?}"
        );
        let _ = std::fs::remove_file(created_path);
    }
}

