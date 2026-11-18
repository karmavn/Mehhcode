//! Runtime tool governance pipeline: security inspection, permission modes, and repetition breakers.

use std::path::{Path, PathBuf};
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Result of inspecting a candidate tool call before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectionVerdict {
    /// Proceed with tool execution without user prompt.
    Allow,
    /// Tool requires interactive or external user approval.
    RequireApproval {
        reason: String,
        preview: Option<String>,
    },
    /// Tool call is blocked due to security or policy constraint.
    Deny {
        reason: String,
    },
}

/// Active permission mode controlling tool execution governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Executes all safe and mutating tools without approval prompts (security flags still block).
    Auto,
    /// Default interactive mode: auto-allows read-only exploration and whitelisted inspection commands,
    /// prompts for file mutations, unknown shell commands, and destructive actions.
    #[default]
    SmartApprove,
    /// Prompts for all tool calls unless explicitly marked always allow.
    Approve,
    /// Conversational exploration only: allows read-only tools, strictly denies mutating tools and shell execution.
    Chat,
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(PermissionMode::Auto),
            "smart_approve" | "smart" | "smartapprove" => Ok(PermissionMode::SmartApprove),
            "approve" => Ok(PermissionMode::Approve),
            "chat" => Ok(PermissionMode::Chat),
            other => Err(format!(
                "Invalid permission mode '{other}'. Supported modes: auto, smart_approve, approve, chat"
            )),
        }
    }
}

/// Context provided to each tool inspector.
#[derive(Debug, Clone)]
pub struct ToolInspectionContext {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Map<String, serde_json::Value>,
    pub is_read_only: Option<bool>,
    pub cwd: PathBuf,
}

/// Pluggable tool inspection component in the chain of responsibility.
#[async_trait]
pub trait ToolInspector: Send + Sync {
    fn name(&self) -> &str;
    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
}

/// Dangerous shell patterns matched by `SecurityInspector`.
static DANGEROUS_SHELL_PATTERNS: &[(&str, &str)] = &[
    (r"(?i)\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?(/|~|\$HOME)", "Recursive root or home directory deletion"),
    (r"(?i)\bmkfs(\.\w+)?\b", "Filesystem creation"),
    (r"(?i)\bdd\s+if=.*of=/dev/", "Raw block device write"),
    (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:", "Fork bomb"),
    (r"(?i)(curl|wget)\s+.*\|\s*(sh|bash|zsh)", "Piping web download directly to shell interpreter"),
    (r"(?i)(\.ssh/id_|\.mehh/agent/auth\.json)", "Accessing private keys or auth credentials"),
    (r"(?i)(Invoke-Expression|iex)\s*(\(New-Object\s+Net\.WebClient|\(Invoke-WebRequest)", "PowerShell remote script execution"),
];

/// Normalizes path for boundary and security checks.
fn normalize_path_for_check(base: &Path, target: &str) -> PathBuf {
    let p = Path::new(target);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut components = Vec::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(last) = components.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        components.pop();
                        continue;
                    }
                }
                components.push(comp);
            }
            _ => components.push(comp),
        }
    }
    let collapsed: PathBuf = components.into_iter().collect();
    #[cfg(windows)]
    {
        PathBuf::from(collapsed.to_string_lossy().to_ascii_lowercase().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        collapsed
    }
}

/// Priority 1 Inspector: Enforces invariant security constraints across ALL modes.
pub struct SecurityInspector {
    compiled_patterns: Vec<(Regex, &'static str)>,
}

impl SecurityInspector {
    pub fn new() -> Self {
        let mut compiled = Vec::new();
        for &(pat, desc) in DANGEROUS_SHELL_PATTERNS {
            if let Ok(re) = Regex::new(pat) {
                compiled.push((re, desc));
            }
        }
        Self { compiled_patterns: compiled }
    }
}

impl Default for SecurityInspector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolInspector for SecurityInspector {
    fn name(&self) -> &str {
        "security_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // 1. Guard against direct mutation of .mehh configuration and auth files
        if matches!(ctx.tool_name.as_str(), "edit" | "write" | "undo_edit") {
            if let Some(path_val) = ctx.args.get("path").and_then(|v| v.as_str()) {
                let norm = path_val.replace('\\', "/").to_ascii_lowercase();
                if norm.starts_with(".mehh/")
                    || norm == ".mehh"
                    || norm.contains("/.mehh/")
                    || norm.contains("~/.mehh/")
                    || norm.contains("auth.json")
                    || norm.contains("trust.json")
                {
                    return InspectionVerdict::Deny {
                        reason: format!("Direct mutation of .mehh configuration metadata file '{path_val}' is blocked by security policy."),
                    };
                }

                let target_path = normalize_path_for_check(&ctx.cwd, path_val);
                let cwd_norm = normalize_path_for_check(&ctx.cwd, "");
                if !target_path.starts_with(&cwd_norm) {
                    return InspectionVerdict::RequireApproval {
                        reason: format!("File modification outside current working directory requires approval: '{path_val}'"),
                        preview: Some(path_val.to_string()),
                    };
                }
            }
        }

        // 2. Guard against catastrophic shell commands
        if matches!(ctx.tool_name.as_str(), "bash" | "powershell") {
            if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                for (re, desc) in &self.compiled_patterns {
                    if re.is_match(cmd) {
                        return InspectionVerdict::Deny {
                            reason: format!("Command blocked by security policy: {desc} ('{cmd}')"),
                        };
                    }
                }
            }
        }

        InspectionVerdict::Allow
    }
}

/// Priority 2 Inspector: Enforces user permission mode and granular rule overrides.
pub struct PermissionInspector {
    mode: PermissionMode,
    safe_commands: Vec<Regex>,
    store: crate::permissions::PermissionStore,
}

impl PermissionInspector {
    pub fn new(mode: PermissionMode) -> Self {
        Self::new_with_store(mode, crate::permissions::PermissionStore::default())
    }

    pub fn new_with_store(mode: PermissionMode, store: crate::permissions::PermissionStore) -> Self {
        let safe_cmd_patterns = &[
            r"^(git\s+(status|diff|log|show|branch|rev-parse))",
            r"^(cargo\s+(check|clippy|test|tree|version))",
            r"^(ls|dir|pwd|cat|find|which|echo)(\s+.*)?$",
            r"^(Get-ChildItem|Get-Location|Get-Content|Select-String)(\s+.*)?$",
        ];
        let mut compiled = Vec::new();
        for &p in safe_cmd_patterns {
            if let Ok(re) = Regex::new(p) {
                compiled.push(re);
            }
        }
        Self {
            mode,
            safe_commands: compiled,
            store,
        }
    }

    pub fn store(&self) -> &crate::permissions::PermissionStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut crate::permissions::PermissionStore {
        &mut self.store
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    fn is_safe_shell_command(&self, cmd: &str) -> bool {
        let trimmed = cmd.trim();
        // Check for shell chaining or output redirection that could mutate
        if trimmed.contains('>') || trimmed.contains(">>") || trimmed.contains('|') || trimmed.contains(';') || trimmed.contains("&&") {
            return false;
        }
        self.safe_commands.iter().any(|re| re.is_match(trimmed))
    }
}

#[async_trait]
impl ToolInspector for PermissionInspector {
    fn name(&self) -> &str {
        "permission_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // 0. Explicit rule matching from persisted permissions
        if let Some(rule_verdict) = self.store.evaluate(ctx) {
            return rule_verdict;
        }

        match self.mode {
            PermissionMode::Auto => InspectionVerdict::Allow,
            PermissionMode::Chat => {
                if ctx.is_read_only == Some(true) || ctx.tool_name == "todo" || ctx.tool_name == "tool_search" {
                    InspectionVerdict::Allow
                } else {
                    InspectionVerdict::Deny {
                        reason: format!("Tool '{}' is blocked in Chat mode (only read-only exploration tools are permitted).", ctx.tool_name),
                    }
                }
            }
            PermissionMode::Approve => {
                InspectionVerdict::RequireApproval {
                    reason: format!("Mode 'approve' requires confirmation for tool '{}'", ctx.tool_name),
                    preview: ctx.args.get("command").or_else(|| ctx.args.get("path")).and_then(|v| v.as_str()).map(str::to_string),
                }
            }
            PermissionMode::SmartApprove => {
                // Read-only tools proceed automatically
                if ctx.is_read_only == Some(true) || ctx.tool_name == "todo" || ctx.tool_name == "tool_search" {
                    return InspectionVerdict::Allow;
                }

                // Check whitelisted shell commands
                if matches!(ctx.tool_name.as_str(), "bash" | "powershell") {
                    if let Some(cmd) = ctx.args.get("command").and_then(|v| v.as_str()) {
                        if self.is_safe_shell_command(cmd) {
                            return InspectionVerdict::Allow;
                        }
                        return InspectionVerdict::RequireApproval {
                            reason: format!("Shell command requires approval: '{cmd}'"),
                            preview: Some(cmd.to_string()),
                        };
                    }
                }

                // File mutations require approval
                if matches!(ctx.tool_name.as_str(), "edit" | "write" | "undo_edit") {
                    let path_str = ctx.args.get("path").and_then(|v| v.as_str()).unwrap_or("(unknown)");
                    return InspectionVerdict::RequireApproval {
                        reason: format!("Tool '{}' on '{}' requires approval", ctx.tool_name, path_str),
                        preview: Some(path_str.to_string()),
                    };
                }

                // Unannotated or unknown tool calls default to requiring approval
                InspectionVerdict::RequireApproval {
                    reason: format!("Tool '{}' requires approval", ctx.tool_name),
                    preview: None,
                }
            }
        }
    }
}

/// State tracking consecutive identical tool calls to prevent runaway loops.
#[derive(Debug, Default)]
struct RepetitionState {
    last_key: Option<String>,
    count: usize,
}

/// Priority 3 Inspector: Halts identical repeated tool calls after `max_repetitions`.
pub struct RepetitionInspector {
    max_repetitions: usize,
    state: Mutex<RepetitionState>,
}

impl RepetitionInspector {
    pub fn new(max_repetitions: usize) -> Self {
        Self {
            max_repetitions,
            state: Mutex::new(RepetitionState::default()),
        }
    }
}

impl Default for RepetitionInspector {
    fn default() -> Self {
        Self::new(3)
    }
}

#[async_trait]
impl ToolInspector for RepetitionInspector {
    fn name(&self) -> &str {
        "repetition_inspector"
    }

    async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        let key = format!("{}:{}", ctx.tool_name, serde_json::to_string(&ctx.args).unwrap_or_default());
        let mut st = self.state.lock().await;

        if st.last_key.as_deref() == Some(&key) {
            st.count += 1;
            if st.count > self.max_repetitions {
                return InspectionVerdict::Deny {
                    reason: format!(
                        "Repeated identical tool call '{}' detected ({} times). Change strategy or inspect prior output instead of repeating the exact same call.",
                        ctx.tool_name, st.count
                    ),
                };
            }
        } else {
            st.last_key = Some(key);
            st.count = 1;
        }

        InspectionVerdict::Allow
    }
}

/// Manager orchestrating the priority chain of tool inspectors.
pub struct ToolInspectionManager {
    mode: Mutex<PermissionMode>,
    security: SecurityInspector,
    permission: Mutex<PermissionInspector>,
    repetition: RepetitionInspector,
    cwd: PathBuf,
}

impl ToolInspectionManager {
    pub fn new(mode: PermissionMode, cwd: PathBuf) -> Self {
        Self::new_with_store(
            mode,
            cwd.clone(),
            crate::permissions::PermissionStore::load(&cwd, false).unwrap_or_default(),
        )
    }

    pub fn new_with_store(
        mode: PermissionMode,
        cwd: PathBuf,
        store: crate::permissions::PermissionStore,
    ) -> Self {
        Self {
            mode: Mutex::new(mode),
            security: SecurityInspector::new(),
            permission: Mutex::new(PermissionInspector::new_with_store(mode, store)),
            repetition: RepetitionInspector::default(),
            cwd,
        }
    }

    pub async fn add_permission_rule(&self, rule: crate::permissions::PermissionRule) -> anyhow::Result<()> {
        let mut perm = self.permission.lock().await;
        perm.store_mut().add_rule(rule)
    }

    pub async fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().await = mode;
        self.permission.lock().await.set_mode(mode);
    }

    pub async fn mode(&self) -> PermissionMode {
        *self.mode.lock().await
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict {
        // Priority 1: Security Inspector (runs in all modes, strictly authoritative)
        let sec_verdict = self.security.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = sec_verdict {
            return sec_verdict;
        }
        if let InspectionVerdict::RequireApproval { .. } = sec_verdict {
            return sec_verdict;
        }

        // Priority 2: Permission Inspector
        let perm_verdict = self.permission.lock().await.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = perm_verdict {
            return perm_verdict;
        }

        // Priority 3: Repetition Inspector
        let rep_verdict = self.repetition.inspect(ctx).await;
        if let InspectionVerdict::Deny { .. } = rep_verdict {
            return rep_verdict;
        }

        // If any inspector requested approval, return RequireApproval
        if let InspectionVerdict::RequireApproval { .. } = perm_verdict {
            return perm_verdict;
        }

        InspectionVerdict::Allow
    }

    /// Evaluates inspectors and queries the approval handler if `RequireApproval` is reached.
    pub async fn inspect_and_authorize(
        &self,
        ctx: &ToolInspectionContext,
        handler: Option<&dyn ApprovalHandler>,
    ) -> mehh_agent::BeforeToolCallResult {
        match self.inspect(ctx).await {
            InspectionVerdict::Allow => mehh_agent::BeforeToolCallResult::default(),
            InspectionVerdict::Deny { reason } => mehh_agent::BeforeToolCallResult {
                block: true,
                reason: Some(reason),
                terminate: false,
            },
            InspectionVerdict::RequireApproval { reason, preview } => {
                if let Some(h) = handler {
                    if h.request_approval(ctx, &reason, preview.as_deref()).await {
                        return mehh_agent::BeforeToolCallResult::default();
                    }
                }
                mehh_agent::BeforeToolCallResult {
                    block: true,
                    reason: Some(format!(
                        "Tool '{}' blocked: approval required but not granted ({})",
                        ctx.tool_name, reason
                    )),
                    terminate: false,
                }
            }
        }
    }
}

/// Handler deciding whether to authorize a tool call requiring user approval.
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request_approval(
        &self,
        ctx: &ToolInspectionContext,
        reason: &str,
        preview: Option<&str>,
    ) -> bool;
}

/// Headless approval handler: fails closed on any approval requirement.
pub struct HeadlessDenyHandler;

#[async_trait]
impl ApprovalHandler for HeadlessDenyHandler {
    async fn request_approval(
        &self,
        _ctx: &ToolInspectionContext,
        _reason: &str,
        _preview: Option<&str>,
    ) -> bool {
        false
    }
}
