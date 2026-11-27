//! Project directory trust store and fail-closed gating for workspace-local configs.

use std::collections::HashMap;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

static TRUST_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn trust_lock_mutex(path: &Path) -> Arc<Mutex<()>> {
    let locks = TRUST_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = mehh_ai::normalize_lock_path(path);
    let mut guard = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn with_trust_file_lock<T>(path: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create trust directory: {}", parent.display()))?;
    }
    let mut lock_os = path.as_os_str().to_os_string();
    lock_os.push(".lock");
    let lock_path = PathBuf::from(lock_os);

    let mutex = trust_lock_mutex(&lock_path);
    let _in_proc_guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let lock_file = opts
        .open(&lock_path)
        .with_context(|| format!("Failed to open trust lock file: {}", lock_path.display()))?;
    lock_file
        .lock()
        .with_context(|| format!("Failed to acquire trust lock on: {}", lock_path.display()))?;

    let result = f();
    let _ = lock_file.unlock();
    result
}

/// Recorded trust decision for a project directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::Prefix(_)) | Some(Component::RootDir)
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Normalizes a project directory path into a canonical lookup key.
pub fn normalize_project_key(cwd: &Path) -> String {
    let abs = cwd.canonicalize().unwrap_or_else(|_| {
        let joined = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(cwd)
        };
        normalize_lexical_path(&joined)
    });

    let raw = abs.to_string_lossy();
    let stripped = crate::settings::strip_windows_verbatim_prefix(&raw);
    let mut normalized = stripped.replace('\\', "/");

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

/// Returns the directory chain from `cwd` up to the git root (inclusive),
/// stopping before `user_home_dir()` so global `~/.mehh/` is never treated as
/// a workspace-local trust target.
fn trust_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let mut current = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .join(cwd)
    };
    current = normalize_lexical_path(&current);
    let home_key = crate::user_home_dir().map(|h| normalize_project_key(&h));
    let mut dirs = Vec::new();
    loop {
        if let Some(ref hk) = home_key {
            if normalize_project_key(&current) == *hk {
                break;
            }
        }
        dirs.push(current.clone());
        if current.join(".git").exists() {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => {
                current = parent.to_path_buf();
            }
            _ => break,
        }
    }
    dirs
}

fn dir_has_local_trust_targets(dir: &Path) -> bool {
    let mehh_dir = dir.join(".mehh");
    if mehh_dir.is_dir()
        && (mehh_dir.join("mcp.json").exists()
            || mehh_dir.join("models.json").exists()
            || mehh_dir.join("settings.json").exists()
            || mehh_dir.join("permissions.json").exists()
            || mehh_dir.join("MOIM.md").exists()
            || mehh_dir.join("skills").is_dir()
            || mehh_dir.join("prompts").is_dir())
    {
        return true;
    }
    if dir.join(".agents").join("skills").is_dir() {
        return true;
    }

    // Match AGENTS.md / AGENTS.override.md / CLAUDE.md case-insensitively on
    // case-sensitive filesystems (e.g., AGENTS.MD, CLAUDE.MD).
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(s) = name.to_str() {
                if (s.eq_ignore_ascii_case("agents.md")
                    || s.eq_ignore_ascii_case("agents.override.md")
                    || s.eq_ignore_ascii_case("claude.md"))
                    && entry
                        .file_type()
                        .is_ok_and(|ft| ft.is_file() || (ft.is_symlink() && entry.path().is_file()))
                {
                    return true;
                }
            }
        }
    }

    false
}

/// Returns true if the workspace (or an ancestor up to `.git`) contains
/// project-local config, MCP servers, skills, or prompts.
pub fn has_local_trust_targets(cwd: &Path) -> bool {
    trust_ancestors(cwd)
        .into_iter()
        .any(|dir| dir_has_local_trust_targets(&dir))
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
        let home = crate::user_home_dir()
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
        serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse trust store JSON: {}", path.display()))
    }

    /// Atomically persists the trust store without UTF-8 BOM and with restrictive permissions
    /// under an advisory file lock.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        with_trust_file_lock(path, || {
            let json = serde_json::to_string_pretty(self)
                .context("Failed to serialize trust store JSON")?;
            crate::settings::write_file_atomic(path, json.as_bytes(), true)
        })
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
    let mut stdin = io::stdin().lock();
    resolve_project_trust_from_reader(cwd, cli_trust, interactive_prompt, &mut stdin)
}

fn resolve_project_trust_from_reader<R: io::BufRead>(
    cwd: &Path,
    cli_trust: bool,
    interactive_prompt: bool,
    reader: &mut R,
) -> anyhow::Result<bool> {
    if cli_trust {
        return Ok(true);
    }
    let ancestors = trust_ancestors(cwd);
    if !ancestors.iter().any(|dir| dir_has_local_trust_targets(dir)) {
        return Ok(false);
    }

    let path = TrustStore::default_path()?;
    let store = TrustStore::load_from(&path)?;
    for dir in ancestors {
        match store.get(&dir) {
            Some(TrustLevel::Trusted) => return Ok(true),
            Some(TrustLevel::Untrusted) => return Ok(false),
            None => {}
        }
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
    let bytes_read = reader.read_line(&mut answer)?;
    if bytes_read == 0 {
        return Ok(false);
    }
    let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    let level = if trusted {
        TrustLevel::Trusted
    } else {
        TrustLevel::Untrusted
    };
    with_trust_file_lock(&path, || {
        let mut latest = TrustStore::load_from(&path)?;
        latest.set(cwd, level);
        let json = serde_json::to_string_pretty(&latest)
            .context("Failed to serialize trust store JSON")?;
        crate::settings::write_file_atomic(&path, json.as_bytes(), true)
    })?;

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
    fn normalize_project_key_collapses_dotdot_when_path_does_not_exist() {
        let k1 = normalize_project_key(Path::new("/nonexistent_root/a/b/../c"));
        let k2 = normalize_project_key(Path::new("/nonexistent_root/a/c"));
        assert_eq!(k1, k2);
    }

    #[test]
    fn has_local_trust_targets_detects_agents_md_moim_and_permissions_json() {
        let dir = crate::TempTestDir::new("trust_targets");
        let cwd = dir.path();
        assert!(!has_local_trust_targets(cwd), "empty workspace has no targets");

        for rel in [
            "AGENTS.md",
            "AGENTS.MD",
            "AGENTS.override.md",
            "CLAUDE.md",
            "CLAUDE.MD",
            ".mehh/MOIM.md",
            ".mehh/models.json",
            ".mehh/permissions.json",
        ] {
            let full = cwd.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "x").unwrap();
            assert!(has_local_trust_targets(cwd), "must detect {rel}");
            std::fs::remove_file(&full).unwrap();
        }
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

    #[test]
    fn i14_empty_or_whitespace_trust_store_fails_closed() {
        let dir = crate::TempTestDir::new("trust_empty_fail");
        let store_path = dir.path().join("trust.json");

        for bad in ["", "   \n\t", "\u{feff}   "] {
            std::fs::write(&store_path, bad).unwrap();
            assert!(
                TrustStore::load_from(&store_path).is_err(),
                "empty/whitespace trust store must fail closed: {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn i14_walks_ancestors_up_to_git_root_for_targets_and_trust_resolution() {
        let _env_lock = crate::ENV_MUTEX.lock().await;
        struct HomeGuard(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
                match &self.1 {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
        let prev_up = std::env::var_os("USERPROFILE");
        let prev_home = std::env::var_os("HOME");
        let dir = crate::TempTestDir::new("trust_ancestors");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".mehh").join("agent")).unwrap();
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("HOME", &home);
        let _guard = HomeGuard(prev_up, prev_home);

        // Outer parent above .git has a target that must NOT be walked into from inside repo.
        let outer = dir.path().join("outer");
        let repo = outer.join("repo");
        let sub = repo.join("packages").join("app");
        std::fs::create_dir_all(outer.join(".mehh")).unwrap();
        std::fs::write(outer.join(".mehh").join("mcp.json"), "{}").unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();

        assert!(
            !has_local_trust_targets(&sub),
            "walk must stop at .git root and not detect outer/.mehh"
        );

        // Put trust target at repo root and mark repo trusted.
        std::fs::create_dir_all(repo.join(".mehh")).unwrap();
        std::fs::write(repo.join(".mehh").join("mcp.json"), "{}").unwrap();
        assert!(
            has_local_trust_targets(&sub),
            "sub inside repo must detect repo/.mehh/mcp.json"
        );

        let mut store = TrustStore::default();
        store.set(&repo, TrustLevel::Trusted);
        store.save_to(&TrustStore::default_path().unwrap()).unwrap();
        assert!(
            resolve_project_trust(&sub, false, false).unwrap(),
            "sub must inherit Trusted from ancestor repo"
        );

        store.set(&repo, TrustLevel::Untrusted);
        store.save_to(&TrustStore::default_path().unwrap()).unwrap();
        assert!(
            !resolve_project_trust(&sub, false, false).unwrap(),
            "sub must inherit Untrusted from ancestor repo"
        );
    }

    #[tokio::test]
    async fn i14_eof_on_prompt_returns_false_without_persisting_untrusted() {
        let _env_lock = crate::ENV_MUTEX.lock().await;
        struct HomeGuard(Option<std::ffi::OsString>, Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
                match &self.1 {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
        let prev_up = std::env::var_os("USERPROFILE");
        let prev_home = std::env::var_os("HOME");
        let dir = crate::TempTestDir::new("trust_eof");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("HOME", &home);
        let _guard = HomeGuard(prev_up, prev_home);

        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(".mehh")).unwrap();
        std::fs::write(proj.join(".mehh").join("mcp.json"), "{}").unwrap();

        let mut eof_reader = io::Cursor::new(Vec::<u8>::new());
        let res = resolve_project_trust_from_reader(&proj, false, true, &mut eof_reader).unwrap();
        assert!(!res, "EOF must fail closed as untrusted");
        let store = TrustStore::load_from(&TrustStore::default_path().unwrap()).unwrap();
        assert_eq!(
            store.get(&proj),
            None,
            "EOF must not persist TrustLevel::Untrusted"
        );
    }
}
