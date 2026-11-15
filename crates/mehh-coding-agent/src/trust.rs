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
    let trusted = matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    );

    let level = if trusted {
        TrustLevel::Trusted
    } else {
        TrustLevel::Untrusted
    };
    store.set(cwd, level);
    store.save_to(&path)?;

    Ok(trusted)
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
}
