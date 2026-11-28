//! `@file` path completion helpers for the interactive editor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

const MAX_WALKED_FILES: usize = 5_000;
const WALK_CACHE_TTL: Duration = Duration::from_secs(15);

struct CachedWalkEntry {
    rel_str: String,
    rel_lower: String,
    file_name_lower: String,
}

struct CachedWalk {
    built_at: Instant,
    entries: std::sync::Arc<Vec<CachedWalkEntry>>,
}

fn walk_cache() -> &'static Mutex<HashMap<PathBuf, CachedWalk>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedWalk>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn normalize_cwd_key(cwd: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    #[cfg(windows)]
    {
        PathBuf::from(resolved.to_string_lossy().to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        resolved
    }
}

fn build_walk_entries(cwd: &Path) -> Vec<CachedWalkEntry> {
    let walker = WalkBuilder::new(cwd)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name().to_str() != Some(".git"))
        .build();

    let mut entries = Vec::new();
    let mut seen = 0usize;

    for entry in walker.flatten() {
        seen += 1;
        if seen > MAX_WALKED_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() {
            continue;
        }
        let rel_lower = rel_str.to_ascii_lowercase();
        let file_name_lower = entry
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        entries.push(CachedWalkEntry {
            rel_str,
            rel_lower,
            file_name_lower,
        });
    }

    entries.sort_by(|a, b| a.rel_str.cmp(&b.rel_str));
    entries
}

/// Extracted `@<prefix>` query anchored at a char index on the active editor line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtFileQuery {
    /// Character index of the `@` sigil on the active line.
    pub replace_start: usize,
    /// Query substring typed after `@` up to the cursor.
    pub query: String,
}

/// Extracts an active `@file` completion query ending at `cursor_col` (char index) on `line`.
/// Returns `None` when `@` is mid-word (e.g. an email address `user@example.com`).
pub fn extract_at_query(line: &str, cursor_col: usize) -> Option<AtFileQuery> {
    if cursor_col == 0 || !line.contains('@') {
        return None;
    }

    let mut prev_ch: Option<char> = None;
    let mut last_at: Option<(usize, usize, bool)> = None;
    let mut cursor_byte_end = 0usize;

    for (char_idx, (byte_idx, ch)) in line.char_indices().take(cursor_col).enumerate() {
        cursor_byte_end = byte_idx + ch.len_utf8();
        if ch == '@' {
            let boundary_ok =
                char_idx == 0 || matches!(prev_ch, Some(' ' | '\t' | '(' | '[' | '"' | '\''));
            last_at = Some((char_idx, cursor_byte_end, boundary_ok));
        } else if ch.is_whitespace() {
            last_at = None;
        }
        prev_ch = Some(ch);
    }

    let (at_idx, query_start_byte, boundary_ok) = last_at?;
    if !boundary_ok {
        return None;
    }
    let query = line.get(query_start_byte..cursor_byte_end)?.to_string();
    Some(AtFileQuery {
        replace_start: at_idx,
        query,
    })
}

/// Completes workspace-relative file paths matching `prefix`, honoring `.gitignore` and skipping `.git`.
pub fn complete_file_paths(cwd: &Path, prefix: &str, limit: usize) -> Vec<String> {
    if limit == 0 || !cwd.is_dir() {
        return Vec::new();
    }

    let query_lower = prefix.replace('\\', "/").to_ascii_lowercase();
    let cache_key = normalize_cwd_key(cwd);
    let now = Instant::now();

    let cached_entries = {
        let cache = walk_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(&cache_key)
            .filter(|entry| now.saturating_duration_since(entry.built_at) < WALK_CACHE_TTL)
            .map(|entry| std::sync::Arc::clone(&entry.entries))
    };

    let entries = match cached_entries {
        Some(entries) => entries,
        None => {
            let built = std::sync::Arc::new(build_walk_entries(cwd));
            let mut cache = walk_cache().lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() > 32 {
                cache.retain(|_, v| now.saturating_duration_since(v.built_at) < WALK_CACHE_TTL);
            }
            cache.insert(
                cache_key,
                CachedWalk {
                    built_at: Instant::now(),
                    entries: std::sync::Arc::clone(&built),
                },
            );
            built
        }
    };

    let mut prefix_matches = Vec::with_capacity(limit);
    let mut substring_matches = Vec::with_capacity(limit);

    if query_lower.is_empty() {
        for entry in entries.iter().take(limit) {
            prefix_matches.push(entry.rel_str.clone());
        }
        return prefix_matches;
    }

    for entry in entries.iter() {
        if entry.rel_lower.starts_with(&query_lower)
            || entry.file_name_lower.starts_with(&query_lower)
        {
            prefix_matches.push(entry.rel_str.clone());
            if prefix_matches.len() >= limit {
                break;
            }
        } else if substring_matches.len() < limit && entry.rel_lower.contains(&query_lower) {
            substring_matches.push(entry.rel_str.clone());
        }
    }

    if prefix_matches.len() < limit {
        let need = limit - prefix_matches.len();
        prefix_matches.extend(substring_matches.into_iter().take(need));
    }
    prefix_matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_at_query_basics_and_multibyte() {
        assert_eq!(
            extract_at_query("inspect @src/ma", 15),
            Some(AtFileQuery {
                replace_start: 8,
                query: "src/ma".to_string(),
            })
        );
        assert_eq!(
            extract_at_query("@", 1),
            Some(AtFileQuery {
                replace_start: 0,
                query: String::new(),
            })
        );
        // Email address mid-token must not trigger file completion
        assert_eq!(extract_at_query("mail user@host.com", 18), None);
        // Multibyte prefix before '@'
        assert_eq!(
            extract_at_query("xem @tệp", 8),
            Some(AtFileQuery {
                replace_start: 4,
                query: "tệp".to_string(),
            })
        );
        // No '@' fast path, trailing whitespace, mid-token double '@'
        assert_eq!(extract_at_query("plain code line", 10), None);
        assert_eq!(extract_at_query("@src ", 5), None);
        assert_eq!(extract_at_query("@foo@bar", 8), None);
        assert_eq!(
            extract_at_query("mail user@host.com @src", 23),
            Some(AtFileQuery {
                replace_start: 19,
                query: "src".to_string(),
            })
        );
    }

    #[test]
    fn test_complete_file_paths_filters_git_and_matches_prefix() {
        let dir = std::env::temp_dir().join(format!("mehh_file_comp_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".git").join("config"), "[core]").unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("src").join("lib.rs"), "pub fn x() {}").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();

        let matches = complete_file_paths(&dir, "src/", 10);
        assert_eq!(
            matches,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
        assert!(!matches.iter().any(|p| p.contains(".git")));

        let by_name = complete_file_paths(&dir, "main", 10);
        assert_eq!(by_name, vec!["src/main.rs".to_string()]);

        let _ = std::fs::remove_dir_all(dir);
    }
}
