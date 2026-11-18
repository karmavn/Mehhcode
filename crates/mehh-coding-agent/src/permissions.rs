//! Persisted tool permission store and granular rule evaluation.

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};
use crate::tool_inspection::{InspectionVerdict, ToolInspectionContext};

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
        let file: PermissionsFile = serde_json::from_str(clean).with_context(|| {
            format!("Failed to parse permissions JSON in '{}'", path.display())
        })?;

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
                let matches_prefix = if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                    cmd.trim().starts_with(pfx.trim())
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
                let matches_glob = if let Some(raw_path) = ctx.args.get("path").and_then(|v| v.as_str()) {
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
        match rule.action {
            PermissionAction::AlwaysAllow => Some(InspectionVerdict::Allow),
            PermissionAction::AskBefore => Some(InspectionVerdict::RequireApproval {
                reason: format!("Permission rule requires approval for tool '{}'", ctx.tool_name),
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
            args.insert("command".to_string(), serde_json::Value::String(cmd.to_string()));
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
        }
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
