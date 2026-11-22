# Dump 04 - Session, settings, trust, permissions

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-coding-agent/src/session.rs | 71.9 | 1993 | a4b9dbfe1ca1dabddd937e8e7eecf7530ff041a1c64fa898a857e60dd9ed335a |
| crates/mehh-coding-agent/src/settings.rs | 17.3 | 414 | 2bf2861fda2b542f454c87d50558d7a2cb1b7332e798036c70d99c49fa07907f |
| crates/mehh-coding-agent/src/trust.rs | 9.5 | 271 | e8d15093d77e7db281558aaa6eec0b496bfa07f4b424bfb03ed3bd3fd0cb5e0a |
| crates/mehh-coding-agent/src/permissions.rs | 19.6 | 509 | 6822e759f71508063331618abcc22d66389ce541b34d85d44682cc37fb2f36d1 |

## crates/mehh-coding-agent/src/session.rs

`````rust
//! Session management with JSONL v3 format, tree branching, and compaction.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
        digits.parse().unwrap_or(0)
    } else {
        0
    };

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
                    #[serde(borrow)]
                    name: Option<&'a str>,
                }

                if let Ok(fast) = serde_json::from_str::<FastSessionLine>(trimmed) {
                    if fast.id.is_some() && fast.kind.is_some() {
                        entry_count += 1;
                        if fast.kind == Some("session_info") {
                            session_name = fast
                                .name
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
        if out == Path::new("-") {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", serde_json::to_string(&sm.header)?)?;
            for entry in sm.entries() {
                writeln!(stdout, "{}", serde_json::to_string(entry)?)?;
            }
            std::io::Write::flush(&mut stdout)?;
        } else {
            sm.export_jsonl(out)?;
            eprintln!("Exported session to {}", out.display());
        }
    } else {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", serde_json::to_string(&sm.header)?)?;
        for entry in sm.entries() {
            writeln!(stdout, "{}", serde_json::to_string(entry)?)?;
        }
        std::io::Write::flush(&mut stdout)?;
    }
    Ok(())
}

/// Manages session persistence, tree navigation, branching, and context projection.
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    file_path: Option<PathBuf>,
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

            for (line_idx, line_res) in reader.lines().enumerate() {
                let line = match line_res {
                    Ok(l) => l,
                    Err(e) => {
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
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("Failed to append to session file: {}", path.display()))?;
            let line = serde_json::to_string(&entry)?;
            writeln!(file, "{line}")?;
            file.flush()?;
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
            self.leaf_id = Some(entry_id.to_string());
            if let Some(path) = &self.file_path {
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
                let line = serde_json::to_string(&PersistedBranchSelection::new(entry_id))?;
                writeln!(file, "{line}")?;
                file.flush()?;
            }
            Ok(())
        } else {
            Err(anyhow!("Entry ID not found in session: {entry_id}"))
        }
    }

    /// Projects active messages from `leaf_id` back to root, taking compactions into account.
    pub fn build_context_messages(&self) -> Vec<Message> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();

        while let Some(id) = curr {
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
                    .unwrap_or(comp_pos);

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

    /// Looks up an entry by its ID in O(1) time.
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        let &idx = self.by_id.get(id)?;
        self.entries.get(idx)
    }

    /// Exports the entire session as a JSONL file to `output_path`.
    pub fn export_jsonl(&self, output_path: &Path) -> Result<()> {
        if let Some(parent) = output_path.parent() {
            create_dir_all(parent)?;
        }
        let mut file = File::create(output_path)?;
        writeln!(file, "{}", serde_json::to_string(&self.header)?)?;
        for entry in &self.entries {
            writeln!(file, "{}", serde_json::to_string(entry)?)?;
        }
        file.flush()?;
        Ok(())
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
        self.header.id = id.clone();
        self.header.timestamp = timestamp.clone();
        self.entries.clear();
        self.by_id.clear();
        self.leaf_id = None;

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

            let header_line = serde_json::to_string(&self.header)?;
            writeln!(file, "{header_line}")?;
            file.flush()?;

            self.file_path = Some(new_path);
        }

        Ok(())
    }

    /// Returns the entries along the active branch from root to the current leaf.
    pub fn active_path_entries(&self) -> Vec<&SessionEntry> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();

        while let Some(id) = curr {
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
        ) -> RpcSessionTreeNode {
            let labels = match labels_by_target.get(&entry.id) {
                Some(Some(lbl)) if !lbl.is_empty() => vec![lbl.clone()],
                _ => Vec::new(),
            };
            let children = children_map
                .get(&Some(entry.id.as_str()))
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|child| build_rpc_node(child, children_map, labels_by_target))
                .collect();

            RpcSessionTreeNode {
                id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
                entry: entry.kind.clone(),
                labels,
                children,
            }
        }

        children_map
            .get(&None)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| build_rpc_node(entry, &children_map, &labels_by_target))
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

        while let Some(id) = curr {
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
        })
    }

    /// Duplicates the active path up to current leaf into a brand new session file with `parent_session: None`.
    pub fn clone_at_leaf(&self) -> Result<SessionManager> {
        let mut path_indices = Vec::new();
        let mut curr = self.leaf_id.as_deref();

        while let Some(id) = curr {
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
}
`````

## crates/mehh-coding-agent/src/settings.rs

`````rust
//! User and project settings management.

use anyhow::Context;
use std::path::{Path, PathBuf};

static ATOMIC_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomically writes `bytes` to `path` via a unique temp file in the same directory and `rename`.
/// When `restricted` is true on Unix, creates the temp file with `0o600` permissions.
pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8], restricted: bool) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directories for: {}", path.display()))?;

    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let pid = std::process::id();

    let mut attempt = 0u32;
    let (tmp_path, mut file) = loop {
        let seq = ATOMIC_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.tmp.{pid}.{seq}"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if restricted {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(f) => break (candidate, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to create temporary file: {}", candidate.display())
                });
            }
        }
    };

    #[cfg(not(unix))]
    let _ = restricted;

    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e)
            .with_context(|| format!("Failed to write temporary file: {}", tmp_path.display()));
    }

    #[cfg(unix)]
    if restricted {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| {
                format!("Failed to set 0600 permissions on: {}", tmp_path.display())
            });
        }
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| {
            format!(
                "Failed to rename '{}' to '{}'",
                tmp_path.display(),
                path.display()
            )
        });
    }

    Ok(())
}
use serde::{Deserialize, Serialize};

use mehh_ai::ThinkingLevel;

/// Application and session settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<mehh_agent::QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<mehh_agent::QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::tool_inspection::PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_turns: Option<usize>,
}

/// Settings loader and merger.
pub struct SettingsManager;

impl SettingsManager {
    /// Returns the global settings file path `~/.mehh/agent/settings.json`.
    pub fn global_path() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot determine global settings path: neither USERPROFILE nor HOME is set"
                )
            })?;
        Ok(home.join(".mehh").join("agent").join("settings.json"))
    }

    /// Returns the project-local settings file path `<cwd>/.mehh/settings.json`.
    pub fn project_path(cwd: &Path) -> PathBuf {
        cwd.join(".mehh").join("settings.json")
    }

    /// Loads settings from a specific file path, stripping UTF-8 BOM if present.
    /// Returns default settings if the file does not exist, or Err on invalid JSON syntax.
    pub fn load_file(path: &Path) -> anyhow::Result<Settings> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read settings file: {}", path.display()))
            }
        };

        let clean = content.strip_prefix('\u{feff}').unwrap_or(&content);
        if clean.trim().is_empty() {
            return Ok(Settings::default());
        }

        serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse settings JSON: {}", path.display()))
    }

    /// Saves settings to a specific file path, writing pretty UTF-8 JSON without BOM
    /// atomically (creating temp file with restricted permissions on Unix).
    pub fn save_file(path: &Path, settings: &Settings) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(settings)
            .context("Failed to serialize settings to JSON")?;
        write_file_atomic(path, json.as_bytes(), true)
    }

    /// Saves settings to global path `~/.mehh/agent/settings.json`.
    pub fn save_global(settings: &Settings) -> anyhow::Result<()> {
        Self::save_file(&Self::global_path()?, settings)
    }

    /// Saves settings to project-local path `<cwd>/.mehh/settings.json`.
    pub fn save_project(cwd: &Path, settings: &Settings) -> anyhow::Result<()> {
        Self::save_file(&Self::project_path(cwd), settings)
    }

    /// Merges two settings structs, with non-None fields in `overlay` overriding `base`.
    pub fn merge(base: Settings, overlay: Settings) -> Settings {
        Settings {
            default_provider: overlay.default_provider.or(base.default_provider),
            default_model: overlay.default_model.or(base.default_model),
            default_thinking_level: overlay
                .default_thinking_level
                .or(base.default_thinking_level),
            theme: overlay.theme.or(base.theme),
            auto_compact: overlay.auto_compact.or(base.auto_compact),
            max_tokens: overlay.max_tokens.or(base.max_tokens),
            temperature: overlay.temperature.or(base.temperature),
            steering_mode: overlay.steering_mode.or(base.steering_mode),
            follow_up_mode: overlay.follow_up_mode.or(base.follow_up_mode),
            tree_filter_mode: overlay.tree_filter_mode.or(base.tree_filter_mode),
            hide_thinking_block: overlay.hide_thinking_block.or(base.hide_thinking_block),
            permission_mode: overlay.permission_mode.or(base.permission_mode),
            worker_model: overlay.worker_model.or(base.worker_model),
            lead_turns: overlay.lead_turns.or(base.lead_turns),
        }
    }

    /// Loads and merges global and project settings for the given working directory.
    pub fn try_load(cwd: &Path) -> anyhow::Result<Settings> {
        Self::try_load_with_trust(cwd, true)
    }

    /// Loads global settings and, only when `trusted` is true, merges `<cwd>/.mehh/settings.json`.
    /// Fails closed on corrupt JSON or read errors other than `NotFound`.
    pub fn try_load_with_trust(cwd: &Path, trusted: bool) -> anyhow::Result<Settings> {
        let global_path = Self::global_path()?;
        let project_path = Self::project_path(cwd);
        Self::try_load_from_paths(&global_path, &project_path, trusted)
    }

    pub fn try_load_from_paths(
        global_path: &Path,
        project_path: &Path,
        trusted: bool,
    ) -> anyhow::Result<Settings> {
        let global = Self::load_file(global_path)?;
        if !trusted {
            return Ok(global);
        }
        let mut local = Self::load_file(project_path)?;
        // Escalation guard: Workspace .mehh/settings.json cannot elevate to Auto.
        // Neutralized to None so that it does not override user's global preference.
        if local.permission_mode == Some(crate::tool_inspection::PermissionMode::Auto) {
            local.permission_mode = None;
        }
        Ok(Self::merge(global, local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_agent::QueueMode;

    #[test]
    fn test_settings_save_and_load_file_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("settings_test_{}", uuid::Uuid::now_v7()));
        let file_path = temp_dir.join("sub").join("settings.json");

        let settings = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-3-5-sonnet".to_string()),
            default_thinking_level: Some(ThinkingLevel::High),
            theme: Some("nord".to_string()),
            auto_compact: Some(true),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            steering_mode: Some(QueueMode::All),
            follow_up_mode: Some(QueueMode::OneAtATime),
            tree_filter_mode: Some("user".to_string()),
            hide_thinking_block: Some(true),
            permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
            worker_model: None,
            lead_turns: None,
        };

        SettingsManager::save_file(&file_path, &settings).unwrap();
        assert!(file_path.exists());

        let loaded = SettingsManager::load_file(&file_path).unwrap();
        assert_eq!(loaded, settings);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_settings_merge_all_fields() {
        let base = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-3-7-sonnet".to_string()),
            default_thinking_level: Some(ThinkingLevel::Low),
            theme: Some("dark".to_string()),
            auto_compact: Some(false),
            max_tokens: Some(2048),
            temperature: Some(0.2),
            steering_mode: Some(QueueMode::OneAtATime),
            follow_up_mode: Some(QueueMode::OneAtATime),
            tree_filter_mode: Some("all".to_string()),
            hide_thinking_block: Some(false),
            permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
            worker_model: Some("gemini-2.5-flash".to_string()),
            lead_turns: Some(2),
        };

        let overlay = Settings {
            default_provider: Some("openai".to_string()),
            default_model: None,
            default_thinking_level: Some(ThinkingLevel::High),
            theme: None,
            auto_compact: Some(true),
            max_tokens: None,
            temperature: None,
            steering_mode: Some(QueueMode::All),
            follow_up_mode: None,
            tree_filter_mode: None,
            hide_thinking_block: Some(true),
            permission_mode: Some(crate::tool_inspection::PermissionMode::Auto),
            worker_model: None,
            lead_turns: None,
        };

        let merged = SettingsManager::merge(base, overlay);
        assert_eq!(merged.default_provider.as_deref(), Some("openai"));
        assert_eq!(merged.default_model.as_deref(), Some("claude-3-7-sonnet"));
        assert_eq!(merged.default_thinking_level, Some(ThinkingLevel::High));
        assert_eq!(merged.theme.as_deref(), Some("dark"));
        assert_eq!(merged.auto_compact, Some(true));
        assert_eq!(merged.max_tokens, Some(2048));
        assert_eq!(merged.temperature, Some(0.2));
        assert_eq!(merged.steering_mode, Some(QueueMode::All));
        assert_eq!(merged.follow_up_mode, Some(QueueMode::OneAtATime));
        assert_eq!(merged.tree_filter_mode.as_deref(), Some("all"));
        assert_eq!(merged.hide_thinking_block, Some(true));
        assert_eq!(
            merged.permission_mode,
            Some(crate::tool_inspection::PermissionMode::Auto)
        );
        assert_eq!(merged.worker_model.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(merged.lead_turns, Some(2));
    }

    #[test]
    fn test_settings_bom_stripping_and_malformed_json() {
        let temp_dir = std::env::temp_dir().join(format!("settings_bom_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let file_path = temp_dir.join("settings.json");

        // UTF-8 BOM
        let bom_json = "\u{feff}{\"theme\": \"solarized\"}";
        std::fs::write(&file_path, bom_json.as_bytes()).unwrap();

        let loaded = SettingsManager::load_file(&file_path).unwrap();
        assert_eq!(loaded.theme.as_deref(), Some("solarized"));

        // Malformed JSON returns Err
        std::fs::write(&file_path, b"{invalid: json}").unwrap();
        assert!(SettingsManager::load_file(&file_path).is_err());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_try_load_with_trust_gates_project_settings_and_fails_closed() {
        let dir = crate::TempTestDir::new("settings_trust");
        let global_path = dir.path().join("global-settings.json");
        let project_path = dir.path().join("project-settings.json");

        std::fs::write(
            &global_path,
            r#"{"defaultProvider":"anthropic","autoCompact":true}"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"{"defaultProvider":"openai","autoCompact":false}"#,
        )
        .unwrap();

        let untrusted =
            SettingsManager::try_load_from_paths(&global_path, &project_path, false).unwrap();
        assert_eq!(untrusted.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(untrusted.auto_compact, Some(true));

        let trusted =
            SettingsManager::try_load_from_paths(&global_path, &project_path, true).unwrap();
        assert_eq!(trusted.default_provider.as_deref(), Some("openai"));
        assert_eq!(trusted.auto_compact, Some(false));

        // Corrupt project JSON must fail closed when trusted
        std::fs::write(&project_path, b"{corrupt").unwrap();
        assert!(SettingsManager::try_load_from_paths(&global_path, &project_path, true).is_err());
        // Corrupt global JSON always fails closed
        std::fs::write(&global_path, b"{corrupt").unwrap();
        assert!(SettingsManager::try_load_from_paths(&global_path, &project_path, false).is_err());
    }

    #[test]
    fn test_project_settings_auto_permission_escalation_guard() {
        let dir = crate::TempTestDir::new("settings_escalation");
        let global_path = dir.path().join("global-settings.json");
        let project_path = dir.path().join("project-settings.json");

        // Global has Auto, project also has Auto
        std::fs::write(&global_path, r#"{"permissionMode":"auto"}"#).unwrap();
        std::fs::write(&project_path, r#"{"permissionMode":"auto"}"#).unwrap();
        let loaded =
            SettingsManager::try_load_from_paths(&global_path, &project_path, true).unwrap();
        // Since local Auto was neutralized to None, global Auto is preserved!
        assert_eq!(
            loaded.permission_mode,
            Some(crate::tool_inspection::PermissionMode::Auto)
        );

        // Global has SmartApprove, project has Auto (trying to escalate)
        std::fs::write(&global_path, r#"{"permissionMode":"smart_approve"}"#).unwrap();
        let loaded2 =
            SettingsManager::try_load_from_paths(&global_path, &project_path, true).unwrap();
        // Global SmartApprove is preserved, project Auto is neutralized!
        assert_eq!(
            loaded2.permission_mode,
            Some(crate::tool_inspection::PermissionMode::SmartApprove)
        );
    }
}
`````

## crates/mehh-coding-agent/src/trust.rs

`````rust
//! Project directory trust store and fail-closed gating for workspace-local configs.

use std::collections::HashMap;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

/// Recorded trust decision for a project directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// Normalizes a project directory path into a canonical lookup key.
pub fn normalize_project_key(cwd: &Path) -> String {
    let abs = cwd.canonicalize().unwrap_or_else(|_| {
        if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(cwd)
        }
    });

    let mut normalized = abs.to_string_lossy().replace('\\', "/");
    if let Some(unc) = normalized.strip_prefix("//?/UNC/") {
        normalized = format!("//{unc}");
    } else if let Some(stripped) = normalized.strip_prefix("//?/") {
        normalized = stripped.to_string();
    }

    while normalized.len() > 1 && normalized.ends_with('/') {
        // Keep drive roots like "C:/" intact.
        if normalized.len() == 3 && normalized.as_bytes()[1] == b':' {
            break;
        }
        normalized.pop();
    }

    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

/// Returns true if the workspace contains project-local config, MCP servers, skills, or prompts.
pub fn has_local_trust_targets(cwd: &Path) -> bool {
    let mehh_dir = cwd.join(".mehh");
    mehh_dir.join("mcp.json").exists()
        || mehh_dir.join("settings.json").exists()
        || mehh_dir.join("skills").is_dir()
        || mehh_dir.join("prompts").is_dir()
        || cwd.join(".agents").join("skills").is_dir()
}

/// Persistent trust decisions stored under `~/.mehh/agent/trust.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub projects: HashMap<String, TrustLevel>,
}

impl TrustStore {
    /// Returns `~/.mehh/agent/trust.json`, failing closed if `USERPROFILE`/`HOME` is unset.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("USERPROFILE/HOME is not set; cannot locate trust store"))?;
        Ok(home.join(".mehh").join("agent").join("trust.json"))
    }

    /// Loads trust decisions from `path`. Returns an empty store on `NotFound`,
    /// strips UTF-8 BOM, and fails closed on corrupt JSON.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to read trust store: {}", path.display()))
            }
        };
        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        if clean.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse trust store JSON: {}", path.display()))
    }

    /// Atomically persists the trust store without UTF-8 BOM and with restrictive permissions.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create trust directory: {}", parent.display())
            })?;
        }

        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize trust store JSON")?;
        crate::settings::write_file_atomic(path, json.as_bytes(), true)
    }

    pub fn get(&self, cwd: &Path) -> Option<TrustLevel> {
        let key = normalize_project_key(cwd);
        self.projects.get(&key).copied()
    }

    pub fn set(&mut self, cwd: &Path, level: TrustLevel) {
        let key = normalize_project_key(cwd);
        self.projects.insert(key, level);
    }
}

/// Resolves whether project-local `.mehh/` config and `.agents/skills` should be trusted.
///
/// Fails closed: headless runs without `--trust` never spawn workspace MCP servers or load
/// untrusted project configs, and corrupt `trust.json` returns an error.
pub fn resolve_project_trust(
    cwd: &Path,
    cli_trust: bool,
    interactive_prompt: bool,
) -> anyhow::Result<bool> {
    if cli_trust {
        return Ok(true);
    }
    if !has_local_trust_targets(cwd) {
        return Ok(false);
    }

    let path = TrustStore::default_path()?;
    let mut store = TrustStore::load_from(&path)?;
    match store.get(cwd) {
        Some(TrustLevel::Trusted) => return Ok(true),
        Some(TrustLevel::Untrusted) => return Ok(false),
        None => {}
    }

    if !interactive_prompt {
        eprintln!(
            "Untrusted project directory '{}': skipping local .mehh/ config and MCP servers (pass --trust to allow).",
            cwd.display()
        );
        return Ok(false);
    }

    eprint!(
        "Project directory '{}' contains local .mehh/ or .agents/ configuration.\nTrust this workspace to load local settings, skills, prompts, and MCP servers? [y/N]: ",
        cwd.display()
    );
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    let level = if trusted {
        TrustLevel::Trusted
    } else {
        TrustLevel::Untrusted
    };
    store.set(cwd, level);
    store.save_to(&path)?;

    Ok(trusted)
}

/// Explicit CLI trust override for the current invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOverride {
    None,
    Approve,
    NoApprove,
}

/// Resolves whether project-local configuration should be trusted taking CLI overrides into account.
/// Overrides (`Approve` or `NoApprove`) take effect for this invocation without mutating disk.
pub fn resolve_project_trust_with_override(
    cwd: &Path,
    trust_override: TrustOverride,
    interactive_prompt: bool,
) -> anyhow::Result<bool> {
    match trust_override {
        TrustOverride::Approve => Ok(true),
        TrustOverride::NoApprove => Ok(false),
        TrustOverride::None => resolve_project_trust(cwd, false, interactive_prompt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_project_key_strips_trailing_slash() {
        let temp = std::env::temp_dir();
        let k1 = normalize_project_key(&temp);
        assert!(!k1.ends_with('\\'));
    }

    #[test]
    fn test_trust_store_bom_and_fail_closed() {
        let dir = crate::TempTestDir::new("trust_store");
        let dir = dir.path().to_path_buf();
        let store_path = dir.join("trust.json");

        // Missing store returns empty
        let empty = TrustStore::load_from(&store_path).unwrap();
        assert!(empty.projects.is_empty());

        // Atomic save and reload roundtrip
        let mut store = TrustStore::default();
        store.set(&dir, TrustLevel::Trusted);
        store.save_to(&store_path).unwrap();

        let reloaded = TrustStore::load_from(&store_path).unwrap();
        assert_eq!(reloaded.get(&dir), Some(TrustLevel::Trusted));

        // UTF-8 BOM stripping
        let raw = std::fs::read_to_string(&store_path).unwrap();
        std::fs::write(&store_path, format!("\u{feff}{raw}")).unwrap();
        let from_bom = TrustStore::load_from(&store_path).unwrap();
        assert_eq!(from_bom.get(&dir), Some(TrustLevel::Trusted));

        // Corrupt JSON must fail closed
        std::fs::write(&store_path, b"{corrupt json").unwrap();
        assert!(TrustStore::load_from(&store_path).is_err());
    }

    #[test]
    fn test_resolve_project_trust_headless_fails_closed() {
        let dir = crate::TempTestDir::new("trust_gate");
        std::fs::create_dir_all(dir.path().join(".mehh")).unwrap();
        std::fs::write(dir.path().join(".mehh").join("mcp.json"), "{}").unwrap();

        assert!(resolve_project_trust(dir.path(), true, false).unwrap());
        if TrustStore::default_path().is_ok() {
            assert!(!resolve_project_trust(dir.path(), false, false).unwrap());
        }
    }

    #[test]
    fn no_approve_overrides_persisted_trust_without_writing_disk() {
        let dir = crate::TempTestDir::new("trust_override");
        let store_dir = crate::TempTestDir::new("trust_override_store");
        let store_path = store_dir.path().join("trust.json");

        let mut store = TrustStore::default();
        store.set(dir.path(), TrustLevel::Trusted);
        store.save_to(&store_path).unwrap();

        let res = resolve_project_trust_with_override(dir.path(), TrustOverride::NoApprove, false)
            .unwrap();
        assert!(!res);

        let res =
            resolve_project_trust_with_override(dir.path(), TrustOverride::Approve, false).unwrap();
        assert!(res);

        let reloaded = TrustStore::load_from(&store_path).unwrap();
        assert_eq!(reloaded.get(dir.path()), Some(TrustLevel::Trusted));
    }
}
`````

## crates/mehh-coding-agent/src/permissions.rs

`````rust
//! Persisted tool permission store and granular rule evaluation.

use crate::tool_inspection::{InspectionVerdict, ToolInspectionContext};
use anyhow::{Context, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    AlwaysAllow,
    AskBefore,
    NeverAllow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionsFile {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionStore {
    user_rules_path: Option<PathBuf>,
    rules: Vec<PermissionRule>,
}

/// Returns default user permissions path `~/.mehh/agent/permissions.json`.
pub fn default_permissions_path() -> Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default permissions path: neither USERPROFILE nor HOME is set"
            )
        })?;
    Ok(home.join(".mehh").join("agent").join("permissions.json"))
}

type RuleSpecificity = (bool, bool, bool, usize);

impl PermissionStore {
    pub fn new(rules: Vec<PermissionRule>, user_rules_path: Option<PathBuf>) -> Self {
        Self {
            user_rules_path,
            rules,
        }
    }

    /// Loads permissions from disk:
    /// 1. Global user rules at `~/.mehh/agent/permissions.json`
    /// 2. Workspace rules at `<cwd>/.mehh/permissions.json` (only if `trusted` is true)
    pub fn load(cwd: &Path, trusted: bool) -> Result<Self> {
        let user_path = default_permissions_path().ok();
        let mut rules = Vec::new();

        if let Some(ref p) = user_path {
            if let Some(user_rules) = Self::read_rules_from_file(p)? {
                rules.extend(user_rules);
            }
        }

        if trusted {
            let ws_path = cwd.join(".mehh").join("permissions.json");
            if let Some(ws_rules) = Self::read_rules_from_file(&ws_path)? {
                // Workspace rules prepend so they take precedence
                let mut combined = ws_rules;
                combined.extend(rules);
                rules = combined;
            }
        }

        Ok(Self {
            user_rules_path: user_path,
            rules,
        })
    }

    fn read_rules_from_file(path: &Path) -> Result<Option<Vec<PermissionRule>>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to read permissions file '{}'", path.display())
                });
            }
        };

        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let file: PermissionsFile = serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse permissions JSON in '{}'", path.display()))?;

        Ok(Some(file.rules))
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Appends a new rule and persists it to the user's global `permissions.json`.
    pub fn add_rule(&mut self, rule: PermissionRule) -> Result<()> {
        if let Some(ref path) = self.user_rules_path {
            let mut file_rules = Self::read_rules_from_file(path)?.unwrap_or_default();
            file_rules.retain(|r| {
                !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                    && r.arg_prefix == rule.arg_prefix
                    && r.path_glob == rule.path_glob)
            });
            file_rules.push(rule.clone());
            let file = PermissionsFile { rules: file_rules };
            let serialized = serde_json::to_string_pretty(&file)?;
            crate::settings::write_file_atomic(path, serialized.as_bytes(), true)?;
        }
        self.rules.retain(|r| {
            !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                && r.arg_prefix == rule.arg_prefix
                && r.path_glob == rule.path_glob)
        });
        self.rules.push(rule);
        Ok(())
    }

    /// Appends a rule for the current session only (no disk persistence).
    pub fn add_session_rule(&mut self, rule: PermissionRule) {
        self.rules.retain(|r| {
            !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                && r.arg_prefix == rule.arg_prefix
                && r.path_glob == rule.path_glob)
        });
        self.rules.push(rule);
    }

    /// Evaluates active rules against the inspection context.
    ///
    /// Specific rules (those with arg_prefix or path_glob, longer prefix, or exact tool names)
    /// take precedence over broad rules (generic tool or global wildcard '*').
    pub fn evaluate(&self, ctx: &ToolInspectionContext) -> Option<InspectionVerdict> {
        let mut best_match: Option<(RuleSpecificity, &PermissionRule)> = None;

        for rule in &self.rules {
            if !rule.tool_name.eq_ignore_ascii_case(&ctx.tool_name) && rule.tool_name != "*" {
                continue;
            }

            if let Some(ref pfx) = rule.arg_prefix {
                let matches_prefix =
                    if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                        if mehh_execpolicy::is_shell_tool(&ctx.tool_name) {
                            // Never use a string prefix here: a chained command
                            // would inherit the rule for its first command.
                            mehh_execpolicy::check_prefix(pfx, cmd).matched
                        } else {
                            cmd.trim().starts_with(pfx.trim())
                        }
                    } else if let Some(path) = ctx.args.get("path").and_then(|v| v.as_str()) {
                        let norm_path = path.trim().replace('\\', "/");
                        let norm_pfx = pfx.trim().replace('\\', "/");
                        norm_path.starts_with(&norm_pfx) || path.trim().starts_with(pfx.trim())
                    } else {
                        false
                    };
                if !matches_prefix {
                    continue;
                }
            }

            if let Some(ref glob_pattern) = rule.path_glob {
                let matches_glob =
                    if let Some(raw_path) = ctx.args.get("path").and_then(|v| v.as_str()) {
                        let norm_path = raw_path.trim().replace('\\', "/");
                        if let Ok(glob) = Glob::new(glob_pattern) {
                            let matcher = glob.compile_matcher();
                            matcher.is_match(&norm_path)
                                || matcher.is_match(raw_path)
                                || matcher.is_match(Path::new(raw_path))
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                if !matches_glob {
                    continue;
                }
            }

            let is_filtered = rule.arg_prefix.is_some() || rule.path_glob.is_some();
            let both_filtered = rule.arg_prefix.is_some() && rule.path_glob.is_some();
            let exact_tool = !rule.tool_name.eq_ignore_ascii_case("*");
            let prefix_len = rule.arg_prefix.as_ref().map_or(0, |s| s.len());
            let specificity = (is_filtered, both_filtered, exact_tool, prefix_len);

            if let Some((best_spec, _)) = best_match {
                if specificity > best_spec {
                    best_match = Some((specificity, rule));
                }
            } else {
                best_match = Some((specificity, rule));
            }
        }

        let (_, rule) = best_match?;

        // Shell rules are decided by the whole parsed command line, not by a
        // raw string prefix. Every sub-command must be covered before an
        // allow rule applies; anything unparseable fails closed to a prompt.
        if mehh_execpolicy::is_shell_tool(&ctx.tool_name) {
            if let (Some(pfx), Some(cmd)) = (
                rule.arg_prefix.as_deref(),
                ctx.args.get("command").and_then(|v| v.as_str()),
            ) {
                let eval = mehh_execpolicy::check_prefix(pfx, cmd);
                let prompt = || InspectionVerdict::RequireApproval {
                    reason: format!(
                        "Permission rule for '{}' cannot safely cover this command",
                        ctx.tool_name
                    ),
                    preview: Some(cmd.to_string()),
                };
                if !eval.safe_parse {
                    // A hard deny must stay a hard deny even when the command
                    // cannot be parsed; only allow rules degrade to a prompt.
                    return Some(if rule.action == PermissionAction::NeverAllow {
                        InspectionVerdict::Deny {
                            reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
                        }
                    } else {
                        prompt()
                    });
                }
                let verdict = mehh_execpolicy::verdict_for(
                    eval,
                    rule.action == PermissionAction::AlwaysAllow,
                );
                return Some(match verdict {
                    mehh_execpolicy::Verdict::Allow => InspectionVerdict::Allow,
                    mehh_execpolicy::Verdict::Prompt => prompt(),
                    mehh_execpolicy::Verdict::Deny => InspectionVerdict::Deny {
                        reason: format!("Permission rule forbids tool '{}': '{cmd}'", ctx.tool_name),
                    },
                });
            }
        }

        match rule.action {
            PermissionAction::AlwaysAllow => Some(InspectionVerdict::Allow),
            PermissionAction::AskBefore => Some(InspectionVerdict::RequireApproval {
                reason: format!(
                    "Permission rule requires approval for tool '{}'",
                    ctx.tool_name
                ),
                preview: ctx
                    .args
                    .get("command")
                    .or_else(|| ctx.args.get("path"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            }),
            PermissionAction::NeverAllow => Some(InspectionVerdict::Deny {
                reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(tool: &str, command: Option<&str>, path: Option<&str>) -> ToolInspectionContext {
        let mut args = serde_json::Map::new();
        if let Some(cmd) = command {
            args.insert(
                "command".to_string(),
                serde_json::Value::String(cmd.to_string()),
            );
        }
        if let Some(p) = path {
            args.insert("path".to_string(), serde_json::Value::String(p.to_string()));
        }
        ToolInspectionContext {
            tool_call_id: "call_test".to_string(),
            tool_name: tool.to_string(),
            args,
            is_read_only: None,
            cwd: PathBuf::from("."),
            origin: None,
        }
    }

    #[test]
    fn session_rule_stays_in_memory() {
        let path = std::env::temp_dir().join(format!("session_rule_{}", uuid::Uuid::now_v7()));
        let mut store = PermissionStore::new(Vec::new(), Some(path.clone()));
        store.add_session_rule(PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        });
        assert_eq!(store.rules().len(), 1);
        assert!(!path.exists(), "session-only rule must not touch disk");
    }

    #[test]
    fn test_permission_rule_specificity_override() {
        let broad = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        };
        let specific = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("rm".to_string()),
            path_glob: None,
            action: PermissionAction::NeverAllow,
        };

        // Notice broad rule is inserted first; specific rule must still take precedence.
        let store = PermissionStore::new(vec![broad, specific], None);

        let allow_ctx = make_ctx("bash", Some("cargo check"), None);
        assert_eq!(store.evaluate(&allow_ctx), Some(InspectionVerdict::Allow));

        let deny_ctx = make_ctx("bash", Some("rm -rf target"), None);
        match store.evaluate(&deny_ctx) {
            Some(InspectionVerdict::Deny { reason }) => {
                assert!(reason.contains("forbids tool 'bash'"));
            }
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_rule_prefix_length_precedence() {
        let git_allow = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("git".to_string()),
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        };
        let git_force_deny = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("git push --force".to_string()),
            path_glob: None,
            action: PermissionAction::NeverAllow,
        };

        let store = PermissionStore::new(vec![git_allow, git_force_deny], None);

        let safe_ctx = make_ctx("bash", Some("git status"), None);
        assert_eq!(store.evaluate(&safe_ctx), Some(InspectionVerdict::Allow));

        let danger_ctx = make_ctx("bash", Some("git push --force origin main"), None);
        match store.evaluate(&danger_ctx) {
            Some(InspectionVerdict::Deny { reason }) => {
                assert!(reason.contains("forbids tool 'bash'"));
            }
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_rule_wildcard_filter_vs_unfiltered_exact_tool() {
        let global_rm_deny = PermissionRule {
            tool_name: "*".to_string(),
            arg_prefix: Some("rm".to_string()),
            path_glob: None,
            action: PermissionAction::NeverAllow,
        };
        let bash_allow = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: None,
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        };

        let store = PermissionStore::new(vec![bash_allow, global_rm_deny], None);

        let rm_ctx = make_ctx("bash", Some("rm -rf /"), None);
        match store.evaluate(&rm_ctx) {
            Some(InspectionVerdict::Deny { .. }) => {}
            other => panic!("Expected Deny due to filtered rm rule, got {:?}", other),
        }

        let ls_ctx = make_ctx("bash", Some("ls -la"), None);
        assert_eq!(store.evaluate(&ls_ctx), Some(InspectionVerdict::Allow));
    }

    #[test]
    fn test_permission_store_add_rule_and_load_roundtrip() {
        let dir = crate::TempTestDir::new("permissions_store");
        let user_file = dir.path().join("permissions.json");

        let mut store = PermissionStore::new(Vec::new(), Some(user_file.clone()));

        let rule1 = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("echo".to_string()),
            path_glob: None,
            action: PermissionAction::AlwaysAllow,
        };
        store.add_rule(rule1).unwrap();

        assert_eq!(store.rules().len(), 1);
        assert!(user_file.exists());

        // Overwrite existing rule pattern with updated action
        let rule1_updated = PermissionRule {
            tool_name: "bash".to_string(),
            arg_prefix: Some("echo".to_string()),
            path_glob: None,
            action: PermissionAction::AskBefore,
        };
        store.add_rule(rule1_updated).unwrap();
        assert_eq!(store.rules().len(), 1);

        // Load into fresh store from the same directory
        let loaded_rules = PermissionStore::read_rules_from_file(&user_file)
            .unwrap()
            .expect("rules present");
        assert_eq!(loaded_rules.len(), 1);
        assert_eq!(loaded_rules[0].action, PermissionAction::AskBefore);
    }

    #[test]
    fn shell_chain_cannot_inherit_an_allow_rule() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git status".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("git status && rm -rf /"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::RequireApproval { .. }) => {}
            other => panic!("chained command must require approval, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_command_keeps_hard_deny() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("rm -rf".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("rm -rf $(echo /)"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::Deny { .. }) => {}
            other => panic!("hard deny must survive unparseable input, got {other:?}"),
        }
    }

    #[test]
    fn absolute_path_cannot_bypass_a_basename_deny_rule() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git push".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("/usr/bin/git push --force origin main"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::Deny { .. }) => {}
            other => panic!("absolute path must still be denied, got {other:?}"),
        }
    }

    #[test]
    fn test_permission_rule_windows_path_glob_matching() {
        let glob_rule = PermissionRule {
            tool_name: "edit".to_string(),
            arg_prefix: None,
            path_glob: Some("src/**/*.rs".to_string()),
            action: PermissionAction::AlwaysAllow,
        };
        let store = PermissionStore::new(vec![glob_rule], None);

        // Windows-style backslashes
        let ctx = make_ctx("edit", None, Some(r"src\sub\test.rs"));
        assert_eq!(store.evaluate(&ctx), Some(InspectionVerdict::Allow));
    }
}
`````

