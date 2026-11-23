//! Prompt template discovery, argument parsing, and parameter substitution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::skills::parse_frontmatter;

/// A prompt template loaded from markdown with parameter placeholders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
}

/// Parses shell-style arguments from an input string respecting single and double quotes.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quote: Option<char> = None;
    let chars: Vec<char> = args_string.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if let Some(q) = in_quote {
            if c == '\\' && i + 1 < chars.len() && chars[i + 1] == q {
                current.push(q);
                i += 2;
                continue;
            } else if c == q {
                in_quote = None;
            } else {
                current.push(c);
            }
        } else if (c == '"' || c == '\'') && !has_token {
            in_quote = Some(c);
            has_token = true;
        } else if c.is_whitespace() {
            if has_token {
                args.push(current);
                current = String::new();
                has_token = false;
            }
        } else {
            current.push(c);
            has_token = true;
        }

        if in_quote.is_some() && i == chars.len() - 1 {
            // Unterminated quote: keep what was accumulated as the final token
            // instead of replaying the string from the opening quote.
            break;
        }

        i += 1;
    }

    if has_token {
        args.push(current);
    }

    args
}

/// Substitutes argument placeholders in template content.
///
/// Supports:
/// - `$1`, `$2`, ...: positional arguments
/// - `$@`, `$ARGUMENTS`: all arguments
/// - `${N:-default}`: positional argument with fallback default
/// - `${@:-default}`, `${ARGUMENTS:-default}`: all arguments with fallback default
/// - `${@:N}`: bash-style argument slicing from 1-indexed Nth onward
/// - `${@:N:L}`: slice L arguments starting from 1-indexed Nth
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let mut result = String::with_capacity(content.len());
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                // Bracketed placeholder: ${...}
                if let Some(closing) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let end = i + 2 + closing;
                    let inner: String = chars[i + 2..end].iter().collect();

                    if let Some((target, default_val)) = inner.split_once(":-") {
                        let val = match target {
                            "@" | "ARGUMENTS" => {
                                if all_args.is_empty() {
                                    default_val.to_string()
                                } else {
                                    all_args.clone()
                                }
                            }
                            _ => {
                                if let Ok(n) = target.parse::<usize>() {
                                    if n > 0 && n <= args.len() && !args[n - 1].is_empty() {
                                        args[n - 1].clone()
                                    } else {
                                        default_val.to_string()
                                    }
                                } else {
                                    default_val.to_string()
                                }
                            }
                        };
                        result.push_str(&val);
                        i = end + 1;
                        continue;
                    } else if let Some(stripped) = inner.strip_prefix("@:") {
                        let parts: Vec<&str> = stripped.split(':').collect();
                        if let Ok(start_1idx) = parts[0].parse::<usize>() {
                            let start = if start_1idx > 0 { start_1idx - 1 } else { 0 };
                            let slice_str = if parts.len() > 1 {
                                if let Ok(len) = parts[1].parse::<usize>() {
                                    let end = (start + len).min(args.len());
                                    if start < args.len() {
                                        args[start..end].join(" ")
                                    } else {
                                        String::new()
                                    }
                                } else if start < args.len() {
                                    args[start..].join(" ")
                                } else {
                                    String::new()
                                }
                            } else if start < args.len() {
                                args[start..].join(" ")
                            } else {
                                String::new()
                            };
                            result.push_str(&slice_str);
                            i = end + 1;
                            continue;
                        }
                    } else if inner == "*" || inner == "ARGUMENTS" {
                        result.push_str(&all_args);
                        i = end + 1;
                        continue;
                    } else if let Ok(n) = inner.parse::<usize>() {
                        if n > 0 && n <= args.len() {
                            result.push_str(&args[n - 1]);
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }

            // Simple placeholders: $@, $ARGUMENTS, $1..$N
            let remaining: String = chars[i + 1..].iter().collect();
            if remaining.starts_with("ARGUMENTS") {
                result.push_str(&all_args);
                i += 1 + "ARGUMENTS".len();
                continue;
            } else if remaining.starts_with('@') {
                result.push_str(&all_args);
                i += 2;
                continue;
            } else if let Some(first_digit) = chars.get(i + 1) {
                if first_digit.is_ascii_digit() {
                    let mut digit_end = i + 1;
                    while digit_end < chars.len() && chars[digit_end].is_ascii_digit() {
                        digit_end += 1;
                    }
                    let num_str: String = chars[i + 1..digit_end].iter().collect();
                    if let Ok(n) = num_str.parse::<usize>() {
                        if n > 0 && n <= args.len() {
                            result.push_str(&args[n - 1]);
                        }
                    }
                    i = digit_end;
                    continue;
                }
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Loads prompt templates from a directory (non-recursive) by reading `*.md` files.
fn load_templates_from_dir(dir: &Path, out: &mut Vec<PromptTemplate>) {
    if !dir.is_dir() {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("md") {
                    let name = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };

                    if let Ok(raw) = std::fs::read_to_string(&path) {
                        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
                        let (fm, body) = parse_frontmatter(clean);
                        let body_trimmed = body.trim();

                        let description = if let Some(desc) = fm.get("description") {
                            desc.clone()
                        } else {
                            let first_line = body_trimmed
                                .lines()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("");
                            let mut d = first_line.chars().take(60).collect::<String>();
                            if first_line.chars().count() > 60 {
                                d.push_str("...");
                            }
                            d
                        };

                        let argument_hint = fm
                            .get("argument-hint")
                            .or_else(|| fm.get("argument_hint"))
                            .cloned();

                        out.push(PromptTemplate {
                            name,
                            description,
                            argument_hint,
                            content: body_trimmed.to_string(),
                            file_path: path,
                        });
                    }
                }
            }
        }
    }
}

/// Loads prompt templates from `~/.mehh/agent/prompts/*.md` and `<cwd>/.mehh/prompts/*.md`.
/// Project-level templates override global templates with the same name.
/// Loads global prompt templates and, when `trusted` is true, project-local `.mehh/prompts`.
pub fn load_prompt_templates_with_trust(cwd: &Path, trusted: bool) -> Vec<PromptTemplate> {
    let mut template_map: HashMap<String, PromptTemplate> = HashMap::new();

    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    if let Some(home_path) = home {
        let global_dir = home_path.join(".mehh").join("agent").join("prompts");
        let mut global_templates = Vec::new();
        load_templates_from_dir(&global_dir, &mut global_templates);
        for t in global_templates {
            template_map.insert(t.name.clone(), t);
        }
    }

    if trusted {
        let local_dir = cwd.join(".mehh").join("prompts");
        let mut local_templates = Vec::new();
        load_templates_from_dir(&local_dir, &mut local_templates);
        for t in local_templates {
            template_map.insert(t.name.clone(), t);
        }
    }

    let mut result: Vec<PromptTemplate> = template_map.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Loads global prompt templates only (untrusted default). Call `load_prompt_templates_with_trust` for trusted projects.
pub fn load_prompt_templates(cwd: &Path) -> Vec<PromptTemplate> {
    load_prompt_templates_with_trust(cwd, false)
}

/// Expands `/<name> [args]` command input into substituted template content.
pub fn expand_prompt_template(input: &str, templates: &[PromptTemplate]) -> Option<String> {
    if !input.starts_with('/') || input.starts_with("/skill:") {
        return None;
    }

    let without_slash = &input[1..];
    let (cmd, args_str) = match without_slash.find(|c: char| c.is_whitespace()) {
        Some(idx) => (&without_slash[..idx], without_slash[idx..].trim()),
        None => (without_slash, ""),
    };

    let template = templates.iter().find(|t| t.name == cmd)?;
    let args = parse_command_args(args_str);
    let expanded = substitute_args(&template.content, &args);

    Some(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_command_args() {
        let args = parse_command_args(r#"first "second arg" 'third arg' fourth"#);
        assert_eq!(args, vec!["first", "second arg", "third arg", "fourth"]);

        let escaped = parse_command_args(r#"cmd "hello \"world\"" tail"#);
        assert_eq!(escaped, vec!["cmd", "hello \"world\"", "tail"]);
    }

    #[test]
    fn unterminated_quote_flushes_remaining_text_without_stray_quote() {
        assert_eq!(
            parse_command_args(r#"foo "bar baz"#),
            vec!["foo", "bar baz"]
        );
        assert_eq!(parse_command_args("'hello world"), vec!["hello world"]);
        assert_eq!(parse_command_args(r#""unclosed"#), vec!["unclosed"]);
    }

    #[test]
    fn test_substitute_args() {
        let template = "Run on $1 with options $@. Fallback: ${2:-default_val}. Slice: ${@:2:2}.";
        let args = vec![
            "target.rs".to_string(),
            "--release".to_string(),
            "--verbose".to_string(),
            "--all".to_string(),
        ];

        let substituted = substitute_args(template, &args);
        assert_eq!(
            substituted,
            "Run on target.rs with options target.rs --release --verbose --all. Fallback: --release. Slice: --release --verbose."
        );

        let missing_fallback = substitute_args("Check ${3:-my_fallback}", &["one".to_string()]);
        assert_eq!(missing_fallback, "Check my_fallback");

        let slice_rest = substitute_args(
            "Rest: ${@:2}",
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(slice_rest, "Rest: b c");
    }

    #[test]
    fn test_substitute_args_braced_numeric_and_all() {
        assert_eq!(
            substitute_args("Value: ${1}", &["x".to_string()]),
            "Value: x"
        );
        assert_eq!(
            substitute_args("Value: ${2}", &["x".to_string()]),
            "Value: "
        );
        assert_eq!(
            substitute_args("All: ${*} / ${ARGUMENTS}", &["a".to_string(), "b".to_string()]),
            "All: a b / a b"
        );
        assert_eq!(
            substitute_args("Keep: ${UNKNOWN}", &["a".to_string()]),
            "Keep: ${UNKNOWN}"
        );
    }

    #[test]
    fn test_expand_prompt_template() {
        let templates = vec![PromptTemplate {
            name: "review".to_string(),
            description: "Review code changes".to_string(),
            argument_hint: Some("[file]".to_string()),
            content: "Please review $1 focusing on performance.".to_string(),
            file_path: PathBuf::from("review.md"),
        }];

        let expanded = expand_prompt_template("/review src/lib.rs", &templates);
        assert_eq!(
            expanded.as_deref(),
            Some("Please review src/lib.rs focusing on performance.")
        );

        let unknown = expand_prompt_template("/unknown arg", &templates);
        assert!(unknown.is_none());

        let skill_passthrough = expand_prompt_template("/skill:review arg", &templates);
        assert!(skill_passthrough.is_none());
    }

    #[test]
    fn test_load_prompt_templates_respects_trust_and_strips_bom() {
        let dir = std::env::temp_dir().join(format!("mehh_prompt_trust_{}", uuid::Uuid::now_v7()));
        let tpl_dir = dir.join(".mehh").join("prompts");
        std::fs::create_dir_all(&tpl_dir).unwrap();
        std::fs::write(
            tpl_dir.join("audit.md"),
            "\u{feff}---\ndescription: Audit template\n---\nCheck $@",
        )
        .unwrap();

        let untrusted = load_prompt_templates_with_trust(&dir, false);
        assert!(!untrusted.iter().any(|t| t.name == "audit"));

        let trusted = load_prompt_templates_with_trust(&dir, true);
        let found = trusted.iter().find(|t| t.name == "audit").unwrap();
        assert_eq!(found.description, "Audit template");
        assert_eq!(found.content, "Check $@");

        let _ = std::fs::remove_dir_all(dir);
    }
}
