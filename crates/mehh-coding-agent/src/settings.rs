//! User and project settings management.

use std::path::{Path, PathBuf};
use anyhow::Context;
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
}

/// Settings loader and merger.
pub struct SettingsManager;

impl SettingsManager {
    /// Returns the global settings file path `~/.mehh/agent/settings.json`.
    pub fn global_path() -> PathBuf {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".mehh").join("agent").join("settings.json")
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
                return Err(e).with_context(|| format!("Failed to read settings file: {}", path.display()))
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
    /// and ensuring parent directories exist.
    pub fn save_file(path: &Path, settings: &Settings) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directories for: {}", path.display()))?;
        }

        let json = serde_json::to_string_pretty(settings)
            .context("Failed to serialize settings to JSON")?;
        std::fs::write(path, json.as_bytes())
            .with_context(|| format!("Failed to write settings file: {}", path.display()))?;
        Ok(())
    }

    /// Saves settings to global path `~/.mehh/agent/settings.json`.
    pub fn save_global(settings: &Settings) -> anyhow::Result<()> {
        Self::save_file(&Self::global_path(), settings)
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
        }
    }

    /// Loads and merges global and project settings for the given working directory.
    pub fn load(cwd: &Path) -> Settings {
        let global = Self::load_file(&Self::global_path()).unwrap_or_default();
        let local = Self::load_file(&Self::project_path(cwd)).unwrap_or_default();
        Self::merge(global, local)
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
}
