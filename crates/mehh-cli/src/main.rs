use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use clap::Parser;
use mehh_agent::AgentTool;
use mehh_ai::{catalog, faux, LlmProvider, Model, ModelCost, ThinkingLevel};
use mehh_coding_agent::{
    build_system_prompt_with_skills, list_sessions, perform_login, perform_logout, rpc,
    run_interactive_mode, run_print_mode, AgentSession, McpManager, OutputMode, SessionManager,
    SessionSummary, SettingsManager,
};
use mehh_coding_agent::auth::resolve_api_key_async;

#[derive(clap::Parser, Debug, Clone, PartialEq, Eq)]
#[command(name = "mehh", version, about = "mehh coding agent CLI")]
pub struct CliArgs {
    pub prompt: Vec<String>,

    #[arg(short = 'p', long = "print")]
    pub print: bool,

    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    #[arg(
        short = 'r',
        long = "resume",
        num_args = 0..=1,
        require_equals = true,
        value_name = "SESSION"
    )]
    pub resume: Option<Option<String>>,

    #[arg(long = "login", value_name = "PROVIDER")]
    pub login: Option<String>,

    #[arg(long = "logout", value_name = "PROVIDER")]
    pub logout: Option<String>,

    #[arg(long = "mode", default_value = "text")]
    pub mode: String,

    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    #[arg(long = "provider")]
    pub provider: Option<String>,

    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    #[arg(long = "thinking")]
    pub thinking: Option<String>,

    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    #[arg(long = "append-system-prompt")]
    pub append_system_prompt: Option<String>,

    #[arg(long = "session")]
    pub session: Option<PathBuf>,

    #[arg(long = "no-session")]
    pub no_session: bool,

    #[arg(long = "no-tools")]
    pub no_tools: bool,

    #[arg(long = "no-mcp")]
    pub no_mcp: bool,

    #[arg(long = "codemode")]
    pub codemode: bool,

    #[arg(long = "list-models")]
    pub list_models: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Text,
    Json,
    Rpc,
    Interactive,
}

pub fn parse_cli_mode(s: &str) -> anyhow::Result<CliMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(CliMode::Text),
        "json" => Ok(CliMode::Json),
        "rpc" => Ok(CliMode::Rpc),
        "interactive" | "tui" => Ok(CliMode::Interactive),
        other => anyhow::bail!(
            "Invalid mode '{}'. Allowed: text, json, rpc, interactive",
            other
        ),
    }
}

pub fn should_run_interactive(
    args: &CliArgs,
    cli_mode: CliMode,
    stdin_is_terminal: bool,
) -> bool {
    cli_mode == CliMode::Interactive
        || (cli_mode == CliMode::Text && !args.print && args.prompt.is_empty() && stdin_is_terminal)
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

pub fn parse_output_mode(s: &str) -> anyhow::Result<OutputMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(OutputMode::Text),
        "json" => Ok(OutputMode::Json),
        other => anyhow::bail!("Invalid output mode '{}'. Allowed: text, json", other),
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
        _ => "https://api.openai.com/v1".to_string(),
    }
}

/// Resolves a model from the builtin catalog or constructs a fallback custom Model
/// with reasonable defaults for unlisted models.
pub fn resolve_or_custom_model(
    model_arg: Option<&str>,
    provider_arg: Option<&str>,
    default_provider: Option<&str>,
) -> Option<Model> {
    if let Some(m) = model_arg {
        let resolved = if let Some(p) = provider_arg {
            catalog::resolve_model(p, Some(m))
        } else {
            catalog::resolve_model(m, None)
        };

        if let Some(model) = resolved {
            return Some(model);
        }

        // Custom / unlisted fallback model
        let (provider, id) = if let Some(p) = provider_arg {
            (p.to_ascii_lowercase(), m.to_string())
        } else if let Some((p, rest)) = m.split_once('/') {
            (p.to_ascii_lowercase(), rest.to_string())
        } else if m.starts_with("claude-") {
            ("anthropic".to_string(), m.to_string())
        } else if m.starts_with("gemini-") {
            ("google".to_string(), m.to_string())
        } else if let Some(dp) = default_provider {
            (dp.to_string(), m.to_string())
        } else {
            ("openai".to_string(), m.to_string())
        };

        let api = api_for_provider(&provider).to_string();
        let base_url = base_url_for_provider(&provider);

        let reasoning = id.starts_with("o1")
            || id.starts_with("o3")
            || id.starts_with("o4")
            || id.contains("claude")
            || id.contains("gemini-2.5");

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
            headers: None,
        })
    } else if let Some(p) = provider_arg {
        catalog::resolve_model(p, None)
    } else if let Some(dp) = default_provider {
        catalog::resolve_model(dp, None)
    } else {
        catalog::resolve_model("claude-sonnet-4-5", None)
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
    if candidate.exists() {
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

/// Builds the system prompt for a session from its final tool list.
///
/// The prompt is assembled after the MCP and codemode tools are installed so that
/// a custom prompt or an addendum is never dropped by a later tool sync.
pub fn assemble_system_prompt(
    session: &AgentSession,
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
) -> String {
    let tool_names: Vec<&str> = session.tools().iter().map(|t| t.name()).collect();
    build_system_prompt_with_skills(
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
    if args.no_session {
        return create_session_manager(true, None, cwd);
    }
    if let Some(path) = args.session.as_deref() {
        return create_session_manager(false, Some(path), cwd);
    }
    if args.continue_session {
        let sessions = list_sessions(cwd)?;
        let latest = sessions.first().ok_or_else(|| {
            anyhow::anyhow!("No previous session found for {}", cwd.display())
        })?;
        return create_session_manager(false, Some(&latest.path), cwd);
    }
    if let Some(spec) = args.resume.as_ref() {
        let sessions = list_sessions(cwd)?;
        let path = match spec {
            Some(target) => resolve_resume_path(&sessions, target)?,
            None => prompt_session_selection(&sessions)?,
        };
        return create_session_manager(false, Some(&path), cwd);
    }
    create_session_manager(false, None, cwd)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();

    if args.list_models {
        for m in catalog::builtin_models() {
            println!(
                "{}/{} - {} ({} tokens)",
                m.provider, m.id, m.name, m.context_window
            );
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let settings = SettingsManager::load(&cwd);

    // Credential management short-circuits before a session is created.
    if let Some(provider) = args.login.as_deref() {
        let message = perform_login(provider, args.api_key.as_deref(), None).await?;
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

    let (model, provider): (Model, Arc<dyn LlmProvider>) = {
        let is_faux = args
            .provider
            .as_deref()
            .map(|p| p.eq_ignore_ascii_case("faux"))
            .unwrap_or(false)
            || args
                .model
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("faux") || m.eq_ignore_ascii_case("faux-1"))
                .unwrap_or(false);

        if is_faux {
            let model = faux::faux_model();
            let faux_prov = faux::FauxProvider::new();
            faux_prov.add_response(faux::faux_assistant_message(
                "Hello from mehh Rust faux provider!",
            ));
            (model, Arc::new(faux_prov))
        } else {
            let model_arg = args
                .model
                .as_deref()
                .or(settings.default_model.as_deref());
            let provider_arg = args
                .provider
                .as_deref()
                .or(settings.default_provider.as_deref());

            let model = resolve_or_custom_model(
                model_arg,
                provider_arg,
                settings.default_provider.as_deref(),
            )
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to resolve model (model: {:?}, provider: {:?})",
                    args.model,
                    args.provider
                )
            })?;

            let provider = AgentSession::default_provider_for_model(&model);
            (model, provider)
        }
    };

    let session_mgr = resolve_session_manager(&args, &cwd)?;

    let mut agent_session = AgentSession::new(
        cwd.clone(),
        model.clone(),
        provider,
        session_mgr,
        settings,
    );

    let tools: Vec<Arc<dyn AgentTool>> = if args.no_tools {
        Vec::new()
    } else {
        mehh_tools::create_coding_tools(cwd.clone())
    };

    agent_session.set_tools(tools);

    // Resolve the key asynchronously so expired OAuth tokens are refreshed first.
    let api_key = resolve_api_key_async(&model.provider, args.api_key.as_deref(), None::<&Path>).await?;
    agent_session.set_api_key(api_key);

    let thinking_level = match &args.thinking {
        Some(t) => Some(parse_thinking_level(t)?),
        None => None,
    };
    agent_session.set_thinking_level(thinking_level);

    let cli_mode = parse_cli_mode(&args.mode)?;

    let (mut mcp_manager, mcp_tools) = if !args.no_tools && !args.no_mcp {
        let mgr = McpManager::connect_from_configs(&cwd).await;
        let tools = mgr.tools().to_vec();
        (Some(mgr), tools)
    } else {
        (None, Vec::new())
    };

    if !args.no_tools {
        agent_session.enable_mcp_and_codemode(mcp_tools, args.codemode);
    }

    // The system prompt is built after the final tool list is known so that the
    // MCP/codemode tool sync cannot drop a custom prompt or an addendum.
    let system_prompt = assemble_system_prompt(
        &agent_session,
        &cwd,
        args.system_prompt.as_deref(),
        args.append_system_prompt.as_deref(),
    );
    agent_session.set_system_prompt(system_prompt);

    if should_run_interactive(&args, cli_mode, io::stdin().is_terminal()) {
        let summaries = mcp_manager
            .as_ref()
            .map(|mgr| mgr.summaries().to_vec())
            .unwrap_or_default();
        let res = run_interactive_mode(
            &mut agent_session,
            &summaries,
            mcp_manager.as_mut(),
            args.codemode,
        )
        .await;
        if let Some(mgr) = &mcp_manager {
            let _ = mgr.close_all().await;
        }
        return res;
    }

    let run_res = match cli_mode {
        CliMode::Rpc => {
            rpc::run_rpc_mode(
                &mut agent_session,
                tokio::io::BufReader::new(tokio::io::stdin()),
                tokio::io::stdout(),
            )
            .await
        }
        CliMode::Text | CliMode::Interactive => {
            run_print_flow(&mut agent_session, &args, OutputMode::Text).await
        }
        CliMode::Json => {
            run_print_flow(&mut agent_session, &args, OutputMode::Json).await
        }
    };

    if let Some(mgr) = &mcp_manager {
        let _ = mgr.close_all().await;
    }

    run_res
}

async fn run_print_flow(
    agent_session: &mut AgentSession,
    args: &CliArgs,
    output_mode: OutputMode,
) -> anyhow::Result<()> {
    let mut prompt_text = args.prompt.join(" ");
    if prompt_text.trim().is_empty() && !io::stdin().is_terminal() {
        let mut stdin_input = String::new();
        io::stdin().read_to_string(&mut stdin_input)?;
        prompt_text = stdin_input;
    }

    if prompt_text.trim().is_empty() {
        anyhow::bail!(
            "Prompt is required. Pass a prompt argument, pipe input via stdin, or use --mode rpc."
        );
    }

    let expanded = agent_session.expand_input(&prompt_text);
    run_print_mode(agent_session, &expanded, output_mode).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // When mode is text, no prompt, print is false, and stdin is a terminal -> true
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        assert_eq!(args.mode, "text");
        assert!(args.prompt.is_empty());
        assert!(!args.print);

        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(should_run_interactive(&args, cli_mode, true));
    }

    #[test]
    fn test_should_run_interactive_piped_stdin() {
        // When mode is text and no prompt, but stdin is NOT a terminal (piped) -> false
        let args = CliArgs::try_parse_from(["mehh"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_with_prompt_arg() {
        // When prompt is supplied in text mode, even on TTY -> false
        let args = CliArgs::try_parse_from(["mehh", "do something"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true));
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_print_flag() {
        // When print flag -p is set in text mode, even on TTY with no prompt -> false
        let args = CliArgs::try_parse_from(["mehh", "-p"]).unwrap();
        let cli_mode = parse_cli_mode(&args.mode).unwrap();
        assert!(!should_run_interactive(&args, cli_mode, true));
        assert!(!should_run_interactive(&args, cli_mode, false));
    }

    #[test]
    fn test_should_run_interactive_explicit_interactive_mode() {
        // Explicit interactive mode is always true regardless of TTY, prompt, or print
        let args1 = CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert!(should_run_interactive(&args1, CliMode::Interactive, true));
        assert!(should_run_interactive(&args1, CliMode::Interactive, false));

        let args2 = CliArgs::try_parse_from(["mehh", "--mode", "interactive", "-p", "some prompt"]).unwrap();
        assert!(should_run_interactive(&args2, CliMode::Interactive, true));
        assert!(should_run_interactive(&args2, CliMode::Interactive, false));

        let args3 = CliArgs::try_parse_from(["mehh", "--mode", "tui"]).unwrap();
        let mode3 = parse_cli_mode(&args3.mode).unwrap();
        assert_eq!(mode3, CliMode::Interactive);
        assert!(should_run_interactive(&args3, mode3, true));
        assert!(should_run_interactive(&args3, mode3, false));
    }

    #[test]
    fn test_should_run_interactive_json_and_rpc_modes() {
        // Json mode never runs interactive mode
        let args_json = CliArgs::try_parse_from(["mehh", "--mode", "json"]).unwrap();
        let mode_json = parse_cli_mode(&args_json.mode).unwrap();
        assert_eq!(mode_json, CliMode::Json);
        assert!(!should_run_interactive(&args_json, mode_json, true));
        assert!(!should_run_interactive(&args_json, mode_json, false));

        // Rpc mode never runs interactive mode
        let args_rpc = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        let mode_rpc = parse_cli_mode(&args_rpc.mode).unwrap();
        assert_eq!(mode_rpc, CliMode::Rpc);
        assert!(!should_run_interactive(&args_rpc, mode_rpc, true));
        assert!(!should_run_interactive(&args_rpc, mode_rpc, false));
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
    fn test_parse_output_mode() {
        assert!(matches!(parse_output_mode("text").unwrap(), OutputMode::Text));
        assert!(matches!(parse_output_mode("json").unwrap(), OutputMode::Json));
        assert!(matches!(parse_output_mode("TEXT").unwrap(), OutputMode::Text));
        assert!(matches!(parse_output_mode("JSON").unwrap(), OutputMode::Json));
        assert!(parse_output_mode("yaml").is_err());
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
        assert_eq!(m.base_url, "https://api.anthropic.com");
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
        assert_eq!(m_claude.base_url, "https://api.anthropic.com");
        assert!(m_claude.reasoning);

        // Gemini prefix
        let m_gemini = resolve_or_custom_model(Some("gemini-3.0-pro"), None, None).unwrap();
        assert_eq!(m_gemini.provider, "google");
        assert_eq!(m_gemini.api, "google-generative-ai");
        assert_eq!(m_gemini.base_url, "https://generativelanguage.googleapis.com/v1beta");
        assert!(!m_gemini.reasoning);

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
                "https://api.anthropic.com",
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
        assert_eq!(m.base_url, "https://api.anthropic.com");
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
        let dir = make_temp_dir("mehh_cli_continue");
        let args = CliArgs::try_parse_from(["mehh", "-c"]).unwrap();
        let err = match resolve_session_manager(&args, &dir) {
            Ok(_) => panic!("expected --continue to fail without a previous session"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("No previous session"));
        let _ = std::fs::remove_dir_all(dir);
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
}
