use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use mehh_ai::Message;

use super::types::{
    now_iso8601, PersistedBranchSelection, SessionEntry, SessionEntryKind, SessionHeader,
    SessionSummary,
};
use super::SessionManager;

/// Returns the session directory for a given working directory: `~/.mehh/sessions/<cwd_hash>`.
pub fn session_dir_for_cwd(cwd: &Path) -> Result<PathBuf> {
    let norm_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let norm_lossy = norm_cwd.to_string_lossy();
    let stripped = crate::settings::strip_windows_verbatim_prefix(&norm_lossy);
    let norm_cwd_str = stripped.replace('\\', "/");

    let mut hasher = Sha256::new();
    hasher.update(norm_cwd_str.as_bytes());
    let hash_bytes = hasher.finalize();
    let hex_str = format!("{:x}", hash_bytes);
    let cwd_hash = &hex_str[..16];

    let home = crate::user_home_dir()
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
        let mut reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut entry_count = 0;
        let mut session_name: Option<String> = None;
        let mut first_prompt: Option<String> = None;
        let mut line = String::new();
        let mut line_idx = 0usize;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let current_idx = line_idx;
            line_idx += 1;
            let clean = if current_idx == 0 {
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

/// Returns the path of the most recent session file (by header timestamp) for `cwd`
/// by reading only the header line of each `.jsonl` file.
pub fn latest_session_path(cwd: &Path) -> Result<Option<PathBuf>> {
    let dir = session_dir_for_cwd(cwd)?;
    if !dir.exists() {
        return Ok(None);
    }

    let read_dir = std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read session directory: {}", dir.display()))?;

    let mut latest: Option<(String, PathBuf)> = None;
    let mut line = String::new();
    for entry_res in read_dir {
        let Ok(entry) = entry_res else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let mut line_idx = 0usize;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let current_idx = line_idx;
            line_idx += 1;
            let clean = if current_idx == 0 {
                line.strip_prefix('\u{feff}').unwrap_or(&line)
            } else {
                &line
            };
            let trimmed = clean.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(h) = serde_json::from_str::<SessionHeader>(trimmed) {
                let replace = latest
                    .as_ref()
                    .is_none_or(|(best_ts, _)| h.timestamp > *best_ts);
                if replace {
                    latest = Some((h.timestamp, path));
                }
            }
            break;
        }
    }

    Ok(latest.map(|(_, path)| path))
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
    let sm = SessionManager::open_existing_file(input_file, cwd.to_path_buf())?;
    if let Some(out) = output_file {
        if out != Path::new("-") {
            let resolved_out = if out.is_absolute() {
                out.to_path_buf()
            } else {
                cwd.join(out)
            };
            sm.export_jsonl(&resolved_out)?;
            eprintln!("Exported session to {}", resolved_out.display());
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

impl SessionManager {
    fn load_existing_file(file_path: PathBuf) -> Result<Self> {
        let file = File::open(&file_path)
            .with_context(|| format!("Failed to open session file: {}", file_path.display()))?;
        if file.metadata()?.is_dir() {
            anyhow::bail!("Session path is a directory: {}", file_path.display());
        }
        let mut reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut entries = Vec::new();
        let mut by_id = HashMap::new();
        let mut persisted_leaf_id: Option<String> = None;
        let mut skipped_entries = 0usize;
        let mut line = String::new();
        let mut line_idx = 0usize;

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    line_idx += 1;
                    skipped_entries += 1;
                    eprintln!(
                        "Warning: failed to read session line {} in '{}': {e}",
                        line_idx,
                        file_path.display()
                    );
                    continue;
                }
            }
            let current_idx = line_idx;
            line_idx += 1;
            let clean = if current_idx == 0 {
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
                        if entry.id.is_empty() || by_id.contains_key(&entry.id) {
                            skipped_entries += 1;
                            eprintln!(
                                "Warning: skipping duplicate or empty session entry id '{}' at line {} in '{}'",
                                entry.id,
                                line_idx,
                                file_path.display()
                            );
                            continue;
                        }
                        persisted_leaf_id = None;
                        by_id.insert(entry.id.clone(), entries.len());
                        entries.push(entry);
                    }
                    Err(e) => {
                        skipped_entries += 1;
                        eprintln!(
                            "Warning: skipping malformed session entry at line {} in '{}': {e}",
                            line_idx,
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

        let needs_leading_newline = file_needs_leading_newline(&file_path)?;
        Ok(Self {
            header,
            entries,
            by_id,
            leaf_id,
            file_path: Some(file_path),
            needs_leading_newline,
            skipped_entries,
        })
    }

    /// Opens an existing session file on disk (resolving relative paths against `cwd`);
    /// fails if the path does not exist.
    pub fn open_existing_file(path: &Path, cwd: PathBuf) -> Result<Self> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        Self::load_existing_file(resolved)
    }

    fn init_file_with_id(file_path: PathBuf, cwd_buf: PathBuf, id: String) -> Result<Self> {
        let header = SessionHeader {
            kind: "session".to_string(),
            version: 3,
            id,
            timestamp: now_iso8601(),
            cwd: cwd_buf.to_string_lossy().to_string(),
            parent_session: None,
        };

        let header_line = format!("{}\n", serde_json::to_string(&header)?);
        crate::settings::write_file_atomic(&file_path, header_line.as_bytes(), true)?;

        Ok(Self {
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            file_path: Some(file_path),
            needs_leading_newline: false,
            skipped_entries: 0,
        })
    }

    /// Creates a new session file on disk with an explicit session ID in its header.
    pub fn create_file_with_id(
        path: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        id: impl Into<String>,
    ) -> Result<Self> {
        Self::init_file_with_id(path.into(), cwd.into(), id.into())
    }

    /// Opens or initializes a session file on disk.
    pub fn open_file(path: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Result<Self> {
        let file_path = path.into();
        let cwd_buf = cwd.into();

        match Self::load_existing_file(file_path.clone()) {
            Ok(session) => Ok(session),
            Err(err) => {
                let is_not_found = err
                    .chain()
                    .any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::NotFound)
                    });
                if !is_not_found {
                    return Err(err);
                }

                Self::init_file_with_id(file_path, cwd_buf, Uuid::now_v7().to_string())
            }
        }
    }

    /// Appends a generic entry to the session tree under the current `leaf_id`.
    pub fn append_entry(&mut self, kind: SessionEntryKind) -> Result<String> {
        let id = Uuid::now_v7().to_string();
        if self.by_id.contains_key(&id) {
            anyhow::bail!("Duplicate session entry ID: {id}");
        }
        let parent_id = self.leaf_id.clone();
        let timestamp = now_iso8601();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id,
            timestamp,
            kind,
        };

        if let Some(path) = &self.file_path {
            let mut opts = OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let file = opts
                .open(path)
                .with_context(|| format!("Failed to append to session file: {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            if self.needs_leading_newline {
                writer.write_all(b"\n")?;
            }
            serde_json::to_writer(&mut writer, &entry)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            // A flush only reaches the OS page cache; a crash could still lose
            // the last entry. Force it to stable storage before updating memory.
            writer.get_ref().sync_all()?;
            self.needs_leading_newline = false;
        }

        let idx = self.entries.len();
        self.by_id.insert(id.clone(), idx);
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());

        Ok(id)
    }

    /// Appends multiple entries to the session tree in a single buffered write and `sync_all`.
    pub fn append_entries<I>(&mut self, kinds: I) -> Result<Vec<String>>
    where
        I: IntoIterator<Item = SessionEntryKind>,
    {
        let mut prepared: Vec<SessionEntry> = Vec::new();
        let mut batch_ids: HashSet<String> = HashSet::new();
        let mut parent_id = self.leaf_id.clone();

        for kind in kinds {
            let id = Uuid::now_v7().to_string();
            if self.by_id.contains_key(&id) || !batch_ids.insert(id.clone()) {
                anyhow::bail!("Duplicate session entry ID: {id}");
            }
            let entry = SessionEntry {
                id: id.clone(),
                parent_id,
                timestamp: now_iso8601(),
                kind,
            };
            parent_id = Some(id);
            prepared.push(entry);
        }

        if prepared.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(path) = &self.file_path {
            let mut opts = OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let file = opts
                .open(path)
                .with_context(|| format!("Failed to append to session file: {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            if self.needs_leading_newline {
                writer.write_all(b"\n")?;
            }
            for entry in &prepared {
                serde_json::to_writer(&mut writer, entry)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
            self.needs_leading_newline = false;
        }

        let mut ids = Vec::with_capacity(prepared.len());
        for entry in prepared {
            let idx = self.entries.len();
            ids.push(entry.id.clone());
            self.by_id.insert(entry.id.clone(), idx);
            self.leaf_id = Some(entry.id.clone());
            self.entries.push(entry);
        }

        Ok(ids)
    }

    /// Branches conversation from the specified entry ID and persists active branch selection.
    pub fn branch_from(&mut self, entry_id: &str) -> Result<()> {
        if self.by_id.contains_key(entry_id) {
            if let Some(path) = &self.file_path {
                let mut opts = OpenOptions::new();
                opts.create(true).append(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                let file = opts
                    .open(path)
                    .with_context(|| {
                        format!(
                            "Failed to persist branch selection to session file: {}",
                            path.display()
                        )
                    })?;
                let mut writer = std::io::BufWriter::new(file);
                if self.needs_leading_newline {
                    writer.write_all(b"\n")?;
                }
                serde_json::to_writer(&mut writer, &PersistedBranchSelection::new(entry_id))?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                writer.get_ref().sync_all()?;
                self.needs_leading_newline = false;
            }
            // Only after the selection is durable may memory follow the disk.
            self.leaf_id = Some(entry_id.to_string());
            Ok(())
        } else {
            Err(anyhow!("Entry ID not found in session: {entry_id}"))
        }
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

            let header_line = format!("{}\n", serde_json::to_string(&new_header)?);
            crate::settings::write_file_atomic(&new_path, header_line.as_bytes(), true)
                .with_context(|| {
                    format!("Failed to create new session file: {}", new_path.display())
                })?;

            self.file_path = Some(new_path);
        }

        // Only discard the old session once its replacement is on disk. A
        // failed write would otherwise leave a new empty id pointing at the
        // old file, mixing two sessions in one JSONL file.
        self.header = new_header;
        self.entries.clear();
        self.by_id.clear();
        self.leaf_id = None;
        self.needs_leading_newline = false;
        self.skipped_entries = 0;

        Ok(())
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
        let path_indices = self.collect_path_indices(target);

        let cwd_path = Path::new(&self.header.cwd);
        let new_session_path = default_session_path(cwd_path)?;

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

        let mut content = serde_json::to_string(&new_header)?;
        content.push('\n');

        let mut entries = Vec::new();
        let mut by_id = HashMap::new();

        for &idx in &path_indices {
            let entry = self.entries[idx].clone();
            content.push_str(&serde_json::to_string(&entry)?);
            content.push('\n');
            by_id.insert(entry.id.clone(), entries.len());
            entries.push(entry);
        }
        crate::settings::write_file_atomic(&new_session_path, content.as_bytes(), true)
            .with_context(|| {
                format!(
                    "Failed to create forked session file: {}",
                    new_session_path.display()
                )
            })?;

        let leaf_id = target.map(str::to_string);
        Ok(SessionManager {
            header: new_header,
            entries,
            by_id,
            leaf_id,
            file_path: Some(new_session_path),
            needs_leading_newline: false,
            skipped_entries: 0,
        })
    }

    /// Duplicates the active path up to current leaf into a brand new session file with `parent_session: None`.
    pub fn clone_at_leaf(&self) -> Result<SessionManager> {
        let path_indices = self.collect_path_indices(self.leaf_id.as_deref());

        let cwd_path = Path::new(&self.header.cwd);
        let new_session_path = default_session_path(cwd_path)?;

        let new_id = Uuid::now_v7().to_string();
        let new_header = SessionHeader {
            kind: "session".to_string(),
            version: 3,
            id: new_id,
            timestamp: now_iso8601(),
            cwd: self.header.cwd.clone(),
            parent_session: None,
        };

        let mut content = serde_json::to_string(&new_header)?;
        content.push('\n');

        let mut entries = Vec::new();
        let mut by_id = HashMap::new();

        for &idx in &path_indices {
            let entry = self.entries[idx].clone();
            content.push_str(&serde_json::to_string(&entry)?);
            content.push('\n');
            by_id.insert(entry.id.clone(), entries.len());
            entries.push(entry);
        }
        crate::settings::write_file_atomic(&new_session_path, content.as_bytes(), true)
            .with_context(|| {
                format!(
                    "Failed to create cloned session file: {}",
                    new_session_path.display()
                )
            })?;

        let leaf_id = self.leaf_id.clone();
        Ok(SessionManager {
            header: new_header,
            entries,
            by_id,
            leaf_id,
            file_path: Some(new_session_path),
            needs_leading_newline: false,
            skipped_entries: 0,
        })
    }
}
