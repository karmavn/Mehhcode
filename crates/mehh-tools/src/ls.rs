use std::path::PathBuf;
use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::core::context::{self, PathPolicy};

#[derive(Debug, Clone)]
pub struct LsTool {
    pub cwd: PathBuf,
    pub path_policy: PathPolicy,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            path_policy: PathPolicy::Unrestricted,
        }
    }

    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    fn resolve_path(&self, p: &str) -> Result<PathBuf, String> {
        context::resolve_path(&self.cwd, &self.path_policy, p)
    }
}

const LS_MAX_OUTPUT_BYTES: usize = 100 * 1024;
const LS_MAX_ENTRIES: usize = 10_000;

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return (default: 500)"
                }
            },
            "required": []
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Tool execution cancelled".to_string());
        }

        let path_str = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
            .max(1) as usize;

        let dir_path = self.resolve_path(&path_str)?;
        let explicit_protected =
            context::path_targets_protected_credential(&path_str, &dir_path);
        let cancel_clone = cancel.clone();

        tokio::task::spawn_blocking(move || {
            if !dir_path.exists() {
                return Err(format!("Path not found: {}", path_str));
            }

            if !dir_path.is_dir() {
                return Err(format!("Not a directory: {}", path_str));
            }

            let read_dir = std::fs::read_dir(&dir_path)
                .map_err(|e| format!("Cannot read directory: {}", e))?;

            let mut entries = Vec::new();
            let mut entry_cap_reached = false;
            for entry_res in read_dir {
                if cancel_clone.is_cancelled() {
                    return Err("Tool execution cancelled".to_string());
                }
                if entries.len() >= LS_MAX_ENTRIES {
                    entry_cap_reached = true;
                    break;
                }

                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let name = entry.file_name().to_string_lossy().to_string();
                if !explicit_protected && context::is_protected_credential_name(&name) {
                    continue;
                }
                // Use DirEntry::file_type() directly (no extra stat syscall), and only
                // follow symlinks with std::fs::metadata so symlinked dirs still get '/'.
                let is_dir = match entry.file_type() {
                    Ok(ft) if ft.is_symlink() => std::fs::metadata(entry.path())
                        .map(|m| m.is_dir())
                        .unwrap_or(false),
                    Ok(ft) => ft.is_dir(),
                    Err(_) => false,
                };

                if is_dir {
                    entries.push(format!("{}/", name));
                } else {
                    entries.push(name);
                }
            }

            if entries.is_empty() {
                return Ok(AgentToolResult::text("(empty directory)"));
            }

            if entries.iter().all(|s| s.is_ascii()) {
                entries.sort_by(|a, b| {
                    for (&ba, &bb) in a.as_bytes().iter().zip(b.as_bytes()) {
                        let cmp = ba.to_ascii_lowercase().cmp(&bb.to_ascii_lowercase());
                        if cmp != std::cmp::Ordering::Equal {
                            return cmp;
                        }
                    }
                    a.len().cmp(&b.len())
                });
            } else {
                entries.sort_by_cached_key(|s| s.to_lowercase());
            }

            let limit_reached = entries.len() > limit;
            if limit_reached {
                entries.truncate(limit);
            }

            let output_cap = entries
                .iter()
                .map(|e| e.len().saturating_add(1))
                .sum::<usize>()
                .min(LS_MAX_OUTPUT_BYTES.saturating_add(64));
            let mut output = String::with_capacity(output_cap);
            let mut total_bytes = 0usize;
            let mut byte_cap_reached = false;
            for entry in entries {
                let add_bytes = entry.len().saturating_add(1);
                if total_bytes.saturating_add(add_bytes) > LS_MAX_OUTPUT_BYTES {
                    byte_cap_reached = true;
                    break;
                }
                if total_bytes > 0 {
                    output.push('\n');
                }
                output.push_str(&entry);
                total_bytes += add_bytes;
            }
            if byte_cap_reached {
                output.push_str(&format!(
                    "\n\n[Output truncated at {} byte limit]",
                    LS_MAX_OUTPUT_BYTES
                ));
            } else if entry_cap_reached {
                output.push_str(&format!(
                    "\n\n[Stopped after scanning {} entries]",
                    LS_MAX_ENTRIES
                ));
            } else if limit_reached {
                output.push_str(&format!(
                    "\n\n[{} entries limit reached. Use limit={} for more]",
                    limit,
                    limit.saturating_mul(2)
                ));
            }

            Ok(AgentToolResult::text(output))
        })
        .await
        .map_err(|e| format!("Ls task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ls_skips_protected_credentials_unless_explicitly_targeted() {
        let dir = crate::TempTestDir::new("ls_protected");
        std::fs::create_dir_all(dir.path().join(".mehh")).unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".mehh/secret.txt"), "x").unwrap();
        std::fs::write(dir.path().join(".env"), "A=1").unwrap();
        std::fs::write(dir.path().join(".env.local"), "A=2").unwrap();
        std::fs::write(dir.path().join("auth.json"), "{}").unwrap();
        std::fs::write(dir.path().join("mcp-auth.json"), "{}").unwrap();
        std::fs::write(dir.path().join("trust.json"), "{}").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "ok").unwrap();

        let tool = LsTool::new(dir.path().to_path_buf());
        let res = tool
            .execute("tc1", serde_json::Map::new(), CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("visible.txt"));
        for hidden in [".mehh", ".ssh", ".env", "auth.json", "mcp-auth.json", "trust.json"] {
            assert!(!txt.contains(hidden), "must not leak {hidden}: {txt}");
        }

        let explicit = json!({ "path": ".mehh" }).as_object().unwrap().clone();
        let res2 = tool
            .execute("tc2", explicit, CancellationToken::new(), None)
            .await
            .unwrap();
        let txt2 = match &res2.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt2.contains("secret.txt"), "got: {txt2}");
    }

    #[tokio::test]
    async fn test_ls_marks_symlinked_dir_with_trailing_slash() {
        let dir = crate::TempTestDir::new("ls_symlink_dir");
        let target_dir = dir.path().join("real_dir");
        std::fs::create_dir_all(&target_dir).unwrap();
        let link = dir.path().join("dir_link");

        #[cfg(unix)]
        let link_res = std::os::unix::fs::symlink(&target_dir, &link);
        #[cfg(windows)]
        let link_res = std::os::windows::fs::symlink_dir(&target_dir, &link);
        #[cfg(not(any(unix, windows)))]
        let link_res: std::io::Result<()> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported",
        ));

        if let Err(e) = link_res {
            eprintln!("skipping ls symlink dir test: {e}");
            return;
        }

        let tool = LsTool::new(dir.path().to_path_buf());
        let res = tool
            .execute("tc", serde_json::Map::new(), CancellationToken::new(), None)
            .await
            .unwrap();
        let txt = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text"),
        };
        assert!(txt.contains("dir_link/"), "expected trailing slash on symlinked dir: {txt}");
    }
}
