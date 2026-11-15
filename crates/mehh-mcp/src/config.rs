//! MCP configuration loading, schema definition, and override merging.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Context;

/// Configuration defining an MCP server (either local stdio process or remote HTTP endpoint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default, alias = "timeout", alias = "timeoutMs", alias = "timeout_ms", skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
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

    /// Whether this entry is an override (has no command or url specified).
    pub fn is_override(&self) -> bool {
        self.command.is_none() && self.url.is_none()
    }
}

/// Container schema for `mcp.json` configuration files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
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
    if !override_cfg.args.is_empty() {
        base.args = override_cfg.args;
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
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
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
                servers
                    .entry(name)
                    .and_modify(|base| merge_server_config(base, local_cfg.clone()))
                    .or_insert(local_cfg);
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
            args: vec!["-y".to_string(), "server".to_string()],
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
        assert_eq!(base.enabled, Some(false));
        assert_eq!(base.timeout_ms, Some(10000));
        assert_eq!(base.env.get("KEY1").map(|s| s.as_str()), Some("val1"));
        assert_eq!(base.env.get("KEY2").map(|s| s.as_str()), Some("val2"));
    }
}
