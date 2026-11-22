# Dump 06 - Context, skills, memory, compaction

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-coding-agent/src/todo.rs | 11.7 | 367 | 9c33ff1684debb411d627059a35f74ca0200ba58a3b5cbe2e5e782be09524b19 |
| crates/mehh-coding-agent/src/system_prompt.rs | 5.4 | 178 | 9079c18a656fbac254a36ebc0d3c8f3383397af39ae47241fc1cf38ce8ea0126 |
| crates/mehh-coding-agent/src/skills.rs | 12.4 | 353 | a22854fee3cd54ef290528ec4982c596ddad8b7978be53c3942d069e0c05c3fe |
| crates/mehh-coding-agent/src/prompt_templates.rs | 14.6 | 386 | 4d8d1ced7be5425cd58dbe48ce996c495086700ec85590a205765a76488495d5 |
| crates/mehh-coding-agent/src/compaction.rs | 38.4 | 1039 | e0fb0683ff6d73a500e9a793e29be3dd0572f1b71ab0ea82875b540a2678c679 |

## crates/mehh-coding-agent/src/todo.rs

`````rust
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn checkbox(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[/]",
            Self::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: usize,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
    next_id: usize,
}

impl Default for TodoState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }
}

impl TodoState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, content: String) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(TodoItem {
            id,
            content,
            status: TodoStatus::Pending,
        });
        id
    }

    pub fn update(
        &mut self,
        id: usize,
        status: Option<TodoStatus>,
        content: Option<String>,
    ) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            if let Some(s) = status {
                item.status = s;
            }
            if let Some(c) = content {
                item.content = c;
            }
            true
        } else {
            false
        }
    }

    pub fn replace(&mut self, new_items: Vec<TodoItem>) {
        self.next_id = new_items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        self.items = new_items;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 1;
    }

    pub fn format_moim_block(&self, cwd: &Path, trusted: bool) -> Option<String> {
        let persistent_memory = if trusted {
            let moim_path = cwd.join(".mehh").join("MOIM.md");
            if let Ok(raw) = std::fs::read_to_string(&moim_path) {
                let clamped = if raw.len() > 8192 {
                    let mut end = 8192;
                    while end > 0 && !raw.is_char_boundary(end) {
                        end -= 1;
                    }
                    raw[..end].to_string()
                } else {
                    raw
                };
                Some(clamped)
            } else {
                None
            }
        } else {
            None
        };

        if self.items.is_empty()
            && persistent_memory
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
        {
            return None;
        }

        let mut block = String::from("<working_checklist>\n");
        if !self.items.is_empty() {
            block.push_str("Active tasks:\n");
            for item in &self.items {
                block.push_str(&format!(
                    "{} #{} {} ({})\n",
                    item.status.checkbox(),
                    item.id,
                    item.content,
                    item.status.as_str()
                ));
            }
        }

        if let Some(ref mem) = persistent_memory {
            let trimmed = mem.trim();
            if !trimmed.is_empty() {
                if !self.items.is_empty() {
                    block.push('\n');
                }
                block.push_str("Project Memory & Goals (MOIM.md):\n");
                block.push_str(trimmed);
                block.push('\n');
            }
        }

        block.push_str("</working_checklist>");
        Some(block)
    }
}

#[derive(Debug, Clone)]
pub struct TodoTool {
    state: Arc<RwLock<TodoState>>,
}

impl TodoTool {
    pub fn new(state: Arc<RwLock<TodoState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage structured working tasks and goals. Modifies the working checklist injected into context every turn. Actions: 'add', 'update', 'replace', 'list', 'clear'."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "update", "replace", "list", "clear"],
                    "description": "Operation to perform on the todo list"
                },
                "id": {
                    "type": "integer",
                    "description": "Item ID for 'update'"
                },
                "content": {
                    "type": "string",
                    "description": "Task description for 'add' or 'update'"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "Status for 'update'"
                },
                "items": {
                    "type": "array",
                    "description": "List of items for 'replace'",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "integer" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn is_read_only(&self) -> Option<bool> {
        None
    }

    fn is_safe_for_auto_approve(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<bool> {
        let is_list = args
            .get("action")
            .and_then(|v| v.as_str())
            .is_some_and(|a| a == "list");
        Some(is_list)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter 'action'".to_string())?;

        match action {
            "add" => {
                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Parameter 'content' is required for action 'add'".to_string())?;

                let mut state = self.state.write().await;
                let id = state.add(content.to_string());
                Ok(AgentToolResult::text(format!(
                    "Added task #{}: {}",
                    id, content
                )))
            }
            "update" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| "Parameter 'id' is required for action 'update'".to_string())?
                    as usize;

                let status = params.get("status").and_then(|v| v.as_str()).map(|s| {
                    match s {
                        "in_progress" => TodoStatus::InProgress,
                        "completed" => TodoStatus::Completed,
                        _ => TodoStatus::Pending,
                    }
                });

                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let mut state = self.state.write().await;
                if state.update(id, status, content) {
                    Ok(AgentToolResult::text(format!("Updated task #{}", id)))
                } else {
                    Err(format!("Task #{} not found", id))
                }
            }
            "replace" => {
                let items_val = params
                    .get("items")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| "Parameter 'items' array is required for 'replace'".to_string())?;

                let mut new_items = Vec::new();
                for (idx, item_val) in items_val.iter().enumerate() {
                    let id = item_val
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .map(|u| u as usize)
                        .unwrap_or(idx + 1);
                    let content = item_val
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = item_val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| match s {
                            "in_progress" => TodoStatus::InProgress,
                            "completed" => TodoStatus::Completed,
                            _ => TodoStatus::Pending,
                        })
                        .unwrap_or(TodoStatus::Pending);

                    new_items.push(TodoItem { id, content, status });
                }

                let count = new_items.len();
                let mut state = self.state.write().await;
                state.replace(new_items);
                Ok(AgentToolResult::text(format!(
                    "Replaced working checklist with {} items.",
                    count
                )))
            }
            "clear" => {
                let mut state = self.state.write().await;
                state.clear();
                Ok(AgentToolResult::text("Cleared all working tasks."))
            }
            "list" => {
                let state = self.state.read().await;
                if state.items.is_empty() {
                    Ok(AgentToolResult::text("Working checklist is currently empty."))
                } else {
                    let mut out = String::from("Current working tasks:\n");
                    for item in &state.items {
                        out.push_str(&format!(
                            "{} #{} {} ({})\n",
                            item.status.checkbox(),
                            item.id,
                            item.content,
                            item.status.as_str()
                        ));
                    }
                    Ok(AgentToolResult::text(out))
                }
            }
            unknown => Err(format!(
                "Unknown action '{}'. Supported actions: 'add', 'update', 'replace', 'list', 'clear'",
                unknown
            )),
        }
    }
}
`````

## crates/mehh-coding-agent/src/system_prompt.rs

`````rust
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
`````

## crates/mehh-coding-agent/src/skills.rs

`````rust
//! Skill discovery, frontmatter parsing, prompt formatting, and invocation expansion.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Skill definition loaded from a `SKILL.md` markdown file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub disable_model_invocation: bool,
}

/// Parses YAML-style frontmatter from markdown text.
/// Returns a map of key-value pairs and the remaining body text.
pub fn parse_frontmatter(content: &str) -> (HashMap<String, String>, &str) {
    let mut map = HashMap::new();
    let clean = content.strip_prefix('\u{feff}').unwrap_or(content);
    let trimmed = clean.trim_start();

    if !trimmed.starts_with("---") {
        return (map, content);
    }

    let after_start = &trimmed[3..];
    let after_nl = if let Some(stripped) = after_start.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = after_start.strip_prefix('\n') {
        stripped
    } else {
        return (map, content);
    };

    let (fm_str, body) = match after_nl.find("\n---") {
        Some(idx) => {
            let fm = &after_nl[..idx];
            let rest = &after_nl[idx + 4..];
            let body = if let Some(stripped) = rest.strip_prefix("\r\n") {
                stripped
            } else if let Some(stripped) = rest.strip_prefix('\n') {
                stripped
            } else {
                rest.trim_start_matches('-')
            };
            (fm, body)
        }
        None => return (map, content),
    };

    for line in fm_str.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            map.insert(key, val);
        }
    }

    (map, body)
}

/// Recursively scans a directory for `SKILL.md` files, ignoring hidden dirs, `node_modules`, and `target`.
fn scan_dir_for_skills(dir: &Path, out: &mut Vec<Skill>) {
    if !dir.is_dir() {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') || name_str == "node_modules" || name_str == "target" {
                continue;
            }
            scan_dir_for_skills(&path, out);
        } else if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.eq_ignore_ascii_case("skill.md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let (fm, _) = parse_frontmatter(&content);
                        let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
                        let default_name = base_dir
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unnamed")
                            .to_string();

                        let name = fm.get("name").cloned().unwrap_or(default_name);
                        let description = fm.get("description").cloned().unwrap_or_default();
                        let disable_model_invocation = fm
                            .get("disable-model-invocation")
                            .map(|v| v.eq_ignore_ascii_case("true"))
                            .unwrap_or(false);

                        out.push(Skill {
                            name,
                            description,
                            file_path: path,
                            base_dir,
                            disable_model_invocation,
                        });
                    }
                }
            }
        }
    }
}

/// Loads skills from global directory `~/.mehh/agent/skills` and project directory `<cwd>/.mehh/skills`.
/// Project skills override global skills with the same name.
/// Loads global skills and, when `trusted` is true, project-local skills under `.agents/skills` and `.mehh/skills`.
pub fn load_skills_with_trust(cwd: &Path, trusted: bool) -> Vec<Skill> {
    let mut skill_map: HashMap<String, Skill> = HashMap::new();

    // 1. Global skills
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    if let Some(home_path) = home {
        let global_dir = home_path.join(".mehh").join("agent").join("skills");
        let mut global_skills = Vec::new();
        scan_dir_for_skills(&global_dir, &mut global_skills);
        for skill in global_skills {
            skill_map.insert(skill.name.clone(), skill);
        }
    }

    // 2. Project local skills (only when project directory is trusted)
    if trusted {
        for local_dir in [
            cwd.join(".agents").join("skills"),
            cwd.join(".mehh").join("skills"),
        ] {
            let mut local_skills = Vec::new();
            scan_dir_for_skills(&local_dir, &mut local_skills);
            for skill in local_skills {
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    }

    let mut result: Vec<Skill> = skill_map.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Loads global skills only (untrusted default). Call `load_skills_with_trust` for trusted projects.
pub fn load_skills(cwd: &Path) -> Vec<Skill> {
    load_skills_with_trust(cwd, false)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Formats available skills as an `<available_skills>` XML block for the system prompt.
/// Excludes skills with `disable_model_invocation: true`.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();

    if visible_skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("<available_skills>\n");
    for s in visible_skills {
        let loc = s.file_path.to_string_lossy().replace('\\', "/");
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_xml(&s.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&s.description)
        ));
        out.push_str(&format!("    <location>{}</location>\n", escape_xml(&loc)));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");

    out
}

/// Expands `/skill:<name> [args]` command input into full skill prompt markdown.
pub fn expand_skill_command(input: &str, skills: &[Skill]) -> Option<String> {
    if !input.starts_with("/skill:") {
        return None;
    }

    let rest = &input["/skill:".len()..];
    let (skill_name, args) = match rest.find(|c: char| c.is_whitespace()) {
        Some(idx) => (&rest[..idx], rest[idx..].trim()),
        None => (rest, ""),
    };

    let skill = skills.iter().find(|s| s.name == skill_name)?;
    let content = std::fs::read_to_string(&skill.file_path).ok()?;
    let (_, body) = parse_frontmatter(&content);

    let norm_file = skill.file_path.to_string_lossy().replace('\\', "/");
    let norm_base = skill.base_dir.to_string_lossy().replace('\\', "/");

    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        norm_file,
        norm_base,
        body.trim()
    );

    let result = if args.is_empty() {
        skill_block
    } else {
        format!("{skill_block}\n\n{args}")
    };

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn test_frontmatter_parsing() {
        let doc = r#"---
name: rust-best-practices
description: Guidelines for high performance Rust code
disable-model-invocation: true
---
# Rust Best Practices
Use Clippy and avoid unnecessary allocations.
"#;
        let (fm, body) = parse_frontmatter(doc);
        assert_eq!(
            fm.get("name").map(|s| s.as_str()),
            Some("rust-best-practices")
        );
        assert_eq!(
            fm.get("description").map(|s| s.as_str()),
            Some("Guidelines for high performance Rust code")
        );
        assert_eq!(
            fm.get("disable-model-invocation").map(|s| s.as_str()),
            Some("true")
        );
        assert!(body.contains("# Rust Best Practices"));
    }

    #[test]
    fn test_format_skills_for_prompt() {
        let skills = vec![
            Skill {
                name: "test-skill".to_string(),
                description: "A test description & special chars <foo>".to_string(),
                file_path: PathBuf::from("C:/skills/test/SKILL.md"),
                base_dir: PathBuf::from("C:/skills/test"),
                disable_model_invocation: false,
            },
            Skill {
                name: "hidden-skill".to_string(),
                description: "Hidden from model invocation".to_string(),
                file_path: PathBuf::from("C:/skills/hidden/SKILL.md"),
                base_dir: PathBuf::from("C:/skills/hidden"),
                disable_model_invocation: true,
            },
        ];

        let formatted = format_skills_for_prompt(&skills);
        assert!(formatted.contains("<name>test-skill</name>"));
        assert!(formatted.contains("&amp; special chars &lt;foo&gt;"));
        assert!(!formatted.contains("hidden-skill"));
    }

    #[test]
    fn test_expand_skill_command() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_skill_test_{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_dir).unwrap();
        let skill_file = temp_dir.join("SKILL.md");

        fs::write(
            &skill_file,
            "---\nname: refactor\ndescription: Refactor tool\n---\nDetailed instructions for refactoring.",
        )
        .unwrap();

        let skills = vec![Skill {
            name: "refactor".to_string(),
            description: "Refactor tool".to_string(),
            file_path: skill_file.clone(),
            base_dir: temp_dir.clone(),
            disable_model_invocation: false,
        }];

        let expanded = expand_skill_command("/skill:refactor target=src/main.rs", &skills).unwrap();
        assert!(expanded.contains("<skill name=\"refactor\""));
        assert!(expanded.contains("Detailed instructions for refactoring."));
        assert!(expanded.ends_with("target=src/main.rs"));

        let nonexistent = expand_skill_command("/skill:unknown", &skills);
        assert!(nonexistent.is_none());

        let non_skill_input = expand_skill_command("/help me", &skills);
        assert!(non_skill_input.is_none());

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_load_skills_respects_trust_and_strips_bom() {
        let dir = std::env::temp_dir().join(format!("mehh_skill_trust_{}", Uuid::now_v7()));
        let skill_dir = dir.join(".agents").join("skills").join("proj-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "\u{feff}---\nname: proj-skill\ndescription: Project skill\n---\nBody text",
        )
        .unwrap();

        // Untrusted project must NOT load repo-local skills
        let untrusted = load_skills_with_trust(&dir, false);
        assert!(!untrusted.iter().any(|s| s.name == "proj-skill"));

        // Trusted project loads .agents/skills and strips UTF-8 BOM cleanly
        let trusted = load_skills_with_trust(&dir, true);
        let found = trusted.iter().find(|s| s.name == "proj-skill").unwrap();
        assert_eq!(found.description, "Project skill");

        let _ = fs::remove_dir_all(dir);
    }
}
`````

## crates/mehh-coding-agent/src/prompt_templates.rs

`````rust
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
    let mut quote_start = 0;
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
            quote_start = i;
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
            in_quote = None;
            current = chars[quote_start].to_string();
            i = quote_start + 1;
            continue;
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
`````

## crates/mehh-coding-agent/src/compaction.rs

`````rust
//! Context compaction and history summarization for long agent sessions.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use mehh_agent::types::tool_to_definition;
use mehh_agent::AgentTool;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use mehh_ai::{
    now_ms, AssistantContentBlock, Context, LlmProvider, Message, Model, StopReason, StreamOptions,
    UserContent, UserMessage,
};

use crate::session::{SessionEntry, SessionEntryKind};

/// Default token threshold retained at the tail of the active branch.
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;

/// Default headroom reserved before `context_window` to trigger proactive auto-compaction.
pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;

/// Maximum character limit for tool results in serialized conversation transcripts.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Budget settings controlling proactive and overflow-triggered compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: usize,
    pub keep_recent_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

impl CompactionSettings {
    /// Scales reserve and keep-recent budgets proportionally for smaller context windows.
    pub fn for_context_window(context_window: u64, enabled: bool) -> Self {
        let cw = usize::try_from(context_window).unwrap_or(usize::MAX);
        if cw == 0 {
            return Self {
                enabled,
                ..Self::default()
            };
        }
        let reserve_tokens = DEFAULT_RESERVE_TOKENS.min(cw / 5).max(1024);
        let keep_recent_tokens = DEFAULT_KEEP_RECENT_TOKENS.min(cw / 4).max(2048);
        Self {
            enabled,
            reserve_tokens,
            keep_recent_tokens,
        }
    }
}

/// Returns true when auto-compaction is enabled and `context_tokens >= context_window - reserve_tokens`.
pub fn should_auto_compact(
    context_tokens: usize,
    context_window: usize,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled
        && context_window > 0
        && context_tokens >= context_window.saturating_sub(settings.reserve_tokens)
}

/// Estimates total token usage across a slice of conversation messages.
pub fn estimate_context_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// System prompt used for the summarization LLM call.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// Prompt used when generating a fresh summary.
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Prompt used when updating an existing checkpoint summary.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

struct ByteCountWriter(usize);

impl std::io::Write for ByteCountWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCountWriter(0);
    if serde_json::to_writer(&mut counter, value).is_ok() {
        counter.0
    } else {
        0
    }
}

/// Estimated token count for a message using the `chars.div_ceil(4)` heuristic.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let chars = match msg {
        Message::System(s) => s.content.chars().count(),
        Message::User(u) => match &u.content {
            UserContent::Text(t) => t.chars().count(),
            UserContent::Blocks(blocks) => {
                let mut sum = 0;
                for b in blocks {
                    match b {
                        mehh_ai::UserContentBlock::Text(t) => sum += t.text.chars().count(),
                        mehh_ai::UserContentBlock::Image(_) => sum += 4800,
                    }
                }
                sum
            }
        },
        Message::Assistant(a) => {
            let mut sum = 0;
            for b in &a.content {
                match b {
                    AssistantContentBlock::Text(t) => sum += t.text.chars().count(),
                    AssistantContentBlock::Thinking(th) => sum += th.thinking.chars().count(),
                    AssistantContentBlock::ToolCall(tc) => {
                        sum += tc.name.len() + json_serialized_len(&tc.arguments);
                    }
                }
            }
            sum
        }
        Message::ToolResult(tr) => {
            let mut sum = 0;
            for b in &tr.content {
                match b {
                    mehh_ai::ToolResultContentBlock::Text(t) => sum += t.text.chars().count(),
                    mehh_ai::ToolResultContentBlock::Image(_) => sum += 4800,
                }
            }
            // `details` are session metadata only; they are never serialized to the LLM.
            sum
        }
    };
    chars.div_ceil(4)
}

/// Token estimate for tool schemas, which occupy context alongside the conversation.
pub fn estimate_tool_definitions_tokens(tools: &[Arc<dyn AgentTool>]) -> usize {
    tools
        .iter()
        .map(|t| json_serialized_len(&tool_to_definition(t.as_ref())))
        .sum::<usize>()
        .div_ceil(4)
}

/// Projects the entry kinds that `SessionManager::build_context_messages` sends to the LLM.
fn messages_for_entry(entry: &SessionEntry) -> Vec<Message> {
    match &entry.kind {
        SessionEntryKind::Message { message } => vec![message.clone()],
        SessionEntryKind::CustomMessage { content, .. } => vec![Message::User(UserMessage {
            content: content.clone(),
            timestamp: now_ms(),
        })],
        SessionEntryKind::BranchSummary { summary, .. } => vec![Message::User(UserMessage {
            content: UserContent::Text(format!(
                "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{summary}\n</summary>"
            )),
            timestamp: now_ms(),
        })],
        _ => Vec::new(),
    }
}

/// Prepared compaction payload containing the cut point, messages to summarize,
/// token estimates, and touched file lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<Message>,
    pub turn_prefix_messages: Vec<Message>,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Identifies whether a message is an eligible cut point boundary (User or Assistant, never ToolResult).
fn is_cut_point_message(msg: &Message) -> bool {
    matches!(msg, Message::User(_) | Message::Assistant(_))
}

/// Formats a conversation into a serialized text transcript for summarization.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();

    for msg in messages {
        match msg {
            Message::User(u) => {
                let text = u.content.text();
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(a) => {
                let text = a.text_content();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }

                let tool_calls = a.tool_calls();
                if !tool_calls.is_empty() {
                    let calls_str = tool_calls
                        .iter()
                        .map(|tc| {
                            let args_str = tc
                                .arguments
                                .iter()
                                .map(|(k, v)| {
                                    format!("{k}={}", serde_json::to_string(v).unwrap_or_default())
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{}({})", tc.name, args_str)
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    parts.push(format!("[Assistant tool calls]: {calls_str}"));
                }
            }
            Message::ToolResult(tr) => {
                let content_text = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        mehh_ai::ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                        mehh_ai::ToolResultContentBlock::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let mut char_iter = content_text.char_indices();
                let truncated = if let Some((byte_end, _)) = char_iter.nth(TOOL_RESULT_MAX_CHARS) {
                    let remaining = 1 + char_iter.count();
                    format!(
                        "{}\n\n[... {remaining} more characters truncated]",
                        &content_text[..byte_end]
                    )
                } else {
                    content_text
                };

                if !truncated.is_empty() {
                    parts.push(format!("[Tool result]: {truncated}"));
                }
            }
            Message::System(_) => {}
        }
    }

    parts.join("\n\n")
}

/// Evaluates active session entries and prepares a compaction cut point if history exceeds the budget.
pub fn prepare_compaction(
    entries: &[SessionEntry],
    keep_recent_tokens: usize,
) -> Option<CompactionPreparation> {
    if entries.is_empty() {
        return None;
    }

    // Locate latest compaction boundary
    let last_compaction_pos = entries
        .iter()
        .rposition(|e| matches!(&e.kind, SessionEntryKind::Compaction { .. }));

    let (boundary_start, previous_summary) = match last_compaction_pos {
        Some(pos) => {
            if let SessionEntryKind::Compaction { summary, .. } = &entries[pos].kind {
                (pos + 1, Some(summary.clone()))
            } else {
                (0, None)
            }
        }
        None => (0, None),
    };

    if boundary_start >= entries.len() {
        return None;
    }

    // Collect valid cut point candidate indices: Message::User or Message::Assistant
    let mut cut_point_candidates = Vec::new();
    for (i, entry) in entries.iter().enumerate().skip(boundary_start) {
        if messages_for_entry(entry)
            .iter()
            .any(|message| is_cut_point_message(message))
        {
            cut_point_candidates.push(i);
        }
    }

    if cut_point_candidates.is_empty() {
        return None;
    }

    // Calculate total tokens before compaction
    let mut tokens_before: usize = 0;
    if let Some(prev) = &previous_summary {
        tokens_before += prev.len().div_ceil(4);
    }
    for entry in &entries[boundary_start..] {
        tokens_before += messages_for_entry(entry)
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
    }

    // Walk backwards from the tail, accumulating tokens until >= keep_recent_tokens
    let mut accumulated_tokens = 0;
    let mut cut_index = cut_point_candidates[0];
    let mut budget_exceeded = false;

    for i in (boundary_start..entries.len()).rev() {
        for msg in &messages_for_entry(&entries[i]) {
            accumulated_tokens += estimate_message_tokens(msg);
        }
        if accumulated_tokens >= keep_recent_tokens {
            budget_exceeded = true;
            // Snap to the closest candidate at or after index i
            cut_index = cut_point_candidates
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(*cut_point_candidates.last().unwrap());
            break;
        }
    }

    if !budget_exceeded {
        // If the entire history fits within keep_recent_tokens, cut at first message if > 1 message
        if cut_point_candidates.len() <= 1 {
            return None;
        }
        cut_index = cut_point_candidates[0];
    }

    // Check if the cut splits a turn (cut point is Assistant message).
    // Always align kept history on a User message so post-compaction context never starts with Assistant.
    let cut_entry = &entries[cut_index];
    let is_assistant_cut = matches!(
        &cut_entry.kind,
        SessionEntryKind::Message {
            message: Message::Assistant(_)
        }
    );

    let first_kept_index = if is_assistant_cut {
        let mut turn_start = None;
        for i in (boundary_start..=cut_index).rev() {
            if let SessionEntryKind::Message {
                message: Message::User(_),
            } = &entries[i].kind
            {
                turn_start = Some(i);
                break;
            }
        }
        match turn_start {
            Some(ts) if ts > boundary_start => ts,
            _ => entries[cut_index + 1..]
                .iter()
                .position(|e| {
                    matches!(
                        &e.kind,
                        SessionEntryKind::Message {
                            message: Message::User(_)
                        }
                    )
                })
                .map(|rel| cut_index + 1 + rel)?,
        }
    } else {
        cut_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &entries[boundary_start..first_kept_index] {
        messages_to_summarize.extend(messages_for_entry(entry));
    }

    let turn_prefix_messages = Vec::new();

    if messages_to_summarize.is_empty() {
        return None;
    }

    let first_kept_entry = &entries[first_kept_index];

    // Extract file operations from read, write, and edit tool calls
    let mut read_files_set = BTreeSet::new();
    let mut modified_files_set = BTreeSet::new();

    for msg in messages_to_summarize
        .iter()
        .chain(turn_prefix_messages.iter())
    {
        if let Message::Assistant(ast) = msg {
            for call in ast.tool_calls() {
                if let Some(path_val) = call.arguments.get("path").and_then(|v| v.as_str()) {
                    match call.name.as_str() {
                        "read" => {
                            read_files_set.insert(path_val.to_string());
                        }
                        "write" | "edit" => {
                            modified_files_set.insert(path_val.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let read_files: Vec<String> = read_files_set
        .into_iter()
        .filter(|f| !modified_files_set.contains(f))
        .collect();
    let modified_files: Vec<String> = modified_files_set.into_iter().collect();

    Some(CompactionPreparation {
        first_kept_entry_id: first_kept_entry.id.clone(),
        messages_to_summarize,
        turn_prefix_messages,
        tokens_before: tokens_before as u64,
        previous_summary,
        read_files,
        modified_files,
    })
}

/// Calls an LLM provider to produce a structured summary of the prepared messages,
/// appending `<read-files>` and `<modified-files>` metadata blocks.
pub async fn compact_with_provider(
    prep: &CompactionPreparation,
    model: &Model,
    provider: &dyn LlmProvider,
    api_key: Option<&str>,
    custom_instructions: Option<&str>,
    cancel: CancellationToken,
) -> Result<String> {
    let mut serialized_conv = serialize_conversation(&prep.messages_to_summarize);
    // A reactive overflow compaction starts from a context that is already too
    // large. Clamp the summarization payload so the summary request itself
    // cannot overflow the same window.
    let budget_chars = (model.context_window as usize)
        .saturating_sub(8192)
        .saturating_mul(3)
        .max(4096);
    if serialized_conv.chars().count() > budget_chars {
        // Keep the newest tail: a chronological transcript truncated at the head
        // would drop the most recent context while claiming to drop the oldest.
        if let Some((byte_start, _)) = serialized_conv.char_indices().rev().nth(budget_chars - 1) {
            let newest = serialized_conv[byte_start..].to_string();
            serialized_conv =
                format!("[... older history truncated for summarization]\n\n{newest}");
        }
    }

    let mut user_prompt = String::new();
    if let Some(prev) = &prep.previous_summary {
        user_prompt.push_str(&format!(
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        ));
    }

    user_prompt.push_str(&serialized_conv);
    user_prompt.push_str("\n\n");

    if prep.previous_summary.is_some() {
        user_prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        user_prompt.push_str(SUMMARIZATION_PROMPT);
    }

    if let Some(ci) = custom_instructions {
        if !ci.trim().is_empty() {
            user_prompt.push_str(&format!("\n\nCustom Instructions:\n{}", ci.trim()));
        }
    }

    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(user_prompt),
            timestamp: now_ms(),
        })],
        tools: None,
    };

    let options = StreamOptions {
        api_key: api_key.map(|s| s.to_string()),
        ..Default::default()
    };

    let mut stream = provider.stream(model, &context, &options, cancel);
    let final_msg = stream.collect_final_message().await?;

    if matches!(
        final_msg.stop_reason,
        StopReason::Error | StopReason::Aborted
    ) {
        let err = final_msg
            .error_message
            .unwrap_or_else(|| format!("Compaction stopped with {:?}", final_msg.stop_reason));
        return Err(anyhow!("Compaction model stream failed: {err}"));
    }

    let mut summary = final_msg.text_content().trim().to_string();

    if !prep.read_files.is_empty() {
        summary.push_str("\n\n<read-files>\n");
        summary.push_str(&prep.read_files.join("\n"));
        summary.push_str("\n</read-files>");
    }

    if !prep.modified_files.is_empty() {
        summary.push_str("\n\n<modified-files>\n");
        summary.push_str(&prep.modified_files.join("\n"));
        summary.push_str("\n</modified-files>");
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{
        now_ms, AssistantContentBlock, AssistantMessage, FauxProvider, ModelCost, StopReason,
        TextContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
    };

    fn make_test_model() -> Model {
        Model {
            id: "faux-test".to_string(),
            name: "Faux Test".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: "http://localhost".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_prepare_compaction_cut_point_and_file_ops() {
        let mut entries = Vec::new();

        // 1. User message
        entries.push(SessionEntry {
            id: "e1".to_string(),
            parent_id: None,
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Initial user prompt".to_string()),
                    timestamp: now_ms(),
                }),
            },
        });

        // 2. Assistant tool call (read foo.rs and write bar.rs)
        let mut read_args = serde_json::Map::new();
        read_args.insert(
            "path".to_string(),
            serde_json::Value::String("src/foo.rs".to_string()),
        );
        let mut write_args = serde_json::Map::new();
        write_args.insert(
            "path".to_string(),
            serde_json::Value::String("src/bar.rs".to_string()),
        );

        entries.push(SessionEntry {
            id: "e2".to_string(),
            parent_id: Some("e1".to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(AssistantMessage {
                    content: vec![
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_1".to_string(),
                            name: "read".to_string(),
                            arguments: read_args,
                            arguments_error: None,
                            thought_signature: None,
                        }),
                        AssistantContentBlock::ToolCall(ToolCall {
                            id: "call_2".to_string(),
                            name: "write".to_string(),
                            arguments: write_args,
                            arguments_error: None,
                            thought_signature: None,
                        }),
                    ],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            },
        });

        // 3. Tool results (must never be chosen as cut point)
        entries.push(SessionEntry {
            id: "e3".to_string(),
            parent_id: Some("e2".to_string()),
            timestamp: "2026-11-12T10:02:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![mehh_ai::ToolResultContentBlock::Text(TextContent {
                        text: "Contents of foo.rs".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    duration_ms: None,
                    usage: None,
                    timestamp: now_ms(),
                }),
            },
        });

        // 4. User follow-up
        entries.push(SessionEntry {
            id: "e4".to_string(),
            parent_id: Some("e3".to_string()),
            timestamp: "2026-11-12T10:03:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Second turn user question".to_string()),
                    timestamp: now_ms(),
                }),
            },
        });

        // 5. Assistant reply
        entries.push(SessionEntry {
            id: "e5".to_string(),
            parent_id: Some("e4".to_string()),
            timestamp: "2026-11-12T10:04:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "Done with the second turn.".to_string(),
                        text_signature: None,
                    })],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            },
        });

        // Prepare compaction with small keep_recent_tokens so it cuts before e4
        let prep = prepare_compaction(&entries, 10).expect("Should prepare compaction");
        assert_eq!(prep.first_kept_entry_id, "e4");
        assert_eq!(prep.read_files, vec!["src/foo.rs"]);
        assert_eq!(prep.modified_files, vec!["src/bar.rs"]);
        assert!(prep.messages_to_summarize.len() >= 2);
    }

    #[tokio::test]
    async fn compact_with_provider_caps_oversized_history() {
        let mut model = make_test_model();
        model.context_window = 4096;
        let prep = CompactionPreparation {
            first_kept_entry_id: "e2".to_string(),
            messages_to_summarize: vec![Message::User(UserMessage {
                content: UserContent::Text("x".repeat(200_000)),
                timestamp: now_ms(),
            })],
            turn_prefix_messages: Vec::new(),
            tokens_before: 50_000,
            previous_summary: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        };
        let faux = FauxProvider::new();
        faux.add_response(mehh_ai::faux_assistant_message("## Goal\nSmall summary"));
        compact_with_provider(&prep, &model, &faux, None, None, CancellationToken::new())
            .await
            .unwrap();
        let requests = faux.recorded_requests();
        let request = requests.last().expect("compaction request recorded");
        let text = match &request.context.messages[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text prompt"),
            },
            _ => panic!("expected user prompt"),
        };
        assert!(
            text.len() < 20_000,
            "summarization prompt grew to {} chars",
            text.len()
        );
    }

    #[tokio::test]
    async fn compact_with_provider_keeps_newest_history_when_clamped() {
        let mut model = make_test_model();
        model.context_window = 4096;
        let prep = CompactionPreparation {
            first_kept_entry_id: "e2".to_string(),
            messages_to_summarize: vec![
                Message::User(UserMessage {
                    content: UserContent::Text(format!("OLDEST_MARKER{}", "x".repeat(6_000))),
                    timestamp: now_ms(),
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("NEWEST_MARKER".to_string()),
                    timestamp: now_ms(),
                }),
            ],
            turn_prefix_messages: Vec::new(),
            tokens_before: 50_000,
            previous_summary: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        };
        let faux = FauxProvider::new();
        faux.add_response(mehh_ai::faux_assistant_message("## Goal\nSmall summary"));
        compact_with_provider(&prep, &model, &faux, None, None, CancellationToken::new())
            .await
            .unwrap();
        let requests = faux.recorded_requests();
        let text = match &requests.last().expect("compaction request recorded").context.messages[0]
        {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text prompt"),
            },
            _ => panic!("expected user prompt"),
        };
        assert!(text.contains("NEWEST_MARKER"), "newest history must be kept");
        assert!(
            !text.contains("OLDEST_MARKER"),
            "oldest history must be dropped when clamping"
        );
        assert!(text.contains("older history truncated for summarization"));
    }

    #[test]
    fn estimate_message_tokens_uses_chars_not_utf8_bytes() {
        let msg = Message::User(UserMessage {
            content: UserContent::Text("é".repeat(400)),
            timestamp: now_ms(),
        });
        assert_eq!(estimate_message_tokens(&msg), 100);
    }

    #[tokio::test]
    async fn test_compact_with_provider_faux() {
        let prep = CompactionPreparation {
            first_kept_entry_id: "entry_123".to_string(),
            messages_to_summarize: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("Build me a parser".to_string()),
                    timestamp: now_ms(),
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantContentBlock::Text(TextContent {
                        text: "I built the parser".to_string(),
                        text_signature: None,
                    })],
                    api: "faux".to_string(),
                    provider: "faux".to_string(),
                    model: "faux-test".to_string(),
                    response_id: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: now_ms(),
                }),
            ],
            turn_prefix_messages: vec![],
            tokens_before: 500,
            previous_summary: None,
            read_files: vec!["parser.rs".to_string()],
            modified_files: vec!["main.rs".to_string()],
        };

        let faux = FauxProvider::new();
        faux.add_response(AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: "## Goal\nBuild a parser.\n\n## Progress\n### Done\n- [x] Parser built"
                    .to_string(),
                text_signature: None,
            })],
            api: "faux".to_string(),
            provider: "faux".to_string(),
            model: "faux-test".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        });

        let model = make_test_model();
        let cancel = CancellationToken::new();

        let result =
            compact_with_provider(&prep, &model, &faux, None, Some("Keep it brief"), cancel)
                .await
                .unwrap();

        assert!(result.contains("## Goal\nBuild a parser."));
        assert!(result.contains("<read-files>\nparser.rs\n</read-files>"));
        assert!(result.contains("<modified-files>\nmain.rs\n</modified-files>"));
    }

    #[test]
    fn test_compaction_non_ascii_truncation_and_assistant_cut_starts_with_user() {
        // Non-ASCII tool output around byte 2000 must not panic when serialized for compaction.
        let mut big = "a".repeat(TOOL_RESULT_MAX_CHARS - 2);
        big.push_str("你好世界🙂extra");
        let serialized =
            serialize_conversation(&[Message::ToolResult(mehh_ai::ToolResultMessage {
                tool_call_id: "c1".to_string(),
                tool_name: "read".to_string(),
                content: vec![mehh_ai::ToolResultContentBlock::Text(
                    mehh_ai::TextContent {
                        text: big,
                        text_signature: None,
                    },
                )],
                details: None,
                is_error: false,
                duration_ms: None,
                usage: None,
                timestamp: now_ms(),
            })]);
        assert!(serialized.contains("more characters truncated"));

        // Cutting on an assistant entry mid-turn must anchor first_kept_entry_id on that turn's User entry.
        let long_text = "word ".repeat(12_000);
        let mk_user = |id: &str, parent: Option<&str>, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: now_ms(),
                }),
            },
        };
        let mk_ast = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(mehh_ai::faux_assistant_message(text)),
            },
        };

        let entries = vec![
            mk_user("u1", None, "first turn"),
            mk_ast("a1", "u1", "first reply"),
            mk_user("u2", Some("a1"), "second turn question"),
            mk_ast("a2", "u2", &long_text),
            mk_ast("a3", "a2", &long_text),
        ];

        let prep = prepare_compaction(&entries, DEFAULT_KEEP_RECENT_TOKENS).unwrap();
        assert_eq!(prep.first_kept_entry_id, "u2");
        assert_eq!(prep.messages_to_summarize.len(), 2);
        assert!(prep.turn_prefix_messages.is_empty());
    }

    #[test]
    fn test_compaction_cut_splits_turn_advances_to_next_user_when_turn_start_is_boundary() {
        let mk_user = |id: &str, parent: Option<&str>, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-11-12T10:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: now_ms(),
                }),
            },
        };
        let mk_ast = |id: &str, parent: &str, text: &str| SessionEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: "2026-11-12T10:01:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::Assistant(mehh_ai::faux_assistant_message(text)),
            },
        };

        let long_reply = "A".repeat(4000);
        let entries = vec![
            mk_user("u1", None, "initial prompt"),
            mk_ast("a1", "u1", &long_reply),
            mk_user("u2", Some("a1"), "follow-up prompt"),
            mk_ast("a2", "u2", "short reply"),
        ];

        // Budget is small (50 tokens) so walking backwards from a2 reaches a1 and exceeds budget.
        // a1 is an assistant message whose turn_start is u1 (index 0 == boundary_start).
        // It must advance to u2 instead of returning None.
        let prep = prepare_compaction(&entries, 50)
            .expect("Compaction should succeed by advancing to next User turn");
        assert_eq!(prep.first_kept_entry_id, "u2");
        assert_eq!(prep.messages_to_summarize.len(), 2);
    }

    #[test]
    fn test_compaction_settings_and_should_auto_compact() {
        let small = CompactionSettings::for_context_window(8_000, true);
        assert!(small.reserve_tokens <= 1600);
        assert!(small.keep_recent_tokens >= 2048);

        let default_settings = CompactionSettings::default();
        assert!(default_settings.enabled);
        assert!(should_auto_compact(190_000, 200_000, &default_settings));
        assert!(!should_auto_compact(100_000, 200_000, &default_settings));
        assert!(!should_auto_compact(
            190_000,
            200_000,
            &CompactionSettings {
                enabled: false,
                ..default_settings
            }
        ));
    }
}
`````

