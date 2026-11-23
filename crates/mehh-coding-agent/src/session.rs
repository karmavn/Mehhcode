//! Session management with JSONL v3 format, tree branching, and compaction.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use mehh_ai::{now_ms, Message, SystemMessage, ThinkingLevel, UserMessage};

/// Generates an ISO 8601 UTC timestamp string (e.g. `2026-11-12T13:17:17.000Z`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let days = (total_secs / 86400) as i64;
    let day_secs = (total_secs % 86400) as u32;
    let hours = day_secs / 3600;
    let mins = (day_secs % 3600) / 60;
    let secs = day_secs % 60;

    // Euclidean affine transform algorithm (Howard Hinnant)
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{mins:02}:{secs:02}.{millis:03}Z")
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Parses an ISO 8601 UTC string to unix timestamp in milliseconds, falling back to current time.
pub fn parse_iso8601_or_now(s: &str) -> i64 {
    let clean = s.trim().trim_end_matches('Z');
    let parts: Vec<&str> = clean.split('T').collect();
    if parts.len() != 2 {
        return now_ms();
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let time_parts: Vec<&str> = parts[1].split(':').collect();
    if date_parts.len() != 3 || time_parts.len() < 3 {
        return now_ms();
    }
    let y: i64 = match date_parts[0].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };
    let m: i64 = match date_parts[1].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };
    let d: i64 = match date_parts[2].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };

    let h: i64 = match time_parts[0].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };
    let min: i64 = match time_parts[1].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };
    let sec_sub: Vec<&str> = time_parts[2].split('.').collect();
    let sec: i64 = match sec_sub[0].parse() {
        Ok(v) => v,
        Err(_) => return now_ms(),
    };
    let ms: i64 = if sec_sub.len() > 1 {
        let digits: String = sec_sub[1]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        match digits.len() {
            1 => digits.parse::<i64>().unwrap_or(0) * 100,
            2 => digits.parse::<i64>().unwrap_or(0) * 10,
            _ => digits.parse().unwrap_or(0),
        }
    } else {
        0
    };

    // Reject values that the civil-date algorithm would otherwise silently
    // normalize into a different valid date (e.g. Feb 30 -> Mar 2).
    if !(1..=12).contains(&m)
        || d < 1
        || d > days_in_month(y, m)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&min)
        || !(0..=59).contains(&sec)
    {
        return now_ms();
    }

    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u32;
    let days = era * 146097 + doe as i64 - 719468;

    let total_secs = days * 86400 + h * 3600 + min * 60 + sec;
    total_secs * 1000 + ms
}

/// JSONL v3 header record written on line 1 of every session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(
        rename = "parentSession",
        alias = "parent_session",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
}

/// Content that an append-only context edit may replace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextEditableContent {
    Text(String),
    ToolResult(Vec<mehh_ai::ToolResultContentBlock>),
    User(Vec<mehh_ai::UserContentBlock>),
    Assistant(Vec<mehh_ai::AssistantContentBlock>),
    Value(serde_json::Value),
}

impl ContextEditableContent {
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::ToolResult(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::User(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::UserContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Assistant(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    mehh_ai::AssistantContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Value(v) => {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            }
        }
    }

    pub fn to_user_content(&self) -> mehh_ai::UserContent {
        match self {
            Self::Text(s) => mehh_ai::UserContent::Text(s.clone()),
            Self::User(blocks) => mehh_ai::UserContent::Blocks(blocks.clone()),
            Self::ToolResult(blocks) => {
                let user_blocks = blocks
                    .iter()
                    .map(|b| match b {
                        mehh_ai::ToolResultContentBlock::Text(t) => {
                            mehh_ai::UserContentBlock::Text(t.clone())
                        }
                        mehh_ai::ToolResultContentBlock::Image(i) => {
                            mehh_ai::UserContentBlock::Image(i.clone())
                        }
                    })
                    .collect();
                mehh_ai::UserContent::Blocks(user_blocks)
            }
            Self::Assistant(_) => {
                let text = self.to_text();
                mehh_ai::UserContent::Text(text)
            }
            Self::Value(v) => {
                if let Ok(blocks) =
                    serde_json::from_value::<Vec<mehh_ai::UserContentBlock>>(v.clone())
                {
                    mehh_ai::UserContent::Blocks(blocks)
                } else if let Some(s) = v.as_str() {
                    mehh_ai::UserContent::Text(s.to_string())
                } else {
                    mehh_ai::UserContent::Text(v.to_string())
                }
            }
        }
    }

    pub fn to_assistant_content(&self) -> Vec<mehh_ai::AssistantContentBlock> {
        match self {
            Self::Assistant(blocks) => blocks.clone(),
            Self::Text(s) => vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                text: s.clone(),
                text_signature: None,
            })],
            _ => vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                text: self.to_text(),
                text_signature: None,
            })],
        }
    }

    pub fn to_tool_result_content(&self) -> Vec<mehh_ai::ToolResultContentBlock> {
        match self {
            Self::ToolResult(blocks) => blocks.clone(),
            Self::Text(s) => vec![mehh_ai::ToolResultContentBlock::Text(
                mehh_ai::TextContent {
                    text: s.clone(),
                    text_signature: None,
                },
            )],
            _ => vec![mehh_ai::ToolResultContentBlock::Text(
                mehh_ai::TextContent {
                    text: self.to_text(),
                    text_signature: None,
                },
            )],
        }
    }
}

/// Replacement payload for a context edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEditReplacement {
    pub content: ContextEditableContent,
}

/// Variant payload of a session entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryKind {
    Message {
        message: Message,
    },
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkingLevel,
    },
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Usage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        usage: mehh_ai::Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
    },
    SessionInfo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    CustomMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<bool>,
        #[serde(rename = "customType")]
        custom_type: String,
        content: mehh_ai::UserContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    ContextEdit {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement: Option<ContextEditReplacement>,
    },
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// An entry in a session tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: SessionEntryKind,
}

/// Node representing a session entry in a tree projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeNode {
    pub entry_id: String,
    pub parent_id: Option<String>,
    pub depth: usize,
    pub label: String,
    pub timestamp: String,
    pub is_active: bool,
    pub is_leaf: bool,
    pub is_user_or_branch: bool,
}

/// Recursive node representing a session entry in an RPC tree projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionTreeNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub entry: SessionEntryKind,
    pub labels: Vec<String>,
    pub children: Vec<RpcSessionTreeNode>,
}

/// Summary of a stored session on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub entry_count: usize,
    pub name: Option<String>,
    pub first_prompt: Option<String>,
}

/// Returns the session directory for a given working directory: `~/.mehh/sessions/<cwd_hash>`.
pub fn session_dir_for_cwd(cwd: &Path) -> Result<PathBuf> {
    let norm_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let norm_cwd_str = norm_cwd.to_string_lossy().replace('\\', "/");

    let mut hasher = Sha256::new();
    hasher.update(norm_cwd_str.as_bytes());
    let hash_bytes = hasher.finalize();
    let hex_str = format!("{:x}", hash_bytes);
    let cwd_hash = &hex_str[..16];

    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("Failed to determine user home directory"))?;

    Ok(home.join(".mehh").join("sessions").join(cwd_hash))
}

/// Lists all sessions for the given working directory, sorted newest timestamp first.
pub fn list_sessions(cwd: &Path) -> Result<Vec<SessionSummary>> {
    let dir = session_dir_for_cwd(cwd)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    let read_dir = std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read session directory: {}", dir.display()))?;

    for entry_res in read_dir {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut entry_count = 0;
        let mut session_name: Option<String> = None;
        let mut first_prompt: Option<String> = None;

        for (line_idx, line_res) in reader.lines().enumerate() {
            let line = match line_res {
                Ok(l) => l,
                Err(_) => break,
            };
            let clean = if line_idx == 0 {
                line.strip_prefix('\u{feff}').unwrap_or(&line)
            } else {
                &line
            };
            let trimmed = clean.trim();
            if trimmed.is_empty() {
                continue;
            }

            if header.is_none() {
                if let Ok(h) = serde_json::from_str::<SessionHeader>(trimmed) {
                    header = Some(h);
                } else {
                    break;
                }
            } else if PersistedBranchSelection::parse(trimmed).is_some() {
                continue;
            } else if first_prompt.is_none() {
                if let Ok(entry) = serde_json::from_str::<SessionEntry>(trimmed) {
                    entry_count += 1;
                    match &entry.kind {
                        SessionEntryKind::SessionInfo { name } => {
                            session_name = name
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                        }
                        SessionEntryKind::Message {
                            message: Message::User(u),
                        } => {
                            let txt = u.content.text();
                            let first_line = txt.lines().next().unwrap_or("").trim();
                            if !first_line.is_empty() {
                                let snippet = if first_line.chars().count() > 80 {
                                    format!(
                                        "{}...",
                                        first_line.chars().take(77).collect::<String>()
                                    )
                                } else {
                                    first_line.to_string()
                                };
                                first_prompt = Some(snippet);
                            }
                        }
                        _ => {}
                    }
                }
            } else {
                #[derive(serde::Deserialize)]
                struct FastSessionLine<'a> {
                    id: Option<&'a str>,
                    #[serde(rename = "type")]
                    kind: Option<&'a str>,
                    // A renamed session can contain escaped quotes/backslashes;
                    // serde cannot hand those back as a borrowed &str.
                    name: Option<String>,
                }

                if let Ok(fast) = serde_json::from_str::<FastSessionLine>(trimmed) {
                    if fast.id.is_some() && fast.kind.is_some() {
                        entry_count += 1;
                        if fast.kind == Some("session_info") {
                            session_name = fast
                                .name
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                        }
                    }
                }
            }
        }

        if let Some(h) = header {
            summaries.push(SessionSummary {
                path,
                id: h.id,
                timestamp: h.timestamp,
                cwd: h.cwd,
                entry_count,
                name: session_name,
                first_prompt,
            });
        }
    }

    summaries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(summaries)
}

/// Computes default session file path under `~/.mehh/sessions/<cwd_hash>/<timestamp>_<uuid_v7>.jsonl`.
pub fn default_session_path(cwd: &Path) -> Result<PathBuf> {
    let dir = session_dir_for_cwd(cwd)?;
    let ts = now_iso8601().replace(':', "-");
    let id = Uuid::now_v7().to_string();
    let filename = format!("{ts}_{id}.jsonl");
    Ok(dir.join(filename))
}

/// Creates a new default file-backed session manager for the given working directory.
pub fn create_default_file(cwd: impl Into<PathBuf>) -> Result<SessionManager> {
    SessionManager::create_default_file(cwd)
}

/// Exports a session file to an optional destination file or to stdout.
pub fn export_session(input_file: &Path, output_file: Option<&Path>, cwd: &Path) -> Result<()> {
    let sm = SessionManager::open_file(input_file, cwd.to_path_buf())?;
    if let Some(out) = output_file {
        if out != Path::new("-") {
            sm.export_jsonl(out)?;
            eprintln!("Exported session to {}", out.display());
            return Ok(());
        }
    }

    // Both stdout paths share the same serialization, including the final
    // branch selection, so reopening the output restores the active leaf.
    let content = sm.export_content()?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(content.as_bytes())?;
    std::io::Write::flush(&mut stdout)?;
    Ok(())
}

/// Returns true when an existing file's final byte is not a newline, meaning a
/// plain append would be concatenated onto a partial/truncated line.
fn file_needs_leading_newline(path: &Path) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to inspect session file: {}", path.display()));
        }
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Manages session persistence, tree navigation, branching, and context projection.
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    file_path: Option<PathBuf>,
    /// Malformed lines encountered while loading; surfaced via `skipped_entries()`.
    skipped_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBranchSelection {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "leafId")]
    leaf_id: String,
}

impl PersistedBranchSelection {
    fn new(leaf_id: &str) -> Self {
        Self {
            kind: "branch_selection".to_string(),
            leaf_id: leaf_id.to_string(),
        }
    }

    fn parse(line: &str) -> Option<String> {
        let parsed: Self = serde_json::from_str(line).ok()?;
        (parsed.kind == "branch_selection" && !parsed.leaf_id.is_empty()).then_some(parsed.leaf_id)
    }
}

impl SessionManager {
    /// Creates an in-memory session manager without disk persistence.
    pub fn in_memory(cwd: impl Into<PathBuf>) -> Self {
        let cwd_buf = cwd.into();
        let id = Uuid::now_v7().to_string();
        let header = SessionHeader {
            kind: "session".to_string(),
            version: 3,
            id,
            timestamp: now_iso8601(),
            cwd: cwd_buf.to_string_lossy().to_string(),
            parent_session: None,
        };

        Self {
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            file_path: None,
            skipped_entries: 0,
        }
    }

    /// Opens or initializes a session file on disk.
    pub fn open_file(path: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Result<Self> {
        let file_path = path.into();
        let cwd_buf = cwd.into();

        if file_path.exists() {
            let file = File::open(&file_path)
                .with_context(|| format!("Failed to open session file: {}", file_path.display()))?;
            let reader = BufReader::new(file);

            let mut header: Option<SessionHeader> = None;
            let mut entries = Vec::new();
            let mut by_id = HashMap::new();
            let mut persisted_leaf_id: Option<String> = None;
            let mut skipped_entries = 0usize;

            for (line_idx, line_res) in reader.lines().enumerate() {
                let line = match line_res {
                    Ok(l) => l,
                    Err(e) => {
                        skipped_entries += 1;
                        eprintln!(
                            "Warning: failed to read session line {} in '{}': {e}",
                            line_idx + 1,
                            file_path.display()
                        );
                        continue;
                    }
                };
                let clean = if line_idx == 0 {
                    line.strip_prefix('\u{feff}').unwrap_or(&line)
                } else {
                    &line
                };
                let trimmed = clean.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if header.is_none() {
                    let h: SessionHeader = serde_json::from_str(trimmed).with_context(|| {
                        format!("Failed to parse session header: {}", file_path.display())
                    })?;
                    header = Some(h);
                } else if let Some(sel_leaf) = PersistedBranchSelection::parse(trimmed) {
                    persisted_leaf_id = Some(sel_leaf);
                } else {
                    match serde_json::from_str::<SessionEntry>(trimmed) {
                        Ok(entry) => {
                            persisted_leaf_id = None;
                            by_id.insert(entry.id.clone(), entries.len());
                            entries.push(entry);
                        }
                        Err(e) => {
                            skipped_entries += 1;
                            eprintln!(
                                "Warning: skipping malformed session entry at line {} in '{}': {e}",
                                line_idx + 1,
                                file_path.display()
                            );
                        }
                    }
                }
            }

            let header = header.ok_or_else(|| {
                anyhow!(
                    "Session file is empty or missing header: {}",
                    file_path.display()
                )
            })?;
            let leaf_id = persisted_leaf_id
                .filter(|id| by_id.contains_key(id))
                .or_else(|| entries.last().map(|e| e.id.clone()));

            Ok(Self {
                header,
                entries,
                by_id,
                leaf_id,
                file_path: Some(file_path),
                skipped_entries,
            })
        } else {
            if let Some(parent) = file_path.parent() {
                create_dir_all(parent)?;
            }

            let id = Uuid::now_v7().to_string();
            let header = SessionHeader {
                kind: "session".to_string(),
                version: 3,
                id,
                timestamp: now_iso8601(),
                cwd: cwd_buf.to_string_lossy().to_string(),
                parent_session: None,
            };

            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&file_path)?;

            let header_line = serde_json::to_string(&header)?;
            writeln!(file, "{header_line}")?;
            file.flush()?;

            Ok(Self {
                header,
                entries: Vec::new(),
                by_id: HashMap::new(),
                leaf_id: None,
                file_path: Some(file_path),
                skipped_entries: 0,
            })
        }
    }

    /// Appends a generic entry to the session tree under the current `leaf_id`.
    pub fn append_entry(&mut self, kind: SessionEntryKind) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        let parent_id = self.leaf_id.clone();
        let timestamp = now_iso8601();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id,
            timestamp,
            kind,
        };

        if let Some(path) = &self.file_path {
            let needs_newline = file_needs_leading_newline(path)?;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("Failed to append to session file: {}", path.display()))?;
            if needs_newline {
                file.write_all(b"\n")?;
            }
            let line = serde_json::to_string(&entry)?;
            writeln!(file, "{line}")?;
            file.flush()?;
            // A flush only reaches the OS page cache; a crash could still lose
            // the last entry. Force it to stable storage before updating memory.
            file.sync_all()?;
        }

        let idx = self.entries.len();
        self.by_id.insert(id.clone(), idx);
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());

        Ok(id)
    }

    /// Appends a message entry to the session.
    pub fn append_message(&mut self, message: Message) -> Result<String> {
        self.append_entry(SessionEntryKind::Message { message })
    }

    /// Appends a model usage entry to the session.
    pub fn append_usage(
        &mut self,
        kind: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        usage: mehh_ai::Usage,
        note: Option<String>,
    ) -> Result<String> {
        self.append_entry(SessionEntryKind::Usage {
            kind,
            provider,
            model,
            usage,
            note,
        })
    }

    /// Appends a custom message entry participating in model context.
    pub fn append_custom_message(
        &mut self,
        custom_type: impl Into<String>,
        content: mehh_ai::UserContent,
        display: Option<bool>,
        details: Option<serde_json::Value>,
    ) -> Result<String> {
        self.append_entry(SessionEntryKind::CustomMessage {
            display,
            custom_type: custom_type.into(),
            content,
            details,
        })
    }

    /// Appends a context edit replacing or omitting an earlier entry from model context.
    pub fn append_context_edit(
        &mut self,
        target_id: impl Into<String>,
        replacement: Option<ContextEditReplacement>,
    ) -> Result<String> {
        self.append_entry(SessionEntryKind::ContextEdit {
            target_id: target_id.into(),
            replacement,
        })
    }

    /// Appends a label marker on a target entry.
    pub fn append_label(
        &mut self,
        target_id: impl Into<String>,
        label: Option<String>,
    ) -> Result<String> {
        self.append_entry(SessionEntryKind::Label {
            target_id: target_id.into(),
            label,
        })
    }

    /// Sets or clears the session name via a SessionInfo entry.
    pub fn set_session_name(&mut self, name: Option<String>) -> Result<()> {
        let trimmed = name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        self.append_entry(SessionEntryKind::SessionInfo { name: trimmed })?;
        Ok(())
    }

    /// Returns the current session name from the latest SessionInfo entry on the active path, if any.
    pub fn session_name(&self) -> Option<String> {
        let path = self.active_path_entries();
        for entry in path.into_iter().rev() {
            if let SessionEntryKind::SessionInfo { name } = &entry.kind {
                return name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
            }
        }
        None
    }

    /// Branches conversation from the specified entry ID and persists active branch selection.
    pub fn branch_from(&mut self, entry_id: &str) -> Result<()> {
        if self.by_id.contains_key(entry_id) {
            if let Some(path) = &self.file_path {
                let needs_newline = file_needs_leading_newline(path)?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| {
                        format!(
                            "Failed to persist branch selection to session file: {}",
                            path.display()
                        )
                    })?;
                if needs_newline {
                    file.write_all(b"\n")?;
                }
                let line = serde_json::to_string(&PersistedBranchSelection::new(entry_id))?;
                writeln!(file, "{line}")?;
                file.flush()?;
                file.sync_all()?;
            }
            // Only after the selection is durable may memory follow the disk.
            self.leaf_id = Some(entry_id.to_string());
            Ok(())
        } else {
            Err(anyhow!("Entry ID not found in session: {entry_id}"))
        }
    }

    /// Projects active messages from `leaf_id` back to root, taking compactions into account.
    pub fn build_context_messages(&self) -> Vec<Message> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();
        let mut steps = 0;

        while let Some(id) = curr {
            // A tampered file can contain a parentId cycle; never walk more
            // than one step per stored entry.
            if steps >= self.entries.len() {
                break;
            }
            steps += 1;
            if let Some(&idx) = self.by_id.get(id) {
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();

        // Collect context edits along the active path
        let mut edits: HashMap<&str, &Option<ContextEditReplacement>> = HashMap::new();
        for &idx in &path_indices {
            if let SessionEntryKind::ContextEdit {
                target_id,
                replacement,
            } = &self.entries[idx].kind
            {
                edits.insert(target_id.as_str(), replacement);
            }
        }

        let project_entry = |entry: &SessionEntry| -> Vec<Message> {
            if let Some(edit) = edits.get(entry.id.as_str()) {
                if edit.is_none() {
                    // Omitted from context
                    return Vec::new();
                }
            }
            let replacement = edits.get(entry.id.as_str()).and_then(|r| r.as_ref());

            match &entry.kind {
                SessionEntryKind::Message { message } => {
                    if let Some(rep) = replacement {
                        let mut msg = message.clone();
                        match &mut msg {
                            Message::User(u) => {
                                u.content = rep.content.to_user_content();
                            }
                            Message::Assistant(a) => {
                                a.content = rep.content.to_assistant_content();
                            }
                            Message::ToolResult(tr) => {
                                tr.content = rep.content.to_tool_result_content();
                            }
                            Message::System(s) => {
                                s.content = rep.content.to_text();
                            }
                        }
                        vec![msg]
                    } else {
                        vec![message.clone()]
                    }
                }
                SessionEntryKind::CustomMessage { content, .. } => {
                    let effective_content = if let Some(rep) = replacement {
                        rep.content.to_user_content()
                    } else {
                        content.clone()
                    };
                    let ts = parse_iso8601_or_now(&entry.timestamp);
                    vec![Message::User(UserMessage {
                        content: effective_content,
                        timestamp: ts,
                    })]
                }
                SessionEntryKind::BranchSummary { summary, .. } => {
                    const BRANCH_SUMMARY_PREFIX: &str =
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
                    const BRANCH_SUMMARY_SUFFIX: &str = "\n</summary>";
                    let ts = parse_iso8601_or_now(&entry.timestamp);
                    vec![Message::User(UserMessage {
                        content: mehh_ai::UserContent::Text(format!(
                            "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
                        )),
                        timestamp: ts,
                    })]
                }
                _ => Vec::new(),
            }
        };

        // Check if there is a Compaction entry on the active path
        let last_compaction_pos = path_indices.iter().rposition(|&idx| {
            matches!(&self.entries[idx].kind, SessionEntryKind::Compaction { .. })
        });

        if let Some(comp_pos) = last_compaction_pos {
            let comp_idx = path_indices[comp_pos];
            if let SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } = &self.entries[comp_idx].kind
            {
                let comp_ts = parse_iso8601_or_now(&self.entries[comp_idx].timestamp);
                let mut messages = vec![Message::System(SystemMessage {
                    content: summary.clone(),
                    timestamp: comp_ts,
                })];

                // Find first kept entry on active path before compaction
                let first_kept_pos = path_indices[..comp_pos]
                    .iter()
                    .position(|&idx| self.entries[idx].id == *first_kept_entry_id)
                    .unwrap_or(0);

                for &idx in &path_indices[first_kept_pos..comp_pos] {
                    messages.extend(project_entry(&self.entries[idx]));
                }

                for &idx in &path_indices[comp_pos + 1..] {
                    messages.extend(project_entry(&self.entries[idx]));
                }

                return messages;
            }
        }

        // No compaction on active path
        let mut messages = Vec::new();
        for &idx in &path_indices {
            messages.extend(project_entry(&self.entries[idx]));
        }
        messages
    }

    /// Returns a slice of all session entries.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Number of malformed lines skipped while loading the session file.
    pub fn skipped_entries(&self) -> usize {
        self.skipped_entries
    }

    /// Looks up an entry by its ID in O(1) time.
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        let &idx = self.by_id.get(id)?;
        self.entries.get(idx)
    }

    /// Serializes the header, every entry, and the active leaf as JSONL text.
    /// The trailing branch-selection line is required when the leaf is not the
    /// last stored entry; `open_file` otherwise restores that newer entry.
    fn export_content(&self) -> Result<String> {
        let mut content = String::new();
        content.push_str(&serde_json::to_string(&self.header)?);
        content.push('\n');
        for entry in &self.entries {
            content.push_str(&serde_json::to_string(entry)?);
            content.push('\n');
        }
        if let Some(leaf_id) = &self.leaf_id {
            if self.entries.last().map(|e| e.id.as_str()) != Some(leaf_id.as_str()) {
                content.push_str(&serde_json::to_string(&PersistedBranchSelection::new(leaf_id))?);
                content.push('\n');
            }
        }
        Ok(content)
    }

    /// Exports the entire session as a JSONL file to `output_path`.
    pub fn export_jsonl(&self, output_path: &Path) -> Result<()> {
        let content = self.export_content()?;
        crate::settings::write_file_atomic(output_path, content.as_bytes(), true)
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> &str {
        &self.header.id
    }

    /// Returns the current active leaf ID, if any.
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Returns the file path if persisted.
    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    /// Returns a reference to the session header.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Creates a default file-backed session manager for the given working directory.
    pub fn create_default_file(cwd: impl Into<PathBuf>) -> Result<Self> {
        let cwd_buf = cwd.into();
        let path = default_session_path(&cwd_buf)?;
        Self::open_file(path, cwd_buf)
    }

    /// Resets the session to a fresh session, creating a new file in the same directory if file-backed.
    pub fn new_session(&mut self) -> Result<()> {
        let id = Uuid::now_v7().to_string();
        let timestamp = now_iso8601();
        let mut new_header = self.header.clone();
        new_header.id = id.clone();
        new_header.timestamp = timestamp.clone();

        if let Some(existing_path) = &self.file_path {
            let parent = existing_path.parent().unwrap_or_else(|| Path::new("."));
            let ts = timestamp.replace(':', "-");
            let new_filename = format!("{ts}_{id}.jsonl");
            let new_path = parent.join(new_filename);

            create_dir_all(parent)?;
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&new_path)
                .with_context(|| {
                    format!("Failed to create new session file: {}", new_path.display())
                })?;

            let header_line = serde_json::to_string(&new_header)?;
            writeln!(file, "{header_line}")?;
            file.flush()?;

            self.file_path = Some(new_path);
        }

        // Only discard the old session once its replacement is on disk. A
        // failed write would otherwise leave a new empty id pointing at the
        // old file, mixing two sessions in one JSONL file.
        self.header = new_header;
        self.entries.clear();
        self.by_id.clear();
        self.leaf_id = None;
        self.skipped_entries = 0;

        Ok(())
    }

    /// Returns the entries along the active branch from root to the current leaf.
    pub fn active_path_entries(&self) -> Vec<&SessionEntry> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();
        let mut steps = 0;

        while let Some(id) = curr {
            // A tampered file can contain a parentId cycle; never walk more
            // than one step per stored entry.
            if steps >= self.entries.len() {
                break;
            }
            steps += 1;
            if let Some(&idx) = self.by_id.get(id) {
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();
        path_indices
            .into_iter()
            .map(|idx| &self.entries[idx])
            .collect()
    }

    /// Returns the working directory recorded in the session header.
    pub fn cwd(&self) -> &str {
        &self.header.cwd
    }

    /// Builds a DFS tree projection of session entries.
    pub fn build_tree(&self) -> Vec<SessionTreeNode> {
        let active_entries = self.active_path_entries();
        let active_set: HashSet<&str> = active_entries.iter().map(|e| e.id.as_str()).collect();

        let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            let parent_key = entry
                .parent_id
                .as_deref()
                .filter(|p| self.by_id.contains_key(*p));
            children.entry(parent_key).or_default().push(idx);
        }

        fn dfs(
            parent: Option<&str>,
            depth: usize,
            entries: &[SessionEntry],
            children: &HashMap<Option<&str>, Vec<usize>>,
            active_set: &HashSet<&str>,
            leaf_id: Option<&str>,
            out: &mut Vec<SessionTreeNode>,
        ) {
            // Cap recursion depth so a cyclic parent chain cannot overflow the stack.
            if depth > entries.len() {
                return;
            }
            if let Some(child_indices) = children.get(&parent) {
                for &idx in child_indices {
                    let entry = &entries[idx];
                    let is_active = active_set.contains(entry.id.as_str());
                    let is_leaf = leaf_id == Some(entry.id.as_str());
                    let is_user_or_branch = matches!(
                        &entry.kind,
                        SessionEntryKind::Message {
                            message: Message::User(_)
                        } | SessionEntryKind::BranchSummary { .. }
                            | SessionEntryKind::Compaction { .. }
                    );

                    out.push(SessionTreeNode {
                        entry_id: entry.id.clone(),
                        parent_id: entry.parent_id.clone(),
                        depth,
                        label: entry_to_label(entry),
                        timestamp: entry.timestamp.clone(),
                        is_active,
                        is_leaf,
                        is_user_or_branch,
                    });

                    dfs(
                        Some(&entry.id),
                        depth + 1,
                        entries,
                        children,
                        active_set,
                        leaf_id,
                        out,
                    );
                }
            }
        }

        let mut tree = Vec::new();
        dfs(
            None,
            0,
            &self.entries,
            &children,
            &active_set,
            self.leaf_id.as_deref(),
            &mut tree,
        );
        tree
    }

    /// Builds a recursive RPC session tree with resolved labels attached to each entry.
    pub fn build_rpc_tree(&self) -> Vec<RpcSessionTreeNode> {
        let mut labels_by_target: HashMap<String, Option<String>> = HashMap::new();
        for entry in &self.entries {
            if let SessionEntryKind::Label { target_id, label } = &entry.kind {
                labels_by_target.insert(target_id.clone(), label.clone());
            }
        }

        let mut children_map: HashMap<Option<&str>, Vec<&SessionEntry>> = HashMap::new();
        for entry in &self.entries {
            let p = entry
                .parent_id
                .as_deref()
                .filter(|pid| self.by_id.contains_key(*pid));
            children_map.entry(p).or_default().push(entry);
        }

        fn build_rpc_node(
            entry: &SessionEntry,
            children_map: &HashMap<Option<&str>, Vec<&SessionEntry>>,
            labels_by_target: &HashMap<String, Option<String>>,
            max_depth: usize,
            depth: usize,
        ) -> RpcSessionTreeNode {
            let labels = match labels_by_target.get(&entry.id) {
                Some(Some(lbl)) if !lbl.is_empty() => vec![lbl.clone()],
                _ => Vec::new(),
            };
            let children = if depth >= max_depth {
                // Cap recursion depth so a cyclic parent chain cannot overflow the stack.
                Vec::new()
            } else {
                children_map
                    .get(&Some(entry.id.as_str()))
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|child| {
                        build_rpc_node(
                            child,
                            children_map,
                            labels_by_target,
                            max_depth,
                            depth + 1,
                        )
                    })
                    .collect()
            };

            RpcSessionTreeNode {
                id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
                entry: entry.kind.clone(),
                labels,
                children,
            }
        }

        let max_depth = self.entries.len();
        children_map
            .get(&None)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| build_rpc_node(entry, &children_map, &labels_by_target, max_depth, 0))
            .collect()
    }

    /// Forks session at `target_entry_id` (or current leaf if None), copying the branch path
    /// into a new session file with `parent_session` set to this session's file path.
    pub fn fork_at(&self, target_entry_id: Option<&str>) -> Result<SessionManager> {
        if let Some(target_id) = target_entry_id {
            if !self.by_id.contains_key(target_id) {
                anyhow::bail!("Target entry ID not found in session: {target_id}");
            }
        }

        let target = target_entry_id.or(self.leaf_id.as_deref());
        let mut path_indices = Vec::new();
        let mut curr = target;
        let mut steps = 0;

        while let Some(id) = curr {
            if steps >= self.entries.len() {
                break;
            }
            steps += 1;
            if let Some(&idx) = self.by_id.get(id) {
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();

        let cwd_path = Path::new(&self.header.cwd);
        let new_session_path = default_session_path(cwd_path)?;
        if let Some(parent) = new_session_path.parent() {
            create_dir_all(parent)?;
        }

        let new_id = Uuid::now_v7().to_string();
        let parent_session = self.file_path.as_ref().map(|p| p.display().to_string());
        let new_header = SessionHeader {
            kind: "session".to_string(),
            version: 3,
            id: new_id,
            timestamp: now_iso8601(),
            cwd: self.header.cwd.clone(),
            parent_session,
        };

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&new_session_path)
            .with_context(|| {
                format!(
                    "Failed to create forked session file: {}",
                    new_session_path.display()
                )
            })?;

        writeln!(file, "{}", serde_json::to_string(&new_header)?)?;

        let mut entries = Vec::new();
        let mut by_id = HashMap::new();

        for &idx in &path_indices {
            let entry = self.entries[idx].clone();
            writeln!(file, "{}", serde_json::to_string(&entry)?)?;
            by_id.insert(entry.id.clone(), entries.len());
            entries.push(entry);
        }
        file.flush()?;

        let leaf_id = target.map(str::to_string);
        Ok(SessionManager {
            header: new_header,
            entries,
            by_id,
            leaf_id,
            file_path: Some(new_session_path),
            skipped_entries: self.skipped_entries,
        })
    }

    /// Duplicates the active path up to current leaf into a brand new session file with `parent_session: None`.
    pub fn clone_at_leaf(&self) -> Result<SessionManager> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();
        let mut steps = 0;

        while let Some(id) = curr {
            // A tampered file can contain a parentId cycle; never walk more
            // than one step per stored entry.
            if steps >= self.entries.len() {
                break;
            }
            steps += 1;
            if let Some(&idx) = self.by_id.get(id) {
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();

        let cwd_path = Path::new(&self.header.cwd);
        let new_session_path = default_session_path(cwd_path)?;
        if let Some(parent) = new_session_path.parent() {
            create_dir_all(parent)?;
        }

        let new_id = Uuid::now_v7().to_string();
        let new_header = SessionHeader {
            kind: "session".to_string(),
            version: 3,
            id: new_id,
            timestamp: now_iso8601(),
            cwd: self.header.cwd.clone(),
            parent_session: None,
        };

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&new_session_path)
            .with_context(|| {
                format!(
                    "Failed to create cloned session file: {}",
                    new_session_path.display()
                )
            })?;

        writeln!(file, "{}", serde_json::to_string(&new_header)?)?;

        let mut entries = Vec::new();
        let mut by_id = HashMap::new();

        for &idx in &path_indices {
            let entry = self.entries[idx].clone();
            writeln!(file, "{}", serde_json::to_string(&entry)?)?;
            by_id.insert(entry.id.clone(), entries.len());
            entries.push(entry);
        }
        file.flush()?;

        let leaf_id = self.leaf_id.clone();
        Ok(SessionManager {
            header: new_header,
            entries,
            by_id,
            leaf_id,
            file_path: Some(new_session_path),
            skipped_entries: self.skipped_entries,
        })
    }
}

fn entry_to_label(entry: &SessionEntry) -> String {
    match &entry.kind {
        SessionEntryKind::Message { message } => match message {
            Message::User(u) => {
                let txt = u.content.text();
                let line = txt.lines().next().unwrap_or("").trim();
                if line.chars().count() > 40 {
                    format!("User: {}...", line.chars().take(37).collect::<String>())
                } else if line.is_empty() {
                    "User".to_string()
                } else {
                    format!("User: {line}")
                }
            }
            Message::Assistant(a) => {
                let txt = a.text_content();
                let line = txt.lines().next().unwrap_or("").trim();
                if line.chars().count() > 40 {
                    format!(
                        "Assistant: {}...",
                        line.chars().take(37).collect::<String>()
                    )
                } else if line.is_empty() {
                    let tc = a.tool_calls();
                    if !tc.is_empty() {
                        format!("Assistant: tool call {}", tc[0].name)
                    } else {
                        "Assistant".to_string()
                    }
                } else {
                    format!("Assistant: {line}")
                }
            }
            Message::ToolResult(tr) => format!("ToolResult: {}", tr.tool_name),
            Message::System(s) => {
                let line = s.content.lines().next().unwrap_or("").trim();
                if line.chars().count() > 40 {
                    format!("System: {}...", line.chars().take(37).collect::<String>())
                } else {
                    format!("System: {line}")
                }
            }
        },
        SessionEntryKind::ThinkingLevelChange { thinking_level } => {
            format!("Thinking: {:?}", thinking_level)
        }
        SessionEntryKind::ModelChange { model_id, .. } => format!("Model: {model_id}"),
        SessionEntryKind::Compaction { summary, .. } => {
            let line = summary.lines().next().unwrap_or("").trim();
            if line.chars().count() > 40 {
                format!(
                    "Compaction: {}...",
                    line.chars().take(37).collect::<String>()
                )
            } else {
                format!("Compaction: {line}")
            }
        }
        SessionEntryKind::BranchSummary { summary, .. } => {
            let line = summary.lines().next().unwrap_or("").trim();
            if line.chars().count() > 40 {
                format!("Branch: {}...", line.chars().take(37).collect::<String>())
            } else {
                format!("Branch: {line}")
            }
        }
        SessionEntryKind::SessionInfo { name } => {
            format!("Session: {}", name.as_deref().unwrap_or("unnamed"))
        }
        SessionEntryKind::Custom { custom_type, .. } => format!("Custom: {custom_type}"),
        SessionEntryKind::Usage { kind, model, .. } => {
            let k = kind.as_deref().unwrap_or("usage");
            let m = model.as_deref().unwrap_or("");
            if m.is_empty() {
                format!("Usage: {k}")
            } else {
                format!("Usage: {k} ({m})")
            }
        }
        SessionEntryKind::CustomMessage {
            custom_type,
            content,
            ..
        } => {
            let txt = content.text();
            let line = txt.lines().next().unwrap_or("").trim();
            if line.chars().count() > 40 {
                format!(
                    "CustomMessage [{custom_type}]: {}...",
                    line.chars().take(30).collect::<String>()
                )
            } else if line.is_empty() {
                format!("CustomMessage [{custom_type}]")
            } else {
                format!("CustomMessage [{custom_type}]: {line}")
            }
        }
        SessionEntryKind::ContextEdit {
            target_id,
            replacement,
        } => {
            if replacement.is_some() {
                format!("ContextEdit: replace {target_id}")
            } else {
                format!("ContextEdit: hide {target_id}")
            }
        }
        SessionEntryKind::Label { target_id, label } => {
            if let Some(lbl) = label {
                format!("Label: {lbl} -> {target_id}")
            } else {
                format!("Label: clear -> {target_id}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{now_ms, UserContent, UserMessage};
    use std::sync::mpsc;
    use std::time::Duration;

    fn run_with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(1))
            .expect("operation hung on a parentId cycle")
    }

    fn cyclic_session() -> SessionManager {
        let mut s = SessionManager::in_memory(std::env::temp_dir());
        let mk = |id: &str, parent: Option<&str>| SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            kind: SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text(id.to_string()),
                    timestamp: now_ms(),
                }),
            },
        };
        // A.parentId = B and B.parentId = A; the active leaf is A.
        s.entries = vec![mk("A", Some("B")), mk("B", Some("A"))];
        s.by_id.insert("A".to_string(), 0);
        s.by_id.insert("B".to_string(), 1);
        s.leaf_id = Some("A".to_string());
        s
    }

    #[test]
    fn test_default_session_path() {
        let cwd = std::env::current_dir().unwrap();
        let path = default_session_path(&cwd).unwrap();
        let path_str = path.to_string_lossy();
        assert!(path_str.contains(".mehh"));
        assert!(path_str.contains("sessions"));
        assert!(path_str.ends_with(".jsonl"));
        // Check Windows safe filename: no colons in filename
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(!file_name.contains(':'));
    }

    #[test]
    fn test_new_session_and_active_path_entries() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_session_test_{}", Uuid::now_v7()));
        create_dir_all(&temp_dir).unwrap();
        let session_file = temp_dir.join("test_session.jsonl");

        let mut session = SessionManager::open_file(&session_file, &temp_dir).unwrap();
        let initial_id = session.session_id().to_string();

        let m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Hello 1".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Hello 2".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let active = session.active_path_entries();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].id, m1);
        assert_eq!(active[1].id, m2);

        // Start new session
        session.new_session().unwrap();
        assert_ne!(session.session_id(), initial_id);
        assert_eq!(session.entries().len(), 0);
        assert!(session.leaf_id().is_none());
        assert_eq!(session.active_path_entries().len(), 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_build_tree_and_fork_and_clone() {
        let temp_dir = std::env::temp_dir().join(format!("mehh_tree_test_{}", Uuid::now_v7()));
        create_dir_all(&temp_dir).unwrap();
        let session_file = temp_dir.join("main_session.jsonl");

        let mut session = SessionManager::open_file(&session_file, &temp_dir).unwrap();

        // Entry 1: User prompt
        let m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Build a web server".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Entry 2: Assistant reply
        let m2 = session
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Here is code",
            )))
            .unwrap();

        // Entry 3: Branch A follow-up
        let m3 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Add database connection".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Branch from m2 into Branch B
        session.branch_from(&m2).unwrap();
        let m4 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Use in-memory storage instead".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Verify build_tree
        let tree = session.build_tree();
        assert_eq!(tree.len(), 4);
        assert_eq!(tree[0].entry_id, m1);
        assert_eq!(tree[0].depth, 0);
        assert!(tree[0].is_active);
        assert!(!tree[0].is_leaf);
        assert!(tree[0].is_user_or_branch);
        assert_eq!(tree[0].label, "User: Build a web server");

        assert_eq!(tree[1].entry_id, m2);
        assert_eq!(tree[1].depth, 1);
        assert!(tree[1].is_active);
        assert!(!tree[1].is_leaf);
        assert!(!tree[1].is_user_or_branch);

        // Children of m2: m3 and m4
        assert_eq!(tree[2].entry_id, m3);
        assert_eq!(tree[2].depth, 2);
        assert!(!tree[2].is_active); // m3 is inactive branch A
        assert!(!tree[2].is_leaf);

        assert_eq!(tree[3].entry_id, m4);
        assert_eq!(tree[3].depth, 2);
        assert!(tree[3].is_active); // m4 is active leaf branch B
        assert!(tree[3].is_leaf);

        // Test fork_at m3 (Branch A)
        let forked = session.fork_at(Some(&m3)).unwrap();
        assert_eq!(forked.entries().len(), 3); // m1, m2, m3
        assert_eq!(forked.leaf_id(), Some(m3.as_str()));
        assert_eq!(
            forked.header().parent_session.as_deref(),
            Some(session_file.display().to_string().as_str())
        );
        let forked_active = forked.active_path_entries();
        assert_eq!(forked_active.len(), 3);
        assert_eq!(forked_active[0].id, m1);
        assert_eq!(forked_active[1].id, m2);
        assert_eq!(forked_active[2].id, m3);

        // Test clone_at_leaf
        let cloned = session.clone_at_leaf().unwrap();
        assert_eq!(cloned.entries().len(), 3); // m1, m2, m4
        assert_eq!(cloned.leaf_id(), Some(m4.as_str()));
        assert_eq!(cloned.header().parent_session, None);
        assert_ne!(cloned.session_id(), session.session_id());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_list_sessions() {
        let temp_dir =
            std::env::temp_dir().join(format!("mehh_list_sessions_test_{}", Uuid::now_v7()));
        create_dir_all(&temp_dir).unwrap();

        // Initially empty
        let empty_list = list_sessions(&temp_dir).unwrap();
        assert!(empty_list.is_empty());

        // Create a default session in that directory
        let sdir = session_dir_for_cwd(&temp_dir).unwrap();
        create_dir_all(&sdir).unwrap();

        let sfile1 = sdir.join("2026-11-10T10-00-00.000Z_session1.jsonl");
        let mut session1 = SessionManager::open_file(&sfile1, &temp_dir).unwrap();
        session1
            .append_entry(SessionEntryKind::SessionInfo {
                name: Some("First Named Session".to_string()),
            })
            .unwrap();
        session1
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Initial prompt for session 1".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let sfile2 = sdir.join("2026-11-12T12-00-00.000Z_session2.jsonl");
        let mut session2 = SessionManager::open_file(&sfile2, &temp_dir).unwrap();
        session2
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Second session prompt".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let sessions = list_sessions(&temp_dir).unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest timestamp first: session2 then session1
        assert_eq!(
            sessions[0].first_prompt.as_deref(),
            Some("Second session prompt")
        );
        assert_eq!(sessions[0].name, None);
        assert_eq!(sessions[1].name.as_deref(), Some("First Named Session"));
        assert_eq!(
            sessions[1].first_prompt.as_deref(),
            Some("Initial prompt for session 1")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
        let _ = std::fs::remove_dir_all(sdir);
    }

    #[test]
    fn test_session_persists_branch_selection_and_recovers_from_corrupt_line() {
        let dir = std::env::temp_dir().join(format!("mehh_session_recover_{}", Uuid::now_v7()));
        create_dir_all(&dir).unwrap();
        let file = dir.join("branch_recover.jsonl");

        let (m1, m3) = {
            let mut s = SessionManager::open_file(&file, &dir).unwrap();
            let m1 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("root".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            let _m2 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("branch A".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            s.branch_from(&m1).unwrap();
            let m3 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("branch B".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            // Switch active branch back to m1 WITHOUT appending another message
            s.branch_from(&m1).unwrap();
            (m1, m3)
        };

        // Append a trailing corrupted JSONL line
        {
            let mut f = OpenOptions::new().append(true).open(&file).unwrap();
            writeln!(f, "{{corrupt-trailing-line").unwrap();
        }

        let reopened = SessionManager::open_file(&file, &dir).unwrap();
        assert_eq!(reopened.leaf_id(), Some(m1.as_str()));
        assert_eq!(reopened.build_context_messages().len(), 1);
        assert!(reopened.entries().iter().any(|e| e.id == m3));

        // Non-ASCII bytes inside timestamp fractional field must not panic
        let ts = parse_iso8601_or_now("2026-11-12T10:00:00.äöüZ");
        assert!(ts > 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_jsonl_preserves_branch_selection_and_replaces_destination() {
        let temp = crate::TempTestDir::new("export_branch_selection");
        let source = temp.path().join("source.jsonl");
        let backup = temp.path().join("backup.jsonl");

        let (m1, m3) = {
            let mut s = SessionManager::open_file(&source, temp.path()).unwrap();
            let m1 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("root".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            let _m2 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("abandoned branch".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            s.branch_from(&m1).unwrap();
            let m3 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("selected branch".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            // Switch the leaf back to the older message without a later entry.
            s.branch_from(&m1).unwrap();
            (m1, m3)
        };

        // A pre-existing destination must be replaced, not partially rewritten.
        std::fs::write(&backup, b"stale backup content\n").unwrap();
        SessionManager::open_file(&source, temp.path())
            .unwrap()
            .export_jsonl(&backup)
            .unwrap();

        let exported = std::fs::read_to_string(&backup).unwrap();
        assert!(
            exported
                .lines()
                .any(|line| PersistedBranchSelection::parse(line).as_deref() == Some(m1.as_str())),
            "export must emit the persisted branch selection: {exported}"
        );
        assert!(
            !exported.contains("stale backup content"),
            "export must replace the destination with a complete file"
        );

        let reopened = SessionManager::open_file(&backup, temp.path()).unwrap();
        assert_eq!(reopened.leaf_id(), Some(m1.as_str()));
        assert_eq!(reopened.build_context_messages().len(), 1);
        assert!(reopened.entries().iter().any(|e| e.id == m3));
    }

    #[test]
    fn branch_from_does_not_move_leaf_when_persist_fails() {
        let temp = crate::TempTestDir::new("branch_write_order");
        let file = temp.path().join("branch.jsonl");
        let mut session = SessionManager::open_file(&file, temp.path()).unwrap();
        let m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("first".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("second".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();
        assert_eq!(session.leaf_id(), Some(m2.as_str()));

        // Turn the file path into a directory so appending the branch
        // selection fails after the file has been closed.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();

        assert!(
            session.branch_from(&m1).is_err(),
            "persisting a branch selection into a directory must fail"
        );
        assert_eq!(
            session.leaf_id(),
            Some(m2.as_str()),
            "failed persist must not change the in-memory leaf"
        );
    }

    #[test]
    fn open_file_reports_skipped_malformed_entry() {
        let temp = crate::TempTestDir::new("skipped_entry_count");
        let file = temp.path().join("skipped.jsonl");
        let (m1, m2) = {
            let mut s = SessionManager::open_file(&file, temp.path()).unwrap();
            let a = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("one".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            let b = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("two".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            (a, b)
        };

        // Header, m1, corrupt line, m2
        let raw = std::fs::read_to_string(&file).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.insert(2, "{not-json");
        std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();

        let reopened = SessionManager::open_file(&file, temp.path()).unwrap();
        assert_eq!(
            reopened.skipped_entries(),
            1,
            "malformed mid-file line must be surfaced, not silently dropped"
        );
        let ids: Vec<&str> = reopened.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![m1.as_str(), m2.as_str()]);
    }

    #[test]
    fn usage_custom_message_context_edit_and_label_roundtrip() {
        let usage = mehh_ai::Usage {
            input: 100,
            output: 50,
            cache_read: 20,
            cache_write: 10,
            total_tokens: 150,
            cost: Default::default(),
        };
        let usage_entry = SessionEntry {
            id: "entry_usage".to_string(),
            parent_id: None,
            timestamp: "2026-11-15T12:00:00.000Z".to_string(),
            kind: SessionEntryKind::Usage {
                kind: Some("cache_warm".to_string()),
                provider: Some("anthropic".to_string()),
                model: Some("claude-3-5-sonnet".to_string()),
                usage: usage.clone(),
                note: Some("prewarm".to_string()),
            },
        };
        let json_usage = serde_json::to_string(&usage_entry).unwrap();
        assert!(json_usage.contains(r#""type":"usage""#));
        assert!(json_usage.contains(r#""kind":"cache_warm""#));
        let de_usage: SessionEntry = serde_json::from_str(&json_usage).unwrap();
        assert_eq!(de_usage.kind, usage_entry.kind);

        let custom_msg_entry = SessionEntry {
            id: "entry_custom".to_string(),
            parent_id: Some("entry_usage".to_string()),
            timestamp: "2026-11-15T12:01:00.000Z".to_string(),
            kind: SessionEntryKind::CustomMessage {
                display: Some(true),
                custom_type: "notification".to_string(),
                content: UserContent::Text("Build finished".to_string()),
                details: Some(serde_json::json!({ "buildId": "123" })),
            },
        };
        let json_custom = serde_json::to_string(&custom_msg_entry).unwrap();
        assert!(json_custom.contains(r#""type":"custom_message""#));
        assert!(json_custom.contains(r#""customType":"notification""#));
        let de_custom: SessionEntry = serde_json::from_str(&json_custom).unwrap();
        assert_eq!(de_custom.kind, custom_msg_entry.kind);

        let context_edit_entry = SessionEntry {
            id: "entry_edit".to_string(),
            parent_id: Some("entry_custom".to_string()),
            timestamp: "2026-11-15T12:02:00.000Z".to_string(),
            kind: SessionEntryKind::ContextEdit {
                target_id: "target_123".to_string(),
                replacement: Some(ContextEditReplacement {
                    content: ContextEditableContent::Text("Edited message content".to_string()),
                }),
            },
        };
        let json_edit = serde_json::to_string(&context_edit_entry).unwrap();
        assert!(json_edit.contains(r#""type":"context_edit""#));
        assert!(json_edit.contains(r#""targetId":"target_123""#));
        let de_edit: SessionEntry = serde_json::from_str(&json_edit).unwrap();
        assert_eq!(de_edit.kind, context_edit_entry.kind);

        let label_entry = SessionEntry {
            id: "entry_label".to_string(),
            parent_id: Some("entry_edit".to_string()),
            timestamp: "2026-11-15T12:03:00.000Z".to_string(),
            kind: SessionEntryKind::Label {
                target_id: "target_123".to_string(),
                label: Some("checkpoint-1".to_string()),
            },
        };
        let json_label = serde_json::to_string(&label_entry).unwrap();
        assert!(json_label.contains(r#""type":"label""#));
        assert!(json_label.contains(r#""targetId":"target_123""#));
        assert!(json_label.contains(r#""label":"checkpoint-1""#));
        let de_label: SessionEntry = serde_json::from_str(&json_label).unwrap();
        assert_eq!(de_label.kind, label_entry.kind);
    }

    #[test]
    fn context_edit_hides_and_replaces_without_mutating_tree() {
        let mut session = SessionManager::in_memory(std::env::temp_dir());

        let m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Message 1 to hide".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Message 2 to replace".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let _m3 = session
            .append_custom_message(
                "custom_ext",
                UserContent::Text("Custom 3 to keep".to_string()),
                Some(true),
                None,
            )
            .unwrap();

        // 3 messages visible before edits
        let msgs_before = session.build_context_messages();
        assert_eq!(msgs_before.len(), 3);

        // Append ContextEdit hiding m1 (replacement = None)
        session.append_context_edit(&m1, None).unwrap();

        // Append ContextEdit replacing m2 content
        session
            .append_context_edit(
                &m2,
                Some(ContextEditReplacement {
                    content: ContextEditableContent::Text("Replaced Message 2".to_string()),
                }),
            )
            .unwrap();

        // Check context messages
        let msgs_after = session.build_context_messages();
        assert_eq!(msgs_after.len(), 2);
        // m1 is hidden, so first message is m2 with replaced content
        match &msgs_after[0] {
            Message::User(u) => assert_eq!(u.content.text(), "Replaced Message 2"),
            _ => panic!("Expected user message"),
        }
        // second message is m3 (custom message projected as User)
        match &msgs_after[1] {
            Message::User(u) => assert_eq!(u.content.text(), "Custom 3 to keep"),
            _ => panic!("Expected user message from custom message"),
        }

        // Tree is intact (contains all entries: m1, m2, m3, edit1, edit2)
        assert_eq!(session.entries().len(), 5);
    }

    #[test]
    fn label_entries_track_latest_bookmark_per_target() {
        let mut session = SessionManager::in_memory(std::env::temp_dir());

        let m1 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Task 1".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let m2 = session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Task 2".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Set label on m1
        session.append_label(&m1, Some("v1.0".to_string())).unwrap();

        let rpc_tree = session.build_rpc_tree();
        assert_eq!(rpc_tree.len(), 1);
        let root = &rpc_tree[0];
        assert_eq!(root.id, m1);
        assert_eq!(root.labels, vec!["v1.0".to_string()]);

        // Overwrite label on m1 and set label on m2
        session.append_label(&m1, Some("v2.0".to_string())).unwrap();
        session
            .append_label(&m2, Some("feature".to_string()))
            .unwrap();

        let rpc_tree2 = session.build_rpc_tree();
        assert_eq!(rpc_tree2[0].labels, vec!["v2.0".to_string()]);

        // Clear label on m1 (None)
        session.append_label(&m1, None).unwrap();
        let rpc_tree3 = session.build_rpc_tree();
        assert!(rpc_tree3[0].labels.is_empty());
    }

    #[test]
    fn test_fork_at_rejects_unknown_target_entry_id() {
        let temp = crate::TempTestDir::new("fork_reject");
        let session = SessionManager::create_default_file(temp.path()).unwrap();
        let res = session.fork_at(Some("non_existent_id"));
        match res {
            Err(e) => assert!(e
                .to_string()
                .contains("Target entry ID not found in session: non_existent_id")),
            Ok(_) => panic!("Expected fork_at to fail for non_existent_id"),
        }
    }

    #[test]
    fn test_list_sessions_reflects_latest_renamed_session_name() {
        let temp = crate::TempTestDir::new("list_rename");
        let mut session = SessionManager::create_default_file(temp.path()).unwrap();
        session
            .set_session_name(Some("Original Name".to_string()))
            .unwrap();
        session
            .set_session_name(Some("Renamed Name".to_string()))
            .unwrap();

        let list = list_sessions(temp.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("Renamed Name"));
    }

    #[test]
    fn list_sessions_parses_escaped_quotes_and_backslashes_in_session_name() {
        let temp = crate::TempTestDir::new("list_escaped_name");
        let mut session = SessionManager::create_default_file(temp.path()).unwrap();

        session
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("initial prompt".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let name = "Fix \"auth\" bug in D:\\Mehhcode";
        session.set_session_name(Some(name.to_string())).unwrap();

        let list = list_sessions(temp.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some(name));
        assert_eq!(list[0].entry_count, 2);
    }

    #[test]
    fn parse_iso8601_fractional_seconds_pads_right() {
        assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.5Z"), 500);
        assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.25Z"), 250);
        assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.125Z"), 125);
        assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.1234Z"), 123);
    }

    #[test]
    fn branch_from_after_truncated_tail_survives_reopen() {
        let temp = crate::TempTestDir::new("branch_truncated_tail");
        let file = temp.path().join("branch_truncated.jsonl");

        let (m1, m2) = {
            let mut s = SessionManager::open_file(&file, temp.path()).unwrap();
            let m1 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("first".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            let m2 = s
                .append_message(Message::User(UserMessage {
                    content: UserContent::Text("second".to_string()),
                    timestamp: now_ms(),
                }))
                .unwrap();
            (m1, m2)
        };

        {
            let mut f = OpenOptions::new().append(true).open(&file).unwrap();
            write!(f, "{{\"id\":\"fragment\"").unwrap();
        }

        let mut reopened = SessionManager::open_file(&file, temp.path()).unwrap();
        assert_eq!(reopened.leaf_id(), Some(m2.as_str()));
        assert_eq!(reopened.build_context_messages().len(), 2);

        reopened.branch_from(&m1).unwrap();

        let final_session = SessionManager::open_file(&file, temp.path()).unwrap();
        assert_eq!(final_session.leaf_id(), Some(m1.as_str()));
        assert_eq!(final_session.build_context_messages().len(), 1);
    }

    #[test]
    fn test_list_sessions_skips_empty_user_message_for_first_prompt_and_trims_whitespace_name() {
        let temp = crate::TempTestDir::new("list_empty_prompt");
        let mut session = SessionManager::create_default_file(temp.path()).unwrap();
        session.set_session_name(Some("   ".to_string())).unwrap();

        // First user message is whitespace
        session
            .append_entry(SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("   \n  ".to_string()),
                    timestamp: now_ms(),
                }),
            })
            .unwrap();

        // Second user message has real text
        session
            .append_entry(SessionEntryKind::Message {
                message: Message::User(UserMessage {
                    content: UserContent::Text("Actual user goal here".to_string()),
                    timestamp: now_ms(),
                }),
            })
            .unwrap();

        let list = list_sessions(temp.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, None);
        assert_eq!(
            list[0].first_prompt.as_deref(),
            Some("Actual user goal here")
        );
    }

    #[test]
    fn truncated_tail_does_not_corrupt_appended_entry() {
        let dir = std::env::temp_dir().join(format!("mehh_trunc_tail_{}", Uuid::now_v7()));
        create_dir_all(&dir).unwrap();
        let file = dir.join("truncated.jsonl");

        // A valid session with one real entry.
        let real_id = {
            let mut s = SessionManager::open_file(&file, &dir).unwrap();
            s.append_message(Message::User(UserMessage {
                content: UserContent::Text("real entry".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap()
        };

        // Simulate a crash-truncated final line: valid JSON prefix, no newline.
        {
            let mut f = OpenOptions::new().append(true).open(&file).unwrap();
            write!(f, "{{\"id\":\"fragment\"").unwrap();
        }

        // Reopen: the fragment is skipped, the real entry survives.
        let mut reopened = SessionManager::open_file(&file, &dir).unwrap();
        assert!(reopened.get_entry(&real_id).is_some());
        assert_eq!(reopened.entries().len(), 1);

        // Append a new entry. Without the fix this concatenates onto the
        // unterminated fragment, so BOTH the fragment line and the new entry
        // become unparseable on the next open.
        let new_id = reopened
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("appended after truncation".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        let final_session = SessionManager::open_file(&file, &dir).unwrap();
        assert!(
            final_session.get_entry(&new_id).is_some(),
            "the appended entry must survive reopening a file with a truncated tail"
        );
        assert!(
            final_session.get_entry(&real_id).is_some(),
            "the earlier valid entry must also survive"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cyclic_parent_ids_do_not_hang_context_projection() {
        let msgs = run_with_timeout(|| cyclic_session().build_context_messages());
        assert!(
            msgs.len() <= 2,
            "a parentId cycle must be traversed at most once per id, got {} messages",
            msgs.len()
        );
    }

    #[test]
    fn cyclic_parent_ids_do_not_overflow_tree_builders() {
        let (tree, rpc_tree) = run_with_timeout(|| {
            let s = cyclic_session();
            (s.build_tree(), s.build_rpc_tree())
        });
        assert!(
            tree.len() <= 2,
            "tree traversal must terminate on a parentId cycle, got {} nodes",
            tree.len()
        );
        assert!(rpc_tree.len() <= 2);
    }

    #[test]
    fn compaction_falls_back_to_full_history_when_first_kept_id_missing() {
        let mut s = SessionManager::in_memory(std::env::temp_dir());

        let _earlier_id = s
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("earlier message".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // A Compaction referencing an id that does not exist on the active path.
        s.append_entry(SessionEntryKind::Compaction {
            summary: "summary of history".to_string(),
            first_kept_entry_id: "does_not_exist".to_string(),
            tokens_before: 100,
        })
        .unwrap();

        let msgs = s.build_context_messages();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Message::User(u) if u.content.text() == "earlier message"
            )),
            "earlier messages must stay in context when firstKeptEntryId cannot be found"
        );
    }

    #[test]
    fn invalid_calendar_timestamps_fall_back_to_now() {
        let before = now_ms();
        for bad in [
            "2026-13-01T10:00:00.000Z", // month > 12
            "2026-00-01T10:00:00.000Z", // month 0
            "2026-02-30T10:00:00.000Z", // day out of range for February
            "2026-04-31T10:00:00.000Z", // day out of range for April
            "2026-01-00T10:00:00.000Z", // day 0
            "2025-02-29T10:00:00.000Z", // Feb 29 in a non-leap year
            "2026-01-01T24:00:00.000Z", // hour 24
            "2026-01-01T10:60:00.000Z", // minute 60
            "2026-01-01T10:00:60.000Z", // second 60
        ] {
            let got = parse_iso8601_or_now(bad);
            assert!(
                got >= before && got <= now_ms(),
                "invalid timestamp {bad} should fall back to now_ms(), got {got}"
            );
        }
    }

    #[test]
    fn valid_calendar_timestamp_parses_exactly() {
        assert_eq!(parse_iso8601_or_now("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(
            parse_iso8601_or_now("2026-11-12T13:17:17.000Z"),
            1_794_489_437_000
        );
        assert_eq!(
            parse_iso8601_or_now("2024-02-29T10:00:00.000Z"),
            1_709_200_800_000
        );
    }
}
