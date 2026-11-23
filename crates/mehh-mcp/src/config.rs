//! MCP configuration loading, schema definition, and override merging.

use std::collections::{hash_map::Entry, HashMap};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Context;

/// Configuration defining an MCP server (either local stdio process or remote HTTP endpoint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `None` means "not declared"; `Some(vec![])` explicitly overrides with an empty list.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Timeout in milliseconds. Serialized as `timeoutMs`; the legacy snake_case alias is accepted.
    #[serde(default, alias = "timeout_ms", skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Timeout in seconds. Serialized as `timeout`; `timeoutSecs` and `timeout_secs` are also accepted.
    #[serde(default, rename = "timeout", alias = "timeoutSecs", alias = "timeout_secs", skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, rename = "oauthTokenEndpoint", alias = "oauth_token_endpoint", skip_serializing_if = "Option::is_none")]
    pub oauth_token_endpoint: Option<String>,
}

impl McpServerConfig {
    /// Returns whether this server is enabled (defaults to true if unspecified).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Command-line arguments, empty when not declared.
    pub fn args(&self) -> &[String] {
        self.args.as_deref().unwrap_or(&[])
    }

    /// Effective request timeout in milliseconds.
    pub fn effective_timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
            .or_else(|| self.timeout_secs.map(|secs| secs.saturating_mul(1000)))
    }

    /// Whether this entry is an override (has no command or url specified).
    pub fn is_override(&self) -> bool {
        self.command.is_none() && self.url.is_none()
    }
}

/// Container schema for `mcp.json` configuration files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfigFile {
    #[serde(default, alias = "mcp_servers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

/// Merges an override configuration into a base server configuration.
pub fn merge_server_config(base: &mut McpServerConfig, override_cfg: McpServerConfig) {
    if !override_cfg.is_override() {
        *base = override_cfg;
        return;
    }

    if let Some(enabled) = override_cfg.enabled {
        base.enabled = Some(enabled);
    }
    if let Some(timeout) = override_cfg.timeout_ms {
        base.timeout_ms = Some(timeout);
        base.timeout_secs = None;
    }
    if let Some(timeout) = override_cfg.timeout_secs {
        base.timeout_secs = Some(timeout);
        base.timeout_ms = None;
    }
    if let Some(desc) = override_cfg.description {
        base.description = Some(desc);
    }
    if let Some(auth) = override_cfg.auth {
        base.auth = Some(auth);
    }
    if let Some(endpoint) = override_cfg.oauth_token_endpoint {
        base.oauth_token_endpoint = Some(endpoint);
    }
    if let Some(args) = override_cfg.args {
        base.args = Some(args);
    }
    if override_cfg.cwd.is_some() {
        base.cwd = override_cfg.cwd;
    }
    for (k, v) in override_cfg.env {
        base.env.insert(k, v);
    }
    for (k, v) in override_cfg.headers {
        base.headers.insert(k, v);
    }
}

/// Reads a single MCP configuration file, failing on schema/syntax errors and returning None if absent.
fn read_mcp_config_file(path: &Path) -> anyhow::Result<Option<McpConfigFile>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to read MCP config file '{}'", path.display()))
        }
    };

    let stripped = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let parsed: McpConfigFile = serde_json::from_str(stripped)
        .with_context(|| format!("Failed to parse MCP config JSON in '{}'", path.display()))?;

    Ok(Some(parsed))
}

/// Returns the path to the global user MCP configuration: `~/.mehh/agent/mcp.json`.
pub fn global_mcp_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let home = home.filter(|h| !h.is_empty()).map(PathBuf::from)?;
    Some(home.join(".mehh").join("agent").join("mcp.json"))
}

pub(crate) fn load_mcp_configs_from_paths(
    global_path: Option<&Path>,
    cwd: &Path,
    trusted: bool,
) -> anyhow::Result<HashMap<String, McpServerConfig>> {
    let mut servers = HashMap::new();

    // 1. Load global configurations
    if let Some(path) = global_path {
        if let Some(file) = read_mcp_config_file(path)? {
            servers.extend(file.mcp_servers);
        }
    }

    // 2. Load and merge local project configurations only when workspace is trusted.
    if trusted {
        let local_path = cwd.join(".mehh").join("mcp.json");
        if let Some(local_file) = read_mcp_config_file(&local_path)? {
            for (name, local_cfg) in local_file.mcp_servers {
                match servers.entry(name) {
                    Entry::Occupied(mut entry) => {
                        merge_server_config(entry.get_mut(), local_cfg);
                    }
                    Entry::Vacant(entry) => {
                        // An override for a server that does not exist is not a server.
                        if !local_cfg.is_override() {
                            entry.insert(local_cfg);
                        }
                    }
                }
            }
        }
    }

    Ok(servers)
}

/// Loads and merges MCP configurations from global (`~/.mehh/agent/mcp.json`) and,
/// when `trusted` is `true`, workspace (`<cwd>/.mehh/mcp.json`).
pub fn load_mcp_configs_with_trust(
    cwd: &Path,
    trusted: bool,
) -> anyhow::Result<HashMap<String, McpServerConfig>> {
    let global_path = global_mcp_config_path();
    load_mcp_configs_from_paths(global_path.as_deref(), cwd, trusted)
}

/// Loads MCP configurations in fail-closed mode (global only, project config skipped).
/// Prefer [`load_mcp_configs_with_trust`] when workspace trust has been evaluated.
pub fn load_mcp_configs(cwd: &Path) -> anyhow::Result<HashMap<String, McpServerConfig>> {
    load_mcp_configs_with_trust(cwd, false)
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn test_load_mcp_configs_with_trust_gates_project_config() {
        let dir = crate::TempTestDir::new("config_trust");
        let mehh_dir = dir.path().join(".mehh");
        std::fs::create_dir_all(&mehh_dir).unwrap();
        std::fs::write(
            mehh_dir.join("mcp.json"),
            r#"{"mcpServers": {"local_srv": {"command": "echo", "args": ["hi"]}}}"#,
        )
        .unwrap();

        let untrusted = load_mcp_configs_from_paths(None, dir.path(), false).unwrap();
        assert!(!untrusted.contains_key("local_srv"));

        let trusted = load_mcp_configs_from_paths(None, dir.path(), true).unwrap();
        assert!(trusted.contains_key("local_srv"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_override() {
        let mut base = McpServerConfig {
            command: Some("npx".to_string()),
            args: Some(vec!["-y".to_string(), "server".to_string()]),
            env: HashMap::from([("KEY1".to_string(), "val1".to_string())]),
            enabled: Some(true),
            timeout_ms: Some(5000),
            ..Default::default()
        };

        let override_cfg = McpServerConfig {
            enabled: Some(false),
            timeout_ms: Some(10000),
            env: HashMap::from([("KEY2".to_string(), "val2".to_string())]),
            ..Default::default()
        };

        merge_server_config(&mut base, override_cfg);
        assert_eq!(base.command.as_deref(), Some("npx"));
        assert_eq!(base.args(), ["-y".to_string(), "server".to_string()]);
        assert_eq!(base.enabled, Some(false));
        assert_eq!(base.timeout_ms, Some(10000));
        assert_eq!(base.env.get("KEY1").map(|s| s.as_str()), Some("val1"));
        assert_eq!(base.env.get("KEY2").map(|s| s.as_str()), Some("val2"));
    }

    #[test]
    fn test_timeout_seconds_and_empty_args_override() {
        let base: McpServerConfig = serde_json::from_value(serde_json::json!({
            "command": "npx",
            "args": ["server"],
            "timeout": 30
        }))
        .unwrap();
        // Bare `timeout` is seconds, matching the documented legacy contract.
        assert_eq!(base.effective_timeout_ms(), Some(30_000));

        let mut merged = base;
        merge_server_config(
            &mut merged,
            serde_json::from_value(serde_json::json!({ "args": [] })).unwrap(),
        );
        assert!(merged.args().is_empty(), "empty args array must be a real override");

        let legacy: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeoutMs": 250 })).unwrap();
        assert_eq!(legacy.effective_timeout_ms(), Some(250));

        let mut mixed = McpServerConfig {
            timeout_ms: Some(5000),
            ..Default::default()
        };
        merge_server_config(
            &mut mixed,
            serde_json::from_value(serde_json::json!({ "timeout": 12 })).unwrap(),
        );
        assert_eq!(mixed.effective_timeout_ms(), Some(12_000));
    }

    #[test]
    fn timeout_units_accept_bare_secs_explicit_secs_and_millis() {
        let bare_secs: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeout": 5 })).unwrap();
        assert_eq!(bare_secs.effective_timeout_ms(), Some(5000));

        let camel_secs: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeoutSecs": 5 })).unwrap();
        assert_eq!(camel_secs.effective_timeout_ms(), Some(5000));
        let round_trip = serde_json::to_value(&camel_secs).unwrap();
        assert_eq!(round_trip.get("timeout"), Some(&serde_json::json!(5)));
        assert!(round_trip.get("timeoutSecs").is_none());
        let round_tripped: McpServerConfig = serde_json::from_value(round_trip).unwrap();
        assert_eq!(round_tripped.effective_timeout_ms(), Some(5000));

        let snake_secs: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeout_secs": 5 })).unwrap();
        assert_eq!(snake_secs.effective_timeout_ms(), Some(5000));

        let explicit_ms: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeoutMs": 250 })).unwrap();
        assert_eq!(explicit_ms.effective_timeout_ms(), Some(250));

        let snake_ms: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "timeout_ms": 250 })).unwrap();
        assert_eq!(snake_ms.effective_timeout_ms(), Some(250));
    }
}
