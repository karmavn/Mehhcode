//! Session management with JSONL v3 format, tree branching, and compaction.

use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use mehh_ai::{now_ms, Message, SystemMessage, ThinkingLevel};

/// Generates an ISO 8601 UTC timestamp string (e.g. `2026-11-12T13:17:17.000Z`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let duration = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
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
        let frac = sec_sub[1];
        let frac_str = if frac.len() >= 3 { &frac[..3] } else { frac };
        frac_str.parse().unwrap_or(0)
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
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if line_idx == 0 {
                if let Ok(h) = serde_json::from_str::<SessionHeader>(trimmed) {
                    header = Some(h);
                } else {
                    break;
                }
            } else {
                entry_count += 1;
                if let Ok(entry) = serde_json::from_str::<SessionEntry>(trimmed) {
                    match &entry.kind {
                        SessionEntryKind::SessionInfo { name: Some(n) }
                            if session_name.is_none() =>
                        {
                            session_name = Some(n.clone());
                        }
                        SessionEntryKind::Message {
                            message: Message::User(u),
                        } if first_prompt.is_none() => {
                            let txt = u.content.text();
                            let first_line = txt.lines().next().unwrap_or("").trim();
                            let snippet = if first_line.chars().count() > 80 {
                                format!("{}...", first_line.chars().take(77).collect::<String>())
                            } else {
                                first_line.to_string()
                            };
                            first_prompt = Some(snippet);
                        }
                        _ => {}
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

/// Manages session persistence, tree navigation, branching, and context projection.
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    file_path: Option<PathBuf>,
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

            for (line_idx, line_res) in reader.lines().enumerate() {
                let line = line_res?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if line_idx == 0 {
                    let h: SessionHeader = serde_json::from_str(trimmed).with_context(|| {
                        format!("Failed to parse session header: {}", file_path.display())
                    })?;
                    header = Some(h);
                } else {
                    let entry: SessionEntry = serde_json::from_str(trimmed).with_context(|| {
                        format!(
                            "Failed to parse session entry at line {}: {}",
                            line_idx + 1,
                            file_path.display()
                        )
                    })?;
                    by_id.insert(entry.id.clone(), entries.len());
                    entries.push(entry);
                }
            }

            let header = header.ok_or_else(|| {
                anyhow!("Session file is empty or missing header: {}", file_path.display())
            })?;
            let leaf_id = entries.last().map(|e| e.id.clone());

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

    /// Branches conversation from the specified entry ID.
    pub fn branch_from(&mut self, entry_id: &str) -> Result<()> {
        if self.by_id.contains_key(entry_id) {
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

        while let Some(id) = curr {
            if let Some(&idx) = self.by_id.get(id) {
                path_indices.push(idx);
                curr = self.entries[idx].parent_id.as_deref();
            } else {
                break;
            }
        }
        path_indices.reverse();

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
                    if let SessionEntryKind::Message { message } = &self.entries[idx].kind {
                        messages.push(message.clone());
                    }
                }

                for &idx in &path_indices[comp_pos + 1..] {
                    if let SessionEntryKind::Message { message } = &self.entries[idx].kind {
                        messages.push(message.clone());
                    }
                }

                return messages;
            }
        }

        // No compaction on active path
        let mut messages = Vec::new();
        for &idx in &path_indices {
            if let SessionEntryKind::Message { message } = &self.entries[idx].kind {
                messages.push(message.clone());
            }
        }
        messages
    }

    /// Returns a slice of all session entries.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
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
                .with_context(|| format!("Failed to create new session file: {}", new_path.display()))?;

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
        path_indices.into_iter().map(|idx| &self.entries[idx]).collect()
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

    /// Forks session at `target_entry_id` (or current leaf if None), copying the branch path
    /// into a new session file with `parent_session` set to this session's file path.
    pub fn fork_at(&self, target_entry_id: Option<&str>) -> Result<SessionManager> {
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
            .with_context(|| format!("Failed to create forked session file: {}", new_session_path.display()))?;

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
            .with_context(|| format!("Failed to create cloned session file: {}", new_session_path.display()))?;

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
                    format!("Assistant: {}...", line.chars().take(37).collect::<String>())
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
                format!("Compaction: {}...", line.chars().take(37).collect::<String>())
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
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message("Here is code")))
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
        let temp_dir = std::env::temp_dir().join(format!("mehh_list_sessions_test_{}", Uuid::now_v7()));
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
        assert_eq!(sessions[0].first_prompt.as_deref(), Some("Second session prompt"));
        assert_eq!(sessions[0].name, None);
        assert_eq!(sessions[1].name.as_deref(), Some("First Named Session"));
        assert_eq!(sessions[1].first_prompt.as_deref(), Some("Initial prompt for session 1"));

        let _ = std::fs::remove_dir_all(temp_dir);
        let _ = std::fs::remove_dir_all(sdir);
    }
}
