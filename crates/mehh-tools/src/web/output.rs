//! Result formatting, head+tail truncation, and temp-file spill for web tools.

use std::fmt::Write as _;
use std::path::PathBuf;

use uuid::Uuid;

use super::types::SearchHit;

pub const FETCH_INLINE_MAX_CHARS: usize = 20_000;
pub const FETCH_HEAD_CHARS: usize = 15_000;
pub const FETCH_TAIL_CHARS: usize = 5_000;
pub const SEARCH_INLINE_MAX_CHARS: usize = 16_000;
pub const NO_SEARCH_RESULTS: &str = "No search results found. Please try a different query.";
const SPILL_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const SPILL_MAX_FILES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedOutput {
    pub text: String,
    pub truncated: bool,
    pub spilled_path: Option<PathBuf>,
}

/// Formats search hits into OpenCode-style Markdown blocks, capping inline length on UTF-8 char boundaries.
pub fn format_search_results(hits: &[SearchHit]) -> FormattedOutput {
    if hits.is_empty() {
        return FormattedOutput {
            text: NO_SEARCH_RESULTS.to_string(),
            truncated: false,
            spilled_path: None,
        };
    }

    let estimated_bytes: usize = hits
        .iter()
        .map(|h| {
            h.url.len()
                + h.title.as_deref().map_or(h.url.len(), str::len)
                + h.published.as_deref().map_or(0, |p| p.len() + 12)
                + h.content.as_deref().map_or(0, |c| c.len() + 4)
                + 16
        })
        .sum();
    let mut full = String::with_capacity(estimated_bytes);
    for (idx, hit) in hits.iter().enumerate() {
        if idx > 0 {
            full.push_str("\n\n");
        }
        let title = hit.title.as_deref().unwrap_or(&hit.url);
        full.push_str("## [");
        full.push_str(title);
        full.push_str("](");
        full.push_str(&hit.url);
        full.push(')');
        if let Some(ref published) = hit.published {
            full.push_str("\nPublished: ");
            full.push_str(published);
        }
        if let Some(ref content) = hit.content {
            full.push_str("\n\n");
            full.push_str(content);
        }
    }

    if full.len() <= SEARCH_INLINE_MAX_CHARS {
        return FormattedOutput {
            text: full,
            truncated: false,
            spilled_path: None,
        };
    }

    let total_chars = full.chars().count();
    if total_chars <= SEARCH_INLINE_MAX_CHARS {
        return FormattedOutput {
            text: full,
            truncated: false,
            spilled_path: None,
        };
    }

    let cut = byte_index_at_char(full.as_str(), SEARCH_INLINE_MAX_CHARS);
    full.truncate(cut);
    let _ = write!(
        full,
        "\n\n[Search output truncated at {SEARCH_INLINE_MAX_CHARS} of {total_chars} chars]"
    );
    FormattedOutput {
        text: full,
        truncated: true,
        spilled_path: None,
    }
}

fn format_truncated_fetch_output(
    content: &str,
    total_chars: usize,
    spilled_path: PathBuf,
) -> FormattedOutput {
    let head_end = byte_index_at_char(content, FETCH_HEAD_CHARS);
    let tail_start = byte_index_at_char_from_end(content, FETCH_TAIL_CHARS);
    let display_path = spilled_path.to_string_lossy().replace('\\', "/");

    let mut text = String::with_capacity(head_end + (content.len() - tail_start) + 256);
    text.push_str(&content[..head_end]);
    text.push_str("\n\n... [middle omitted] ...\n\n");
    text.push_str(&content[tail_start..]);
    let _ = write!(
        text,
        "\n\n[Output truncated: showing head and tail of {total_chars} chars. Full content saved to {display_path} — use read with offset/limit to inspect specific sections.]"
    );

    FormattedOutput {
        text,
        truncated: true,
        spilled_path: Some(spilled_path),
    }
}

/// Formats fetched page text; if over 20,000 chars, keeps 15k head + 5k tail and spills full UTF-8 to disk.
pub fn format_fetch_content(content: &str, extension: &str) -> Result<FormattedOutput, String> {
    if content.len() <= FETCH_INLINE_MAX_CHARS {
        return Ok(FormattedOutput {
            text: content.to_string(),
            truncated: false,
            spilled_path: None,
        });
    }
    let total_chars = content.chars().count();
    if total_chars <= FETCH_INLINE_MAX_CHARS {
        return Ok(FormattedOutput {
            text: content.to_string(),
            truncated: false,
            spilled_path: None,
        });
    }

    let spilled_path = write_spill_file(content, extension)?;
    Ok(format_truncated_fetch_output(
        content,
        total_chars,
        spilled_path,
    ))
}

/// Async version of [`format_fetch_content`] that offloads disk spill I/O to a blocking worker thread.
pub async fn format_fetch_content_async(
    content: String,
    extension: String,
) -> Result<FormattedOutput, String> {
    if content.len() <= FETCH_INLINE_MAX_CHARS {
        return Ok(FormattedOutput {
            text: content,
            truncated: false,
            spilled_path: None,
        });
    }

    tokio::task::spawn_blocking(move || {
        let total_chars = content.chars().count();
        if total_chars <= FETCH_INLINE_MAX_CHARS {
            return Ok(FormattedOutput {
                text: content,
                truncated: false,
                spilled_path: None,
            });
        }
        let spilled_path = write_spill_file(&content, &extension)?;
        Ok(format_truncated_fetch_output(
            &content,
            total_chars,
            spilled_path,
        ))
    })
    .await
    .map_err(|e| format!("Failed joining web fetch spill task: {e}"))?
}

fn byte_index_at_char(s: &str, char_count: usize) -> usize {
    s.char_indices()
        .nth(char_count)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

fn byte_index_at_char_from_end(s: &str, tail_chars: usize) -> usize {
    if tail_chars == 0 {
        return s.len();
    }
    s.char_indices()
        .rev()
        .nth(tail_chars - 1)
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

#[cfg(unix)]
fn unix_spill_dir() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(runtime_dir).join("mehh-web-fetch");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home)
            .join(".cache")
            .join("mehh")
            .join("web-fetch");
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| std::process::id().to_string());
    let clean_user: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("mehh-web-fetch-{clean_user}"))
}

fn prune_spill_dir(dir: &std::path::Path, preserve: Option<&std::path::Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut remaining: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if preserve.is_some_and(|p| {
            p == path || p.file_name() == Some(entry.file_name().as_os_str())
        }) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).is_ok_and(|age| age > SPILL_MAX_AGE) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        remaining.push((path, modified));
    }

    let max_keep = SPILL_MAX_FILES.saturating_sub(1);
    if remaining.len() > max_keep {
        remaining.sort_by_key(|(_, modified)| *modified);
        let to_remove = remaining.len() - max_keep;
        for (path, _) in remaining.into_iter().take(to_remove) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn write_spill_file(content: &str, extension: &str) -> Result<PathBuf, String> {
    let clean_ext: String = extension
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let ext = if clean_ext.is_empty() {
        "txt"
    } else {
        clean_ext.as_str()
    };

    #[cfg(unix)]
    let path = {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

        let dir = unix_spill_dir();
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
        {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!("Failed creating web fetch spill directory: {e}"));
            }
        }
        let dir_meta = std::fs::symlink_metadata(&dir)
            .map_err(|e| format!("Failed inspecting web fetch spill directory: {e}"))?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err("Refusing web fetch spill directory: not a regular directory".to_string());
        }

        let path = dir.join(format!("{}.{ext}", Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Failed creating web fetch spill file: {e}"))?;
        let file_meta = file
            .metadata()
            .map_err(|e| format!("Failed reading spill file metadata: {e}"))?;
        if dir_meta.uid() != file_meta.uid() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("Refusing web fetch spill directory owned by another user".to_string());
        }
        if (dir_meta.mode() & 0o077) != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("Failed securing web fetch spill directory: {e}"))?;
        }
        if let Err(e) = file.write_all(content.as_bytes()) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(format!("Failed writing web fetch spill file: {e}"));
        }
        drop(file);
        prune_spill_dir(&dir, Some(&path));
        path
    };

    #[cfg(not(unix))]
    let path = {
        use std::io::Write;
        let dir = std::env::temp_dir().join("mehh-web-fetch");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed creating web fetch spill directory: {e}"))?;
        let dir_meta = std::fs::symlink_metadata(&dir)
            .map_err(|e| format!("Failed inspecting web fetch spill directory: {e}"))?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err("Refusing web fetch spill directory: not a regular directory".to_string());
        }

        prune_spill_dir(&dir, None);

        let path = dir.join(format!("{}.{ext}", Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Failed creating web fetch spill file: {e}"))?;
        if let Err(e) = file.write_all(content.as_bytes()) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(format!("Failed writing web fetch spill file: {e}"));
        }
        path
    };

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReadTool;
    use mehh_agent::AgentTool;
    use mehh_ai::ToolResultContentBlock;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_format_search_results_empty_and_markdown() {
        let empty = format_search_results(&[]);
        assert_eq!(empty.text, NO_SEARCH_RESULTS);
        assert!(!empty.truncated);

        let hits = vec![SearchHit {
            url: "https://docs.rs/reqwest".into(),
            title: Some("Reqwest".into()),
            published: Some("2026-01-01".into()),
            content: Some("ế".repeat(SEARCH_INLINE_MAX_CHARS + 200)),
        }];
        let out = format_search_results(&hits);
        assert!(out.truncated);
        assert!(out.text.starts_with("## [Reqwest](https://docs.rs/reqwest)\nPublished: 2026-01-01"));
        assert!(out.text.contains("Search output truncated at 16000"));
    }

    #[tokio::test]
    async fn test_format_fetch_content_head_tail_and_spill() {
        let head_line = format!("{}\n", "đ".repeat(96)); // 97 chars, 193 bytes
        let mid_line = format!("{}\n", "á".repeat(96)); // 97 chars, 193 bytes
        let tail_line = format!("{}\n", "\u{1F680}".repeat(96)); // 97 chars, 385 bytes
        let full = format!(
            "{}{}{}",
            head_line.repeat(155),
            mid_line.repeat(105),
            tail_line.repeat(55)
        );
        assert_eq!(full.chars().count(), 30_555);

        let formatted = format_fetch_content(&full, "md").unwrap();
        assert!(formatted.truncated);
        let spilled = formatted.spilled_path.clone().unwrap();
        assert!(formatted.text.starts_with(&head_line));
        assert!(formatted.text.contains("Full content saved to"));
        assert!(formatted.text.contains(&tail_line));
        assert_eq!(std::fs::read_to_string(&spilled).unwrap(), full);

        let parent_dir = spilled.parent().unwrap().to_path_buf();
        let file_name = spilled.file_name().unwrap().to_string_lossy().to_string();
        let read_tool = ReadTool::new(parent_dir);
        let mut params = serde_json::Map::new();
        params.insert("path".into(), serde_json::json!(file_name));
        params.insert("limit".into(), serde_json::json!(400));

        let res = read_tool
            .execute("tc_read", params, CancellationToken::new(), None)
            .await
            .unwrap();
        let read_back = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(read_back.contains(head_line.trim_end()));
        assert!(read_back.contains(mid_line.trim_end()));
        assert!(read_back.contains(tail_line.trim_end()));
        let _ = std::fs::remove_file(spilled);
    }

    #[test]
    fn test_prune_spill_dir_caps_file_count() {
        let dir = crate::TempTestDir::new("spill_prune");
        for i in 0..(SPILL_MAX_FILES + 15) {
            std::fs::write(dir.path().join(format!("spill_{i:04}.txt")), "x").unwrap();
        }
        prune_spill_dir(dir.path(), None);
        let remaining = std::fs::read_dir(dir.path()).unwrap().count();
        assert!(
            remaining < SPILL_MAX_FILES,
            "expected < {SPILL_MAX_FILES} files after prune, got {remaining}"
        );
    }

    #[test]
    fn test_prune_spill_dir_preserves_target_with_identical_mtime() {
        let dir = crate::TempTestDir::new("spill_preserve");
        let newest = dir.path().join("0000_newest.txt");
        std::fs::write(&newest, "keep").unwrap();
        for i in 0..(SPILL_MAX_FILES + 15) {
            std::fs::write(dir.path().join(format!("spill_{i:04}.txt")), "x").unwrap();
        }
        prune_spill_dir(dir.path(), Some(&newest));
        assert!(
            newest.exists(),
            "preserved spill file must never be pruned even with identical mtime"
        );
        let remaining = std::fs::read_dir(dir.path()).unwrap().count();
        assert!(
            remaining <= SPILL_MAX_FILES,
            "expected <= {SPILL_MAX_FILES} total files including preserved, got {remaining}"
        );
    }
}
