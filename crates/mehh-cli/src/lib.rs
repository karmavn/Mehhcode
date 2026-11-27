//! Shared helpers for the `mehh` CLI.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;
use mehh_ai::{TextContent, UserContent, UserContentBlock};
use mehh_coding_agent::trust::TrustOverride;
use mehh_coding_agent::ProcessedPromptArgs;
use tokio::time::{timeout, timeout_at, Instant};

pub const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;
const STDIN_READ_CHUNK_BYTES: usize = 64 * 1024;
const SUPPLEMENTARY_READ_BUDGET: Duration = Duration::from_secs(5);
const FIRST_READ_IDLE_WINDOW: Duration = Duration::from_millis(500);

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

fn validate_session_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        anyhow::bail!("--session-id cannot be empty");
    }
    if id.chars().any(|c| c.is_whitespace()) {
        anyhow::bail!("--session-id cannot contain whitespace");
    }
    if id.chars().any(|c| c.is_control()) {
        anyhow::bail!("--session-id cannot contain control characters");
    }
    if id.contains('/') || id.contains('\\') || id.contains(':') {
        anyhow::bail!("--session-id cannot contain path separators or ':'");
    }
    if id == "." || id.contains("..") {
        anyhow::bail!("--session-id cannot be '.' or contain '..'");
    }
    Ok(())
}

impl CliArgs {
    pub fn validate(&self) -> anyhow::Result<()> {
        if (self.approve || self.trust) && self.no_approve {
            anyhow::bail!("Cannot specify both --approve/--trust and --no-approve");
        }
        if self.login.is_some() && self.logout.is_some() {
            anyhow::bail!("Cannot specify both --login and --logout");
        }
        let cli_mode = parse_cli_mode(&self.mode)?;
        if self.print && matches!(cli_mode, CliMode::Rpc | CliMode::Interactive) {
            anyhow::bail!(
                "Cannot specify -p/--print with --mode {}",
                self.mode.trim()
            );
        }
        if cli_mode == CliMode::Rpc && !self.prompt.is_empty() {
            anyhow::bail!("Cannot pass prompt arguments with --mode rpc");
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
        if self.resume.is_some() && self.session.is_some() {
            anyhow::bail!("Cannot specify both -r/--resume and --session");
        }
        if self.fork.is_some() && self.no_session {
            anyhow::bail!("Cannot specify --fork with --no-session");
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
        if let Some(ref id) = self.session_id {
            if self.no_session
                || self.session.is_some()
                || self.continue_session
                || self.resume.is_some()
            {
                anyhow::bail!(
                    "--session-id cannot be combined with --no-session, --session, -c/--continue, or -r/--resume"
                );
            }
            validate_session_id(id)?;
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
    stdout_is_terminal: bool,
) -> bool {
    stdout_is_terminal
        && (cli_mode == CliMode::Interactive
            || (cli_mode == CliMode::Text
                && !args.print
                && args.prompt.is_empty()
                && stdin_is_terminal))
}

fn spawn_stdin_reader<R>(
    mut reader: R,
) -> anyhow::Result<tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(16);
    // Run blocking reads on a detached OS thread rather than tokio::io::stdin()
    // so an open/stalled pipe never blocks Tokio runtime shutdown.
    std::thread::Builder::new()
        .name("mehh-stdin-reader".to_string())
        .spawn(move || {
            let mut buf = vec![0u8; STDIN_READ_CHUNK_BYTES];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        })
        .map_err(|err| anyhow::anyhow!("Failed to spawn stdin reader thread: {err}"))?;
    Ok(rx)
}

fn append_capped_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> anyhow::Result<()> {
    if bytes.len().saturating_add(chunk.len()) > max_bytes {
        anyhow::bail!(
            "Piped stdin exceeded maximum allowed size of {} bytes",
            max_bytes
        );
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

/// Reads piped input from a reader using a detached OS thread and a byte cap.
///
/// When `supplementary` is true, stdin is only an addition to a prompt argument,
/// so an idle pipe must not block the run forever. When false, stdin is the
/// prompt itself and the reader is drained until EOF or `max_bytes`.
pub async fn read_piped_stdin_from_with_limit<R>(
    reader: R,
    supplementary: bool,
    max_bytes: usize,
) -> anyhow::Result<String>
where
    R: Read + Send + 'static,
{
    let mut rx = spawn_stdin_reader(reader)?;
    let mut bytes = Vec::new();

    if supplementary {
        // A short idle window on the *first* read only decides whether anything
        // is piped at all. Later chunks get one overall budget, so a producer
        // that pauses between writes is not cut off.
        let first = match timeout(FIRST_READ_IDLE_WINDOW, rx.recv()).await {
            Ok(Some(res)) => res?,
            Ok(None) | Err(_) => return Ok(String::new()),
        };
        if first.is_empty() {
            return Ok(String::new());
        }
        append_capped_chunk(&mut bytes, &first, max_bytes)?;

        let deadline = Instant::now() + SUPPLEMENTARY_READ_BUDGET;
        while let Ok(Some(res)) = timeout_at(deadline, rx.recv()).await {
            let chunk = res?;
            append_capped_chunk(&mut bytes, &chunk, max_bytes)?;
        }
    } else {
        while let Some(res) = rx.recv().await {
            let chunk = res?;
            append_capped_chunk(&mut bytes, &chunk, max_bytes)?;
        }
    }

    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(err) => Ok(String::from_utf8_lossy(err.as_bytes()).into_owned()),
    }
}

/// Reads piped input from an arbitrary reader capped at [`MAX_STDIN_BYTES`] (16 MiB).
pub async fn read_piped_stdin_from<R>(reader: R, supplementary: bool) -> anyhow::Result<String>
where
    R: Read + Send + 'static,
{
    read_piped_stdin_from_with_limit(reader, supplementary, MAX_STDIN_BYTES).await
}

/// Reads piped stdin with the policy implied by the prompt arguments.
/// An explicit prompt argument makes stdin supplementary (bounded read);
/// with no argument, stdin is the prompt itself and is drained to EOF.
pub async fn read_piped_stdin_for_prompt<R>(
    reader: R,
    prompt_args: &[String],
) -> anyhow::Result<String>
where
    R: Read + Send + 'static,
{
    read_piped_stdin_from(reader, !prompt_args.is_empty()).await
}

/// Merges piped stdin into the prompt argument and builds the final content.
/// `process_file_arguments` mirrors the prompt text into the first Text block,
/// so that block must be updated in place; otherwise piped input is lost.
pub fn build_print_user_content(
    processed: ProcessedPromptArgs,
    piped: &str,
) -> anyhow::Result<UserContent> {
    let mut prompt_text = processed.text;
    let mut blocks = processed.blocks;

    let piped = piped.trim_start_matches('\u{feff}');
    if !piped.trim().is_empty() {
        if !prompt_text.trim().is_empty() {
            prompt_text.push_str("\n\n");
        }
        prompt_text.push_str(piped);
    }

    if prompt_text.trim().is_empty() && blocks.is_empty() {
        anyhow::bail!(
            "Prompt is required. Pass a prompt argument, pipe input via stdin, or use --mode rpc."
        );
    }

    if blocks.is_empty() {
        return Ok(UserContent::Text(prompt_text));
    }

    if !prompt_text.trim().is_empty() {
        match blocks
            .iter_mut()
            .find(|b| matches!(b, UserContentBlock::Text(_)))
        {
            Some(UserContentBlock::Text(text)) => text.text = prompt_text,
            _ => blocks.insert(
                0,
                UserContentBlock::Text(TextContent {
                    text: prompt_text,
                    text_signature: None,
                }),
            ),
        }
    }

    Ok(UserContent::Blocks(blocks))
}

pub fn parse_faux_script_json(raw: &str) -> anyhow::Result<Vec<mehh_ai::AssistantMessage>> {
    let val: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("Invalid MEHH_FAUX_SCRIPT JSON: {e}"))?;
    let turns = val
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("MEHH_FAUX_SCRIPT must be a JSON array of turn objects"))?;
    if turns.is_empty() {
        anyhow::bail!("MEHH_FAUX_SCRIPT must contain at least one turn");
    }

    let mut out = Vec::with_capacity(turns.len());
    for (idx, turn) in turns.iter().enumerate() {
        let obj = turn
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("MEHH_FAUX_SCRIPT turn {idx} must be a JSON object"))?;
        let mut content = Vec::new();
        if let Some(text_val) = obj.get("text") {
            let text = text_val.as_str().ok_or_else(|| {
                anyhow::anyhow!("MEHH_FAUX_SCRIPT turn {idx} 'text' must be a string")
            })?;
            if !text.is_empty() {
                content.push(mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                    text: text.to_string(),
                    text_signature: None,
                }));
            }
        }

        let mut has_tool_calls = false;
        if let Some(calls_val) = obj.get("tool_calls") {
            let calls = calls_val.as_array().ok_or_else(|| {
                anyhow::anyhow!("MEHH_FAUX_SCRIPT turn {idx} 'tool_calls' must be an array")
            })?;
            for (c_idx, call) in calls.iter().enumerate() {
                let c_obj = call.as_object().ok_or_else(|| {
                    anyhow::anyhow!(
                        "MEHH_FAUX_SCRIPT turn {idx} tool_call {c_idx} must be an object"
                    )
                })?;
                let id = c_obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "MEHH_FAUX_SCRIPT turn {idx} tool_call {c_idx} requires non-empty string 'id'"
                        )
                    })?;
                let name = c_obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "MEHH_FAUX_SCRIPT turn {idx} tool_call {c_idx} requires non-empty string 'name'"
                        )
                    })?;
                let arguments = match c_obj.get("arguments") {
                    Some(v) => v.as_object().cloned().ok_or_else(|| {
                        anyhow::anyhow!(
                            "MEHH_FAUX_SCRIPT turn {idx} tool_call {c_idx} 'arguments' must be a JSON object"
                        )
                    })?,
                    None => serde_json::Map::new(),
                };
                content.push(mehh_ai::AssistantContentBlock::ToolCall(mehh_ai::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                    arguments_error: None,
                    thought_signature: None,
                }));
                has_tool_calls = true;
            }
        }

        if content.is_empty() {
            anyhow::bail!(
                "MEHH_FAUX_SCRIPT turn {idx} must include non-empty 'text' or at least one 'tool_calls' entry"
            );
        }

        out.push(mehh_ai::AssistantMessage {
            content,
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-1".to_string(),
            response_id: Some(format!("faux-script-{idx}")),
            usage: mehh_ai::Usage::default(),
            stop_reason: if has_tool_calls {
                mehh_ai::StopReason::ToolUse
            } else {
                mehh_ai::StopReason::Stop
            },
            error_message: None,
            timestamp: mehh_ai::now_ms(),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    struct ChannelReader {
        rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
        buf: Vec<u8>,
        pos: usize,
    }

    impl ChannelReader {
        fn pair() -> (std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>, Self) {
            let (tx, rx) = std::sync::mpsc::channel();
            (
                tx,
                Self {
                    rx,
                    buf: Vec::new(),
                    pos: 0,
                },
            )
        }
    }

    impl std::io::Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.pos >= self.buf.len() {
                match self.rx.recv() {
                    Ok(Ok(chunk)) => {
                        self.buf = chunk;
                        self.pos = 0;
                    }
                    Ok(Err(err)) => return Err(err),
                    Err(_) => return Ok(0),
                }
            }
            let available = &self.buf[self.pos..];
            let n = available.len().min(out.len());
            out[..n].copy_from_slice(&available[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[tokio::test]
    async fn supplementary_read_keeps_chunk_arriving_after_idle_gap() {
        let (tx, reader) = ChannelReader::pair();
        let producer = tokio::spawn(async move {
            tx.send(Ok(b"A\n".to_vec())).unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = tx.send(Ok(b"B\n".to_vec()));
        });

        let output = read_piped_stdin_from(reader, true).await.unwrap();
        producer.await.unwrap();

        assert_eq!(output, "A\nB\n");
    }

    #[tokio::test]
    async fn open_idle_pipe_does_not_block_when_prompt_argument_present() {
        // Keep the writer alive: EOF never arrives. With a prompt argument,
        // stdin is supplementary and must stop after the first idle window.
        let (_tx, reader) = ChannelReader::pair();
        let outcome = timeout(
            Duration::from_millis(900),
            read_piped_stdin_for_prompt(reader, &["hello".to_string()]),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Ok(ref text)) if text.is_empty()),
            "stdin must not be drained to EOF when a prompt argument exists: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn slow_first_byte_is_waited_for_when_stdin_is_the_prompt() {
        let (tx, reader) = ChannelReader::pair();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(650)).await;
            tx.send(Ok(b"review this".to_vec())).unwrap();
        });

        let output = read_piped_stdin_for_prompt(reader, &[]).await.unwrap();
        producer.await.unwrap();

        assert_eq!(
            output, "review this",
            "stdin is the prompt, so a slow first byte must not hit the idle window"
        );
    }

    #[tokio::test]
    async fn stdin_reader_enforces_16_mib_cap() {
        assert_eq!(MAX_STDIN_BYTES, 16 * 1024 * 1024);

        let exact = std::io::Cursor::new(vec![b'a'; MAX_STDIN_BYTES]);
        let output = read_piped_stdin_from(exact, false).await.unwrap();
        assert_eq!(output.len(), MAX_STDIN_BYTES);

        let over_prompt = std::io::Cursor::new(vec![b'a'; MAX_STDIN_BYTES + 1]);
        let err_prompt = read_piped_stdin_from(over_prompt, false)
            .await
            .unwrap_err();
        assert!(
            err_prompt.to_string().contains("exceeded maximum"),
            "unexpected error for >16 MiB prompt stdin: {err_prompt}"
        );

        let over_supp = std::io::Cursor::new(vec![b'b'; MAX_STDIN_BYTES + 1]);
        let err_supp = read_piped_stdin_from(over_supp, true).await.unwrap_err();
        assert!(
            err_supp.to_string().contains("exceeded maximum"),
            "unexpected error for >16 MiB supplementary stdin: {err_supp}"
        );
    }

    #[tokio::test]
    async fn stdin_reader_propagates_io_errors() {
        let (tx, reader) = ChannelReader::pair();
        tx.send(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "simulated broken pipe",
        )))
        .unwrap();

        let err = read_piped_stdin_from(reader, false).await.unwrap_err();
        assert!(
            err.to_string().contains("simulated broken pipe"),
            "expected IO error propagation, got: {err}"
        );
    }

    #[test]
    fn cli_args_validate_rejects_invalid_session_id() {
        for bad in [
            "",
            " ",
            "has space",
            "has\ttab",
            "has\nnewline",
            "has\0nul",
            "has\x1bctrl",
            "foo/bar",
            "foo\\bar",
            "C:foo",
            ".",
            "..",
            "../escape",
            "..\\escape",
            "a..b",
        ] {
            let args = CliArgs::try_parse_from(["mehh", "--session-id", bad]).unwrap();
            let res = args.validate();
            assert!(
                res.is_err(),
                "expected --session-id {bad:?} to be rejected, got {res:?}"
            );
        }

        for good in [
            "session-1",
            "550e8400-e29b-41d4-a716-446655440000",
            "alpha_beta.v2",
        ] {
            let args = CliArgs::try_parse_from(["mehh", "--session-id", good]).unwrap();
            assert!(
                args.validate().is_ok(),
                "expected --session-id {good:?} to be valid"
            );
        }
    }

    #[test]
    fn cli_args_validate_rejects_conflicting_flags() {
        let invalid_cases: &[&[&str]] = &[
            &["mehh", "-p", "--mode", "rpc"],
            &["mehh", "--print", "--mode", "interactive"],
            &["mehh", "-p", "--mode", "tui"],
            &["mehh", "--mode", "rpc", "prompt-arg"],
            &["mehh", "--mode", "invalid"],
            &["mehh", "--login", "anthropic", "--logout", "openai"],
            &["mehh", "-r", "--session", "s.jsonl"],
            &["mehh", "-c", "-r"],
            &["mehh", "-c", "--session", "s.jsonl"],
            &["mehh", "--no-session", "-c"],
            &["mehh", "--no-session", "-r"],
            &["mehh", "--no-session", "--session", "s.jsonl"],
            &["mehh", "--no-session", "--fork", "entry-1"],
            &["mehh", "--session-id", "abc", "--no-session"],
            &["mehh", "--session-id", "abc", "--session", "s.jsonl"],
            &["mehh", "--session-id", "abc", "-c"],
            &["mehh", "--session-id", "abc", "-r"],
            &["mehh", "--approve", "--no-approve"],
            &["mehh", "--trust", "--no-approve"],
        ];

        for argv in invalid_cases {
            let args = CliArgs::try_parse_from(*argv).unwrap();
            assert!(
                args.validate().is_err(),
                "expected validation error for {argv:?}"
            );
        }

        let valid_cases: &[&[&str]] = &[
            &["mehh", "--mode", "json", "-p", "explain code"],
            &["mehh", "--session-id", "abc-123", "--session-dir", "sessions"],
            &["mehh", "--mode", "rpc"],
        ];
        for argv in valid_cases {
            let args = CliArgs::try_parse_from(*argv).unwrap();
            assert!(
                args.validate().is_ok(),
                "expected validation ok for {argv:?}"
            );
        }
    }

    #[test]
    fn should_run_interactive_checks_stdout_and_stdin_terminals() {
        let default_args = CliArgs::try_parse_from(["mehh"]).unwrap();
        assert!(should_run_interactive(&default_args, CliMode::Text, true, true));
        assert!(!should_run_interactive(
            &default_args,
            CliMode::Text,
            true,
            false
        ));
        assert!(!should_run_interactive(
            &default_args,
            CliMode::Text,
            false,
            true
        ));
        assert!(!should_run_interactive(
            &default_args,
            CliMode::Text,
            false,
            false
        ));

        let prompt_args = CliArgs::try_parse_from(["mehh", "explain"]).unwrap();
        assert!(!should_run_interactive(
            &prompt_args,
            CliMode::Text,
            true,
            true
        ));

        let print_args = CliArgs::try_parse_from(["mehh", "-p"]).unwrap();
        assert!(!should_run_interactive(&print_args, CliMode::Text, true, true));

        let interactive_args =
            CliArgs::try_parse_from(["mehh", "--mode", "interactive"]).unwrap();
        assert!(should_run_interactive(
            &interactive_args,
            CliMode::Interactive,
            true,
            true
        ));
        assert!(should_run_interactive(
            &interactive_args,
            CliMode::Interactive,
            false,
            true
        ));
        assert!(!should_run_interactive(
            &interactive_args,
            CliMode::Interactive,
            true,
            false
        ));

        let rpc_args = CliArgs::try_parse_from(["mehh", "--mode", "rpc"]).unwrap();
        assert!(!should_run_interactive(&rpc_args, CliMode::Rpc, true, true));
    }

    #[test]
    fn build_print_user_content_merges_piped_stdin_with_prompt_and_at_file() {
        let dir = std::env::temp_dir().join(format!(
            "mehh_merge_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "FILE_BODY").unwrap();

        let processed = mehh_coding_agent::process_file_arguments(
            &dir,
            &["review".to_string(), "@a.txt".to_string()],
        )
        .unwrap();
        let content = build_print_user_content(processed, "\u{feff}DIFF_BODY").unwrap();

        let blocks = match content {
            mehh_ai::UserContent::Blocks(blocks) => blocks,
            other => panic!("expected blocks, got {other:?}"),
        };
        let text = blocks
            .iter()
            .find_map(|b| match b {
                mehh_ai::UserContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .expect("a text block must exist");

        assert!(text.contains("review"), "prompt argument lost: {text}");
        assert!(text.contains("FILE_BODY"), "@file content lost: {text}");
        assert!(
            text.contains("DIFF_BODY"),
            "piped stdin lost when a prompt argument is present: {text}"
        );
        assert!(!text.contains('\u{feff}'), "BOM must be stripped: {text}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_faux_script_accepts_text_and_tool_calls() {
        let raw = r#"[
            {"tool_calls": [{"id": "call_1", "name": "write", "arguments": {"path": "a.txt", "content": "hi"}}]},
            {"text": "all done"}
        ]"#;
        let msgs = parse_faux_script_json(raw).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].stop_reason, mehh_ai::StopReason::ToolUse);
        assert_eq!(msgs[0].tool_calls().len(), 1);
        assert_eq!(msgs[0].tool_calls()[0].name, "write");
        assert_eq!(msgs[1].stop_reason, mehh_ai::StopReason::Stop);
        assert_eq!(msgs[1].text_content(), "all done");
    }

    #[test]
    fn parse_faux_script_rejects_malformed_or_empty_inputs() {
        assert!(parse_faux_script_json("not json").is_err());
        assert!(parse_faux_script_json("[]").is_err());
        assert!(parse_faux_script_json("[{}]").is_err());
        assert!(parse_faux_script_json(r#"[{"tool_calls": [{"id": "1", "name": "", "arguments": {}}]}]"#).is_err());
        assert!(parse_faux_script_json(r#"[{"tool_calls": [{"id": "1", "name": "write", "arguments": "bad"}]}]"#).is_err());
    }
}
