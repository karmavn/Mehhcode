use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use clap::Parser;
use mehh_ai::{
    catalog, faux, LlmProvider, Model, ModelCost, ThinkingLevel,
};
use mehh_coding_agent::auth::resolve_api_key_async;
use mehh_coding_agent::models_config::ModelRegistry;
use mehh_coding_agent::trust::{resolve_project_trust_with_override, TrustOverride};
use mehh_coding_agent::{
    process_file_arguments, ProcessedPromptArgs, list_sessions, perform_login, perform_logout, rpc, run_interactive_mode, run_print_mode_content,
    AgentSession, McpManager, OutputMode, SessionManager, SessionSummary, Settings,
    SettingsManager,
};

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

    #[arg(long = "permission-mode", value_name = "MODE")]
    pub permission_mode: Option<String>,

    #[arg(long = "worker-model", value_name = "MODEL")]
    pub worker_model: Option<String>,

    #[arg(long = "lead-turns", value_name = "TURNS")]
    pub lead_turns: Option<usize>,

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

    #[arg(long = "trust")]
    pub trust: bool,

    #[arg(long = "name", value_name = "NAME")]
    pub name: Option<String>,

    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,

    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    #[arg(long = "fork", value_name = "ENTRY_ID")]
    pub fork: Option<String>,

    #[arg(long = "models", value_name = "MODELS")]
    pub models: Option<String>,

    #[arg(long = "tools", value_name = "TOOLS")]
    pub tools: Option<String>,

    #[arg(long = "exclude-tools", value_name = "TOOLS")]
    pub exclude_tools: Option<String>,

    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,

    #[arg(long = "export", value_name = "SESSION_FILE")]
    pub export: Option<PathBuf>,

    #[arg(long = "skills")]
    pub skills: bool,

    #[arg(long = "no-skills")]
    pub no_skills: bool,

    #[arg(long = "prompt-templates")]
    pub prompt_templates: bool,

    #[arg(long = "no-prompt-templates")]
    pub no_prompt_templates: bool,

    #[arg(long = "use-theme", value_name = "THEME")]
    pub use_theme: Option<String>,

    #[arg(long = "no-themes")]
    pub no_themes: bool,

    #[arg(long = "no-context-files")]
    pub no_context_files: bool,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[arg(long = "approve")]
    pub approve: bool,

    #[arg(long = "no-approve")]
    pub no_approve: bool,
}

impl CliArgs {
    pub fn validate(&self) -> anyhow::Result<()> {
        if (self.approve || self.trust) && self.no_approve {
            anyhow::bail!("Cannot specify both --approve/--trust and --no-approve");
        }
        if self.no_session
            && (self.continue_session || self.resume.is_some() || self.session.is_some())
        {
            anyhow::bail!(
                "Cannot specify --no-session with session resumption or session file flags"
            );
        }
        if self.continue_session && self.session.is_some() {
            anyhow::bail!("Cannot specify both -c/--continue and --session");
        }
        if self.continue_session && self.resume.is_some() {
            anyhow::bail!("Cannot specify both -c/--continue and -r/--resume");
        }
        if self.session_dir.is_some()
            && (self.no_session
                || self.session.is_some()
                || self.continue_session
                || self.resume.is_some())
        {
            anyhow::bail!(
                "--session-dir cannot be combined with --no-session, --session, -c/--continue, or -r/--resume"
            );
        }
        if self.skills && self.no_skills {
            anyhow::bail!("Cannot specify both --skills and --no-skills");
        }
        if self.prompt_templates && self.no_prompt_templates {
            anyhow::bail!("Cannot specify both --prompt-templates and --no-prompt-templates");
        }
        if self.use_theme.is_some() && self.no_themes {
            anyhow::bail!("Cannot specify both --use-theme and --no-themes");
        }
        Ok(())
    }

    pub fn trust_override(&self) -> TrustOverride {
        if self.no_approve {
            TrustOverride::NoApprove
        } else if self.trust {
            // --approve only controls tool approvals. Letting it also trust the
            // repo would enable project MCP servers, skills and instructions
            // without the user ever making a trust decision.
            TrustOverride::Approve
        } else {
            TrustOverride::None
        }
    }
}

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
    if args.no_tools || args.no_builtin_tools {
        return Ok(Vec::new());
    }
    let include = args.tools.as_deref().map(parse_tool_list);
    let exclude = args
        .exclude_tools
        .as_deref()
        .map(parse_tool_list)
        .unwrap_or_default();
    mehh_tools::select_coding_tools(cwd, include.as_deref(), &exclude)
}


pub fn handle_export(
    input_file: &Path,
    output_file: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<()> {
    mehh_coding_agent::session::export_session(input_file, output_file, cwd)
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
    } else {
        (
            settings.default_model.as_deref(),
            args_provider.or(settings.default_provider.as_deref()),
        )
    }
}

/// Applies `--thinking` only when explicitly provided so `settings.default_thinking_level` is preserved otherwise.
pub fn apply_cli_thinking_override(
    session: &mut AgentSession,
    cli_thinking: Option<ThinkingLevel>,
) {
    if let Some(level) = cli_thinking {
        session.set_thinking_level(Some(level));
    }
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
        let sessions = list_sessions(cwd)?;
        let latest = sessions.first().ok_or_else(|| {
            anyhow::anyhow!("No previous session found for {}", cwd.display())
        })?;
        create_session_manager(false, Some(&latest.path), cwd)?
    } else if let Some(spec) = args.resume.as_ref() {
        let sessions = list_sessions(cwd)?;
        let path = match spec {
            Some(target) => resolve_resume_path(&sessions, target)?,
            None => prompt_session_selection(&sessions)?,
        };
        create_session_manager(false, Some(&path), cwd)?
    } else if let Some(id) = args.session_id.as_deref() {
        // --session-id works on its own, not only together with --session-dir.
        if let Some(dir) = args.session_dir.as_deref() {
            let ts = mehh_ai::now_ms();
            let path = dir.join(format!("{ts}_{id}.jsonl"));
            SessionManager::open_file(path, cwd)?
        } else {
            let mut mgr = create_session_manager(false, None, cwd)?;
            mgr.set_session_name(Some(id.to_string()))?;
            mgr
        }
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let cwd = std::env::current_dir()?;

    if let Some(ref export_input) = args.export {
        let export_output = args.prompt.first().map(Path::new);
        return handle_export(export_input, export_output, &cwd);
    }

    let model_registry = ModelRegistry::load_default()?;
    if args.list_models {
        for m in model_registry.models() {
            println!(
                "{}/{} - {} ({} tokens)",
                m.provider, m.id, m.name, m.context_window
            );
        }
        return Ok(());
    }

    let is_interactive = should_run_interactive(&args, cli_mode, io::stdin().is_terminal());
    let trusted = resolve_project_trust_with_override(&cwd, args.trust_override(), is_interactive)?;
    let mut settings = SettingsManager::try_load_with_trust(&cwd, trusted)?;
    if let Some(ref m) = args.permission_mode {
        let mode = m
            .parse::<mehh_coding_agent::PermissionMode>()
            .map_err(|e| anyhow::anyhow!(e))?;
        settings.permission_mode = Some(mode);
    } else if args.approve && !args.no_approve {
        // Keep --approve useful: it now auto-approves tools without granting
        // trust to project-local configuration.
        settings.permission_mode = Some(mehh_coding_agent::PermissionMode::Auto);
    }
    if let Some(ref wm) = args.worker_model {
        settings.worker_model = Some(wm.clone());
    }
    if let Some(lt) = args.lead_turns {
        settings.lead_turns = Some(lt);
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
            let (model_arg, provider_arg) = select_cli_model_and_provider(
                args.model.as_deref(),
                args.provider.as_deref(),
                &settings,
            );

            let model = resolve_or_custom_model_with_registry(
                &model_registry,
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

    let mut agent_session = AgentSession::new_with_trust(
        cwd.clone(),
        model.clone(),
        provider.clone(),
        session_mgr,
        settings,
        trusted,
    );

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

    // Subagent tools are built after MCP connection so child agents see MCP tools.
    let subagent_models = if args.no_tools || args.no_builtin_tools {
        None
    } else {
        let enabled = match args.tools.as_deref() {
            Some(inc) => inc.split(',').any(|s| s.trim() == "subagent"),
            None => !args
                .exclude_tools
                .as_deref()
                .unwrap_or("")
                .split(',')
                .any(|s| s.trim() == "subagent"),
        };
        if enabled {
            let lw = agent_session.lead_worker.lock().unwrap();
            Some((
                lw.worker_model().cloned().unwrap_or_else(|| model.clone()),
                lw.worker_provider().cloned().unwrap_or_else(|| provider.clone()),
            ))
        } else {
            None
        }
    };
    if args.no_tools || args.no_builtin_tools || args.tools.is_some() || args.exclude_tools.is_some() {
        let mut cli_tools = resolve_cli_tools(&args, cwd.clone())?;
        let should_include_todo = if args.no_tools || args.no_builtin_tools {
            false
        } else if let Some(ref inc) = args.tools {
            inc.split(',').any(|s| s.trim() == "todo")
        } else {
            !args
                .exclude_tools
                .as_deref()
                .unwrap_or("")
                .split(',')
                .any(|s| s.trim() == "todo")
        };
        if should_include_todo {
            cli_tools.push(Arc::new(mehh_coding_agent::todo::TodoTool::new(agent_session.todo_state.clone())));
        }

        agent_session.set_tools(cli_tools);
    }

    if let Some(ref models_str) = args.models {
        let mut resolved = Vec::new();
        for token in models_str.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
            // IDs may contain multiple slashes (e.g. openrouter/anthropic/claude-sonnet-4.5).
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

    if args.no_skills {
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

    // Resolve the key asynchronously so expired OAuth tokens are refreshed first.
    let has_explicit_api_key = args
        .api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    // Key refresh (which may perform HTTP) and MCP startup are independent.
    // join! (not try_join!) so a failing key refresh cannot drop an MCP
    // connect future that may already have spawned child processes.
    let (api_key_resolved, mcp_resolved) = tokio::join!(
        resolve_api_key_async(&model.provider, args.api_key.as_deref(), None::<&Path>),
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
            // join! already let MCP finish connecting; close its child processes
            // before propagating the credential error.
            if let Some(ref mgr) = mcp_manager {
                mgr.close_all().await;
            }
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

    let mut mcp_manager = mcp_manager;
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

    if !args.no_tools {
        agent_session.enable_mcp_and_codemode(mcp_tools, args.codemode);
    }

    if let Some((sub_model, sub_provider)) = subagent_models {
        // Rebuild the subagent tool with a usable tool set. The --tools include
        // list filters out "subagent" itself, so with `--tools subagent` the
        // parent has no builtin tools at all; children must not inherit that.
        let mut tools: Vec<Arc<dyn mehh_agent::AgentTool>> = Vec::new();
        if !args.no_tools && !args.no_builtin_tools {
            tools.extend(mehh_tools::select_coding_tools(cwd.clone(), None, &[]));
        }
        let mut seen: std::collections::HashSet<String> =
            tools.iter().map(|t| t.name().to_string()).collect();
        for tool in agent_session.tools.iter().filter(|t| t.name() != "subagent") {
            if seen.insert(tool.name().to_string()) {
                tools.push(tool.clone());
            }
        }
        let subagent = Arc::new(
            mehh_coding_agent::subagent::SubagentTool::new(
                cwd.clone(),
                sub_model,
                sub_provider,
                None,
                tools.clone(),
                0,
            )
            .with_inspection_manager(agent_session.inspection_manager.clone())
            .with_approval_handler(agent_session.approval_handler.clone()),
        );
        tools.push(subagent);
        agent_session.set_tools(tools);
    }

    agent_session.set_custom_system_prompt(
        args.system_prompt.clone(),
        args.append_system_prompt.clone(),
    );

    if is_interactive {
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
        piped = mehh_cli::read_piped_stdin_for_prompt(
            tokio::io::BufReader::new(tokio::io::stdin()),
            &args.prompt,
        )
        .await?;
    }
    let user_content = mehh_cli::build_print_user_content(processed, &piped)?;

    run_print_mode_content(agent_session, user_content, output_mode).await?;
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
    fn cli_rejects_conflicting_approve_and_no_approve() {
        let args1 = CliArgs::try_parse_from(["mehh", "--approve", "--no-approve"]).unwrap();
        assert!(args1.validate().is_err());

        let args2 = CliArgs::try_parse_from(["mehh", "--trust", "--no-approve"]).unwrap();
        assert!(args2.validate().is_err());

        let args3 = CliArgs::try_parse_from(["mehh", "--approve"]).unwrap();
        assert!(args3.validate().is_ok());
        assert_eq!(args3.trust_override(), mehh_coding_agent::TrustOverride::Approve);

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

        // 1. Default tools (9 tools with analyze and undo_edit, powershell is opt-in)
        let args_def = CliArgs::try_parse_from(["mehh"]).unwrap();
        let tools_def = resolve_cli_tools(&args_def, temp_dir.clone()).unwrap();
        assert_eq!(tools_def.len(), 9);
        assert!(!tools_def.iter().any(|t| t.name() == "powershell"));

        // 2. Opt-in powershell via --tools
        let args_ps = CliArgs::try_parse_from(["mehh", "--tools", "powershell"]).unwrap();
        let tools_ps = resolve_cli_tools(&args_ps, temp_dir.clone()).unwrap();
        assert_eq!(tools_ps.len(), 1);
        assert_eq!(tools_ps[0].name(), "powershell");

        // 3. Exclude tools
        let args_ex = CliArgs::try_parse_from(["mehh", "--exclude-tools", "bash,edit"]).unwrap();
        let tools_ex = resolve_cli_tools(&args_ex, temp_dir.clone()).unwrap();
        assert_eq!(tools_ex.len(), 7);
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
        assert_eq!(tools.len(), 9);

        // Unknown names must still be rejected.
        let bogus = CliArgs::try_parse_from(["mehh", "--tools", "bogus"]).unwrap();
        assert!(resolve_cli_tools(&bogus, temp_dir.clone()).is_err());

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
}

