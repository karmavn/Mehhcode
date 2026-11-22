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
