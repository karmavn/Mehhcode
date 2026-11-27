//! System prompt construction and project instruction file discovery.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::skills::{
    escape_xml, format_skills_for_prompt, read_bounded_utf8_file, Skill, MAX_MARKDOWN_FILE_BYTES,
};

fn tool_summary(name: &str) -> Option<&'static str> {
    match name {
        "read" => Some("Read file contents"),
        "write" => Some("Create or overwrite files"),
        "edit" => Some("Apply targeted edits to files"),
        "undo_edit" => Some("Revert the last edit to a file"),
        "bash" => Some("Run shell commands"),
        "powershell" => Some("Run PowerShell commands"),
        "grep" => Some("Search text inside local workspace files"),
        "find" => Some("Find files by path or glob in the local workspace"),
        "ls" => Some("List files and directories in the local workspace"),
        "analyze" => Some("Inspect code structure and symbols in the local workspace"),
        "web_search" => Some(
            "Search the live internet for real-time info, docs, news, weather, releases, and external projects",
        ),
        "web_fetch" => {
            Some("Fetch and read content from a public HTTP/HTTPS URL as markdown, text, or HTML")
        }
        "subagent" => Some("Delegate focused subtasks to child agents"),
        "todo" => Some("Track multi-step tasks and progress"),
        "codemode" => Some("Execute multi-tool workflows via sandboxed JavaScript"),
        "tool_search" => Some("Discover deferred MCP tools by keyword"),
        _ => None,
    }
}

fn current_utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil_from_days: extracts Gregorian YYYY-MM-DD from Unix day offset.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    format!("{year:04}-{m:02}-{d:02}")
}

/// Discovers project context files (`AGENTS.md`, `CLAUDE.md`) walking up from `cwd`,
/// stopping at the git repository root (`.git`) if present.
/// Returns them ordered from root to cwd.
pub fn discover_context_files(cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let candidates = [
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ];

    let mut current = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .join(cwd)
    };

    loop {
        for candidate in &candidates {
            let candidate_path = current.join(candidate);
            if candidate_path.is_file() {
                if let Some(content) =
                    read_bounded_utf8_file(&candidate_path, MAX_MARKDOWN_FILE_BYTES)
                {
                    files.push((candidate_path, content));
                    break;
                }
            }
        }

        // Do not walk above the git repository root.
        if current.join(".git").exists() {
            break;
        }

        if let Some(parent) = current.parent() {
            if parent == current {
                break;
            }
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    files.reverse();
    files
}

/// Builds the structured system prompt for the coding agent.
pub fn build_system_prompt(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
) -> String {
    build_system_prompt_inner(cwd, custom_prompt, append_prompt, tool_names, true)
}

fn build_system_prompt_with_context_slice(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    context_files: &[(PathBuf, String)],
) -> String {
    let mut sections = Vec::new();

    let has_web_search = tool_names.contains(&"web_search");
    let has_web_fetch = tool_names.contains(&"web_fetch");

    let preamble = match custom_prompt {
        Some(cp) if !cp.trim().is_empty() => cp.to_string(),
        _ if has_web_search || has_web_fetch => {
            "You are a coding assistant running inside mehh, a coding agent harness. Help users inspect files, run commands, modify code, and look up live information on the web using your enabled web tools.".to_string()
        }
        _ => "You are a coding assistant running inside mehh, a coding agent harness. Help users inspect files, run commands, and modify code.".to_string(),
    };
    sections.push(preamble);

    if !tool_names.is_empty() {
        let mut tools_sec = String::from("<tools>\n");
        for tool in tool_names {
            if let Some(desc) = tool_summary(tool) {
                tools_sec.push_str(&format!("- {tool}: {desc}\n"));
            } else {
                tools_sec.push_str(&format!("- {tool}\n"));
            }
        }
        tools_sec.push_str("\nAdditional tools may be available.\n</tools>");
        sections.push(tools_sec);
    }

    let mut rules = vec![
        "Be concise in your responses",
        "Show file paths clearly when working with files",
    ];
    let has_bash = tool_names.contains(&"bash");
    let has_ps = tool_names.contains(&"powershell");
    let has_grep = tool_names.contains(&"grep");
    let has_find = tool_names.contains(&"find");
    let has_ls = tool_names.contains(&"ls");
    if (has_bash || has_ps) && !has_grep && !has_find && !has_ls {
        let rule = if has_ps && !has_bash {
            "Use powershell for file operations like ls, rg, find"
        } else {
            "Use bash for file operations like ls, rg, find"
        };
        rules.insert(0, rule);
    }
    if has_web_search && has_web_fetch {
        rules.push("You have live internet access through web_search and web_fetch. Do not claim you lack internet access or cannot look up current information.");
        rules.push("Use web_search for real-time or external queries (weather, news, latest library/tool releases, external open-source projects, documentation outside this workspace); use grep, find, and ls only for files inside the local workspace.");
        rules.push("Use web_fetch to read the full content of specific URLs provided by the user or returned by web_search.");
    } else if has_web_search {
        rules.push("You have live internet search through web_search. Do not claim you lack internet access; use web_search for real-time or external queries (weather, news, latest releases, external projects, documentation) instead of searching only local workspace files.");
    } else if has_web_fetch {
        rules.push("You can fetch live web pages through web_fetch. Use web_fetch whenever the user provides a URL or asks you to inspect a webpage.");
    }

    let mut rules_sec = String::from("<rules>\n");
    for rule in rules {
        rules_sec.push_str(&format!("- {rule}\n"));
    }
    rules_sec.push_str("</rules>");
    sections.push(rules_sec);

    if !context_files.is_empty() {
        let mut proj_sec =
            String::from("<project_context>\nProject-specific instructions and guidelines:\n\n");
        for (path, content) in context_files {
            let normalized_path = path.to_string_lossy().replace('\\', "/");
            proj_sec.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                escape_xml(&normalized_path),
                content.trim()
            ));
        }
        proj_sec.push_str("</project_context>");
        sections.push(proj_sec);
    }

    if let Some(ap) = append_prompt {
        if !ap.trim().is_empty() {
            sections.push(format!("<addendum>\n{}\n</addendum>", ap.trim()));
        }
    }

    sections.push(format!("<current_date>\n{}\n</current_date>", current_utc_date()));

    let normalized_cwd = cwd.to_string_lossy().replace('\\', "/");
    sections.push(format!("<cwd>\n{}\n</cwd>", normalized_cwd));

    sections.join("\n\n")
}

fn build_system_prompt_inner(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    include_context_files: bool,
) -> String {
    let context_files = if include_context_files {
        discover_context_files(cwd)
    } else {
        Vec::new()
    };
    build_system_prompt_with_context_slice(
        cwd,
        custom_prompt,
        append_prompt,
        tool_names,
        &context_files,
    )
}

/// Variant accepting pre-discovered project context files so repeated prompt rebuilds avoid re-walking disk.
pub fn build_system_prompt_with_skills_and_preloaded_context(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    skills: &[Skill],
    context_files: &[(PathBuf, String)],
) -> String {
    let mut prompt = build_system_prompt_with_context_slice(
        cwd,
        custom_prompt,
        append_prompt,
        tool_names,
        context_files,
    );
    let skills_sec = format_skills_for_prompt(skills);
    if !skills_sec.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skills_sec);
    }
    prompt
}

/// Builds the structured system prompt for the coding agent including discovered skills.
pub fn build_system_prompt_with_skills(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    skills: &[Skill],
) -> String {
    build_system_prompt_with_skills_and_context(
        cwd,
        custom_prompt,
        append_prompt,
        tool_names,
        skills,
        true,
    )
}

/// Variant that can omit project context files (AGENTS.md / CLAUDE.md).
pub fn build_system_prompt_with_skills_and_context(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    skills: &[Skill],
    include_context_files: bool,
) -> String {
    let mut prompt = build_system_prompt_inner(
        cwd,
        custom_prompt,
        append_prompt,
        tool_names,
        include_context_files,
    );
    let skills_sec = format_skills_for_prompt(skills);
    if !skills_sec.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skills_sec);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_powershell_only_prompt_uses_powershell_for_file_ops() {
        let prompt = build_system_prompt(Path::new("."), None, None, &["powershell"]);
        assert!(
            prompt.contains("Use powershell for file operations"),
            "expected powershell guidance, got: {prompt}"
        );
        assert!(
            !prompt.contains("Use bash for file operations"),
            "bash is not available, got: {prompt}"
        );
    }

    #[test]
    fn test_bash_prompt_uses_bash_for_file_ops() {
        let prompt = build_system_prompt(Path::new("."), None, None, &["bash"]);
        assert!(
            prompt.contains("Use bash for file operations"),
            "expected bash guidance, got: {prompt}"
        );
        assert!(
            !prompt.contains("Use powershell for file operations"),
            "powershell is not available, got: {prompt}"
        );
    }

    #[test]
    fn test_web_tools_add_explicit_awareness_and_omit_when_disabled() {
        let with_both =
            build_system_prompt(Path::new("."), None, None, &["read", "grep", "web_search", "web_fetch"]);
        assert!(with_both.contains("web_search:"));
        assert!(with_both.contains("web_fetch:"));
        assert!(with_both.contains("live internet"));
        assert!(with_both.contains("Do not claim you lack internet access"));
        assert!(with_both.contains("<current_date>"));

        let offline = build_system_prompt(Path::new("."), None, None, &["read", "grep"]);
        assert!(!offline.contains("web_search"));
        assert!(!offline.contains("web_fetch"));
        assert!(!offline.contains("live internet"));

        let search_only = build_system_prompt(Path::new("."), None, None, &["web_search"]);
        assert!(search_only.contains("web_search:"));
        assert!(search_only.contains("live internet"));
        assert!(!search_only.contains("web_fetch"));

        let fetch_only = build_system_prompt(Path::new("."), None, None, &["web_fetch"]);
        assert!(fetch_only.contains("web_fetch:"));
        assert!(!fetch_only.contains("web_search"));
    }

    #[test]
    fn discover_context_files_stops_at_git_root_strips_bom_and_escapes_path() {
        let dir = crate::TempTestDir::new("sys_prompt_git_root");
        // Above repo root: must NOT be walked into once `.git` is encountered.
        std::fs::write(dir.path().join("AGENTS.md"), "parent outside repo").unwrap();

        let repo_root = dir.path().join("repo&'proj");
        let sub = repo_root.join("crates").join("app");
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            repo_root.join("AGENTS.md"),
            "\u{feff}# Repo Instructions\nUse safe Rust.",
        )
        .unwrap();

        let discovered = discover_context_files(&sub);
        assert_eq!(discovered.len(), 1, "walk must stop at .git root");
        assert!(!discovered[0].1.starts_with('\u{feff}'));
        assert!(discovered[0].1.contains("Repo Instructions"));

        let prompt = build_system_prompt(&sub, None, None, &["read"]);
        assert!(!prompt.contains("parent outside repo"));
        assert!(prompt.contains("repo&amp;&apos;proj/AGENTS.md"));
    }
}
