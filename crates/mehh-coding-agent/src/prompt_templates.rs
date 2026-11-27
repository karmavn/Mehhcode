//! Prompt template discovery, argument parsing, and parameter substitution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::skills::{parse_frontmatter, read_bounded_utf8_file, MAX_MARKDOWN_FILE_BYTES};

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
    let mut all_args_cache: Option<String> = None;
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(dollar_pos) = rest.find('$') {
        result.push_str(&rest[..dollar_pos]);
        let after_dollar = &rest[dollar_pos + 1..];

        if let Some(after_brace) = after_dollar.strip_prefix('{') {
            if let Some(closing) = after_brace.find('}') {
                let inner = &after_brace[..closing];
                let after_close = &after_brace[closing + 1..];

                if let Some((target, default_val)) = inner.split_once(":-") {
                    match target {
                        "@" | "ARGUMENTS" => {
                            let joined = all_args_cache
                                .get_or_insert_with(|| args.join(" "))
                                .as_str();
                            if joined.is_empty() {
                                result.push_str(default_val);
                            } else {
                                result.push_str(joined);
                            }
                        }
                        _ => {
                            if let Ok(n) = target.parse::<usize>() {
                                if n > 0 && n <= args.len() && !args[n - 1].is_empty() {
                                    result.push_str(&args[n - 1]);
                                } else {
                                    result.push_str(default_val);
                                }
                            } else {
                                result.push_str(default_val);
                            }
                        }
                    }
                    rest = after_close;
                    continue;
                } else if let Some(stripped) = inner.strip_prefix("@:") {
                    let mut split_it = stripped.split(':');
                    let first_part = split_it.next().unwrap_or("");
                    if let Ok(start_1idx) = first_part.parse::<usize>() {
                        let start = if start_1idx > 0 { start_1idx - 1 } else { 0 };
                        let second_part = split_it.next();
                        let end = if let Some(len_str) = second_part {
                            if let Ok(len) = len_str.parse::<usize>() {
                                start.saturating_add(len).min(args.len())
                            } else {
                                args.len()
                            }
                        } else {
                            args.len()
                        };
                        if start < end {
                            for (idx, a) in args[start..end].iter().enumerate() {
                                if idx > 0 {
                                    result.push(' ');
                                }
                                result.push_str(a);
                            }
                        }
                        rest = after_close;
                        continue;
                    }
                } else if inner == "*" || inner == "ARGUMENTS" {
                    let joined = all_args_cache
                        .get_or_insert_with(|| args.join(" "))
                        .as_str();
                    result.push_str(joined);
                    rest = after_close;
                    continue;
                } else if let Ok(n) = inner.parse::<usize>() {
                    if n > 0 && n <= args.len() {
                        result.push_str(&args[n - 1]);
                    }
                    rest = after_close;
                    continue;
                }
            }
        }

        // Simple placeholders: $@, $ARGUMENTS, $1..$N
        if let Some(after_kw) = after_dollar.strip_prefix("ARGUMENTS") {
            let joined = all_args_cache
                .get_or_insert_with(|| args.join(" "))
                .as_str();
            result.push_str(joined);
            rest = after_kw;
            continue;
        } else if let Some(after_at) = after_dollar.strip_prefix('@') {
            let joined = all_args_cache
                .get_or_insert_with(|| args.join(" "))
                .as_str();
            result.push_str(joined);
            rest = after_at;
            continue;
        } else {
            let digits_end = after_dollar
                .as_bytes()
                .iter()
                .position(|b| !b.is_ascii_digit())
                .unwrap_or(after_dollar.len());
            if digits_end > 0 {
                if let Ok(n) = after_dollar[..digits_end].parse::<usize>() {
                    if n > 0 && n <= args.len() {
                        result.push_str(&args[n - 1]);
                    }
                }
                rest = &after_dollar[digits_end..];
                continue;
            }
        }

        result.push('$');
        rest = after_dollar;
    }

    result.push_str(rest);
    result
}

/// Loads prompt templates from a directory (non-recursive) by reading `*.md` files.
/// Enforces bounded reads, strips UTF-8 BOM, and requires symlinks to stay within `dir`.
fn load_templates_from_dir(dir: &Path, out: &mut Vec<PromptTemplate>) {
    let Ok(canon_root) = std::fs::canonicalize(dir) else {
        return;
    };
    if !canon_root.is_dir() {
        return;
    }

    let entries = match std::fs::read_dir(&canon_root) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(canon_path) = std::fs::canonicalize(&path) else {
            continue;
        };
        if !canon_path.starts_with(&canon_root) || !canon_path.is_file() {
            continue;
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("md") {
                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                if let Some(clean) = read_bounded_utf8_file(&canon_path, MAX_MARKDOWN_FILE_BYTES) {
                    let (fm, body) = parse_frontmatter(&clean);
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

/// Loads prompt templates from `~/.mehh/agent/prompts/*.md` and `<cwd>/.mehh/prompts/*.md`.
/// Project-level templates override global templates with the same name.
/// Loads global prompt templates and, when `trusted` is true, project-local `.mehh/prompts`.
pub fn load_prompt_templates_with_trust(cwd: &Path, trusted: bool) -> Vec<PromptTemplate> {
    let mut template_map: HashMap<String, PromptTemplate> = HashMap::new();

    if let Some(home_path) = crate::user_home_dir() {
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

    #[test]
    fn test_substitute_args_slice_saturates_on_huge_length() {
        let out = substitute_args(
            "Slice: ${@:2:18446744073709551615}",
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(out, "Slice: b c");
    }

    #[test]
    fn test_load_prompt_templates_bounds_reads_and_strips_bom_without_frontmatter() {
        let dir = crate::TempTestDir::new("prompt_bounds");
        let tpl_dir = dir.path().join(".mehh").join("prompts");
        std::fs::create_dir_all(&tpl_dir).unwrap();
        std::fs::write(tpl_dir.join("plain.md"), "\u{feff}Plain body $@").unwrap();
        std::fs::write(
            tpl_dir.join("huge.md"),
            "x".repeat(MAX_MARKDOWN_FILE_BYTES + 1024),
        )
        .unwrap();

        let loaded = load_prompt_templates_with_trust(dir.path(), true);
        let plain = loaded.iter().find(|t| t.name == "plain").unwrap();
        assert_eq!(plain.content, "Plain body $@");
        assert!(!loaded.iter().any(|t| t.name == "huge"));
    }

    #[cfg(unix)]
    #[test]
    fn test_load_prompt_templates_rejects_symlink_escape() {
        let dir = crate::TempTestDir::new("prompt_symlink");
        let tpl_dir = dir.path().join(".mehh").join("prompts");
        let outside = dir.path().join("secret.md");
        std::fs::create_dir_all(&tpl_dir).unwrap();
        std::fs::write(&outside, "top secret").unwrap();
        std::os::unix::fs::symlink(&outside, tpl_dir.join("escape.md")).unwrap();

        let loaded = load_prompt_templates_with_trust(dir.path(), true);
        assert!(!loaded.iter().any(|t| t.name == "escape"));
    }
}
