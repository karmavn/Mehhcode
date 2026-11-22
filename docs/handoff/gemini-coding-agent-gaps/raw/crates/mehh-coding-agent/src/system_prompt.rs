//! System prompt construction and project instruction file discovery.

use std::path::{Path, PathBuf};

use crate::skills::{format_skills_for_prompt, Skill};

/// Discovers project context files (`AGENTS.md`, `CLAUDE.md`) walking up from `cwd` to root.
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
                if let Ok(content) = std::fs::read_to_string(&candidate_path) {
                    files.push((candidate_path, content));
                    break;
                }
            }
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

fn build_system_prompt_inner(
    cwd: &Path,
    custom_prompt: Option<&str>,
    append_prompt: Option<&str>,
    tool_names: &[&str],
    include_context_files: bool,
) -> String {
    let mut sections = Vec::new();

    let preamble = match custom_prompt {
        Some(cp) if !cp.trim().is_empty() => cp.to_string(),
        _ => "You are a coding assistant running inside mehh, a coding agent harness. Help users inspect files, run commands, and modify code.".to_string(),
    };
    sections.push(preamble);

    if !tool_names.is_empty() {
        let mut tools_sec = String::from("<tools>\n");
        for tool in tool_names {
            tools_sec.push_str(&format!("- {tool}\n"));
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
        rules.insert(0, "Use bash for file operations like ls, rg, find");
    }

    let mut rules_sec = String::from("<rules>\n");
    for rule in rules {
        rules_sec.push_str(&format!("- {rule}\n"));
    }
    rules_sec.push_str("</rules>");
    sections.push(rules_sec);

    let context_files = if include_context_files {
        discover_context_files(cwd)
    } else {
        Vec::new()
    };
    if !context_files.is_empty() {
        let mut proj_sec =
            String::from("<project_context>\nProject-specific instructions and guidelines:\n\n");
        for (path, content) in &context_files {
            let normalized_path = path.to_string_lossy().replace('\\', "/");
            proj_sec.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                normalized_path,
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

    let normalized_cwd = cwd.to_string_lossy().replace('\\', "/");
    sections.push(format!("<cwd>\n{}\n</cwd>", normalized_cwd));

    sections.join("\n\n")
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
