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
        return (map, clean);
    }

    let after_start = &trimmed[3..];
    let after_nl = if let Some(stripped) = after_start.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = after_start.strip_prefix('\n') {
        stripped
    } else {
        return (map, clean);
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
        None => return (map, clean),
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

const MAX_SKILL_SCAN_DEPTH: usize = 32;
pub(crate) const MAX_MARKDOWN_FILE_BYTES: usize = 512 * 1024;

/// Reads a UTF-8 markdown file up to `max_bytes` and strips any leading UTF-8 BOM.
pub(crate) fn read_bounded_utf8_file(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max_bytes as u64 {
        return None;
    }
    let mut buf = Vec::with_capacity((meta.len() as usize).min(max_bytes));
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > max_bytes {
        return None;
    }
    let text = String::from_utf8(buf).ok()?;
    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Returns true for symlinks and other reparse points (e.g. Windows junctions).
/// Following them would let a link cycle recurse forever, so they are skipped.
fn is_link_entry(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return true;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
        // Fail closed: never follow an entry we cannot inspect.
        Err(_) => true,
    }
}

/// Recursively scans a directory for `SKILL.md` files, ignoring hidden dirs, `node_modules`, and `target`.
/// Linked directories are never followed, recursion is depth-bounded, and symlinked `SKILL.md` files
/// must remain contained within the canonicalized skill root.
fn scan_dir_for_skills(dir: &Path, out: &mut Vec<Skill>) {
    let Ok(canon_root) = std::fs::canonicalize(dir) else {
        return;
    };
    if !canon_root.is_dir() {
        return;
    }
    scan_dir_for_skills_inner(dir, &canon_root, out, 0);
}

fn scan_dir_for_skills_inner(dir: &Path, canon_root: &Path, out: &mut Vec<Skill>, depth: usize) {
    if depth > MAX_SKILL_SCAN_DEPTH || !dir.is_dir() {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let skip_dir_name =
            name_str.starts_with('.') || name_str == "node_modules" || name_str == "target";
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() || ft.is_symlink() {
            if !ft.is_symlink() {
                if skip_dir_name {
                    continue;
                }
                let path = entry.path();
                if is_link_entry(&path) {
                    continue;
                }
                let Ok(canon_dir) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canon_dir.starts_with(canon_root) {
                    continue;
                }
                scan_dir_for_skills_inner(&path, canon_root, out, depth + 1);
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                // Only a linked directory can create a scan cycle. A linked
                // SKILL.md is a leaf; skipping it loses legitimate skills (for
                // example a SKILL.md symlinked within the same skill root).
                continue;
            }
        }
        if name_str.eq_ignore_ascii_case("skill.md") {
            let path = entry.path();
            if path.is_file() {
                let Ok(canon_file) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canon_file.starts_with(canon_root) || !canon_file.is_file() {
                    continue;
                }
                if let Some(content) = read_bounded_utf8_file(&canon_file, MAX_MARKDOWN_FILE_BYTES)
                {
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

/// Loads skills from global directory `~/.mehh/agent/skills` and project directory `<cwd>/.mehh/skills`.
/// Project skills override global skills with the same name.
/// Loads global skills and, when `trusted` is true, project-local skills under `.agents/skills` and `.mehh/skills`.
pub fn load_skills_with_trust(cwd: &Path, trusted: bool) -> Vec<Skill> {
    let mut skill_map: HashMap<String, Skill> = HashMap::new();

    // 1. Global skills
    if let Some(home_path) = crate::user_home_dir() {
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

pub(crate) fn escape_xml(s: &str) -> String {
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
    let canon_file = std::fs::canonicalize(&skill.file_path).ok()?;
    let content = read_bounded_utf8_file(&canon_file, MAX_MARKDOWN_FILE_BYTES)?;
    let (_, body) = parse_frontmatter(&content);

    let norm_file = skill.file_path.to_string_lossy().replace('\\', "/");
    let norm_base = skill.base_dir.to_string_lossy().replace('\\', "/");

    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        escape_xml(&skill.name),
        escape_xml(&norm_file),
        escape_xml(&norm_base),
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
    fn test_expand_skill_command_escapes_name_and_paths() {
        let dir = std::env::temp_dir().join(format!("mehh_skill_escape_{}", Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let skill_file = dir.join("SKILL.md");
        fs::write(&skill_file, "---\ndescription: d\n---\nBody").unwrap();

        let skills = vec![Skill {
            name: "x\"on_error=\"bad".to_string(),
            description: String::new(),
            file_path: skill_file,
            base_dir: dir.join("base_<weird>"),
            disable_model_invocation: false,
        }];

        let expanded = expand_skill_command("/skill:x\"on_error=\"bad", &skills).unwrap();
        assert!(
            expanded.contains("name=\"x&quot;on_error=&quot;bad\""),
            "raw attribute breakout: {expanded}"
        );
        assert!(
            expanded.contains("&lt;weird&gt;"),
            "raw '<' in location: {expanded}"
        );
        assert!(!expanded.contains("on_error=\"bad\""), "raw breakout: {expanded}");

        let _ = fs::remove_dir_all(dir);
    }

    fn create_dir_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .expect("run mklink /J");
            assert!(output.status.success(), "mklink /J failed: {output:?}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).expect("create symlink");
        }
    }

    #[test]
    fn test_scan_dir_for_skills_skips_links_and_loops() {
        let dir = std::env::temp_dir().join(format!("mehh_skill_loop_{}", Uuid::now_v7()));
        let skills_root = dir.join(".agents").join("skills");
        let real_skill = skills_root.join("real-skill");
        fs::create_dir_all(&real_skill).unwrap();
        fs::write(
            real_skill.join("SKILL.md"),
            "---\nname: loop-skill\ndescription: Project skill\n---\nBody",
        )
        .unwrap();

        // A link back to the scanned root forms a cycle; a link to the skill
        // directory makes the same SKILL.md reachable twice.
        create_dir_link(&skills_root, &skills_root.join("loop"));
        create_dir_link(&real_skill, &skills_root.join("alias"));

        let mut found = Vec::new();
        scan_dir_for_skills(&skills_root, &mut found);

        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["loop-skill"],
            "links/loops must be skipped; got {names:?}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    // On Windows a file symlink needs SeCreateSymbolicLinkPrivilege, which is
    // unavailable in the test environment (verified: `mklink` exits 1). The
    // behavior is platform-independent, so exercise it wherever symlinks work.
    #[cfg(unix)]
    #[test]
    fn test_scan_dir_for_skills_enforces_symlink_containment_within_root() {
        let dir = std::env::temp_dir().join(format!("mehh_skill_file_link_{}", Uuid::now_v7()));
        let skills_root = dir.join(".agents").join("skills");
        let outside_dir = dir.join("dotfiles");
        let escaped_dir = skills_root.join("escaped-skill");
        let shared_dir = skills_root.join("shared");
        let in_root_dir = skills_root.join("in-root-skill");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::create_dir_all(&escaped_dir).unwrap();
        fs::create_dir_all(&shared_dir).unwrap();
        fs::create_dir_all(&in_root_dir).unwrap();

        let outside_skill = outside_dir.join("SKILL.md");
        fs::write(
            &outside_skill,
            "---\nname: escaped-skill\ndescription: from outside\n---\nSecret",
        )
        .unwrap();
        let shared_skill = shared_dir.join("SKILL.md");
        fs::write(
            &shared_skill,
            "---\nname: shared-skill\ndescription: inside root\n---\nBody",
        )
        .unwrap();

        std::os::unix::fs::symlink(&outside_skill, escaped_dir.join("SKILL.md")).unwrap();
        std::os::unix::fs::symlink(&shared_skill, in_root_dir.join("SKILL.md")).unwrap();

        let mut found = Vec::new();
        scan_dir_for_skills(&skills_root, &mut found);
        assert!(
            !found.iter().any(|s| s.name == "escaped-skill"),
            "symlinked SKILL.md escaping root must be rejected, got {found:?}"
        );
        assert!(
            found.iter().any(|s| s.name == "shared-skill"),
            "symlinked SKILL.md inside root should still load, got {found:?}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_load_skills_respects_trust_strips_bom_and_bounds_reads() {
        let dir = std::env::temp_dir().join(format!("mehh_skill_trust_{}", Uuid::now_v7()));
        let skill_dir = dir.join(".agents").join("skills").join("proj-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "\u{feff}---\nname: proj-skill\ndescription: Project skill\n---\nBody text",
        )
        .unwrap();

        let oversized_dir = dir.join(".agents").join("skills").join("huge-skill");
        fs::create_dir_all(&oversized_dir).unwrap();
        let huge_body = "x".repeat(MAX_MARKDOWN_FILE_BYTES + 1024);
        fs::write(oversized_dir.join("SKILL.md"), huge_body).unwrap();

        // Untrusted project must NOT load repo-local skills
        let untrusted = load_skills_with_trust(&dir, false);
        assert!(!untrusted.iter().any(|s| s.name == "proj-skill"));

        // Trusted project loads .agents/skills, strips UTF-8 BOM, and skips oversized files
        let trusted = load_skills_with_trust(&dir, true);
        let found = trusted.iter().find(|s| s.name == "proj-skill").unwrap();
        assert_eq!(found.description, "Project skill");
        assert!(!trusted.iter().any(|s| s.name == "huge-skill"));

        // parse_frontmatter without frontmatter still strips leading UTF-8 BOM
        let (_, body_no_fm) = parse_frontmatter("\u{feff}# Heading");
        assert_eq!(body_no_fm, "# Heading");

        let _ = fs::remove_dir_all(dir);
    }
}
