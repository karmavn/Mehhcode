//! Session management with JSONL v3 format, tree branching, and compaction.

mod projection;
mod storage;
mod types;
#[cfg(test)]
mod tests;

pub use storage::{
    create_default_file, default_session_path, export_session, latest_session_path, list_sessions,
    session_dir_for_cwd,
};
pub use types::{
    now_iso8601, parse_iso8601_or_now, ContextEditReplacement, ContextEditableContent,
    RpcSessionTreeNode, SessionEntry, SessionEntryKind, SessionHeader, SessionSummary,
    SessionTreeNode,
};
#[cfg(test)]
use types::PersistedBranchSelection;

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use mehh_ai::Message;

/// Manages session persistence, tree navigation, branching, and context projection.
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    file_path: Option<PathBuf>,
    /// True only if the existing on-disk file lacked a trailing `\n` at load time.
    needs_leading_newline: bool,
    /// Malformed lines encountered while loading; surfaced via `skipped_entries()`.
    skipped_entries: usize,
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
            needs_leading_newline: false,
            skipped_entries: 0,
        }
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

    /// Returns the working directory recorded in the session header.
    pub fn cwd(&self) -> &str {
        &self.header.cwd
    }
}
