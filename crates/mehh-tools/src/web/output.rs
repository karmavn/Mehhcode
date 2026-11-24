//! Result formatting, head+tail truncation, and temp-file spill for web tools.

use std::path::PathBuf;

use uuid::Uuid;

use super::types::SearchHit;

pub const FETCH_INLINE_MAX_CHARS: usize = 20_000;
pub const FETCH_HEAD_CHARS: usize = 15_000;
pub const FETCH_TAIL_CHARS: usize = 5_000;
pub const SEARCH_INLINE_MAX_CHARS: usize = 16_000;
pub const NO_SEARCH_RESULTS: &str = "No search results found. Please try a different query.";

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

    let blocks: Vec<String> = hits
        .iter()
        .map(|hit| {
            let title = hit.title.as_deref().unwrap_or(&hit.url);
            let mut block = format!("## [{title}]({})", hit.url);
            if let Some(ref published) = hit.published {
                block.push_str(&format!("\nPublished: {published}"));
            }
            if let Some(ref content) = hit.content {
                block.push_str(&format!("\n\n{content}"));
            }
            block
        })
        .collect();
    let full = blocks.join("\n\n");

    let total_chars = full.chars().count();
    if total_chars <= SEARCH_INLINE_MAX_CHARS {
        return FormattedOutput {
            text: full,
            truncated: false,
            spilled_path: None,
        };
    }

    let cut = byte_index_at_char(full.as_str(), SEARCH_INLINE_MAX_CHARS);
    let mut text = full[..cut].to_string();
    text.push_str(&format!(
        "\n\n[Search output truncated at {SEARCH_INLINE_MAX_CHARS} of {total_chars} chars]"
    ));
    FormattedOutput {
        text,
        truncated: true,
        spilled_path: None,
    }
}

/// Formats fetched page text; if over 20,000 chars, keeps 15k head + 5k tail and spills full UTF-8 to disk.
pub fn format_fetch_content(content: &str, extension: &str) -> Result<FormattedOutput, String> {
    let total_chars = content.chars().count();
    if total_chars <= FETCH_INLINE_MAX_CHARS {
        return Ok(FormattedOutput {
            text: content.to_string(),
            truncated: false,
            spilled_path: None,
        });
    }

    let spilled_path = write_spill_file(content, extension)?;
    let head_end = byte_index_at_char(content, FETCH_HEAD_CHARS);
    let tail_start = byte_index_at_char_from_end(content, FETCH_TAIL_CHARS);
    let display_path = spilled_path.to_string_lossy().replace('\\', "/");

    let mut text = String::with_capacity(head_end + (content.len() - tail_start) + 256);
    text.push_str(&content[..head_end]);
    text.push_str("\n\n... [middle omitted] ...\n\n");
    text.push_str(&content[tail_start..]);
    text.push_str(&format!(
        "\n\n[Output truncated: showing head and tail of {total_chars} chars. Full content saved to {display_path} — use read with offset/limit to inspect specific sections.]"
    ));

    Ok(FormattedOutput {
        text,
        truncated: true,
        spilled_path: Some(spilled_path),
    })
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

    let dir = std::env::temp_dir().join("mehh-web-fetch");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed creating web fetch spill directory: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = dir.join(format!("{}.{ext}", Uuid::new_v4()));

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Failed creating web fetch spill file: {e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Failed writing web fetch spill file: {e}"))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(&path, content.as_bytes())
            .map_err(|e| format!("Failed writing web fetch spill file: {e}"))?;
    }

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
        let tail_line = format!("{}\n", "🚀".repeat(96)); // 97 chars, 385 bytes
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
}
