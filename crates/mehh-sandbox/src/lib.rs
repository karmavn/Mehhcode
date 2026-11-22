//! Kernel-level sandbox planning for shell tools.
//!
//! The policy is deliberately separate from approval: approval decides
//! whether a command may run, this module decides what it can reach once it
//! runs. Every backend reports whether isolation is actually active; a
//! degraded backend never pretends to be safe.

use std::path::{Path, PathBuf};

/// Access level granted to a spawned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Read workspace and system, no writes, no network.
    ReadOnly,
    /// Write only inside `cwd` (plus its own temp area), no network by default.
    WorkspaceWrite,
    /// Explicitly opted out. No kernel restrictions are applied.
    DangerFullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError(String);

impl SandboxError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SandboxError {}

/// What isolation is really in force for a spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    Active(String),
    Degraded(String),
}

impl SandboxStatus {
    pub fn degraded_reason(&self) -> Option<&str> {
        match self {
            SandboxStatus::Active(_) => None,
            SandboxStatus::Degraded(reason) => Some(reason),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, SandboxStatus::Active(_))
    }
}

/// A spawn after policy has been applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub status: SandboxStatus,
}

#[derive(Debug, Clone)]
pub struct SandboxPlan {
    pub policy: SandboxPolicy,
    pub cwd: PathBuf,
    /// Paths mounted read-only or masked even when they do not exist yet.
    pub protected_paths: Vec<PathBuf>,
    /// Paths whose contents must never be readable by the child.
    pub deny_read_paths: Vec<PathBuf>,
    pub env_denylist: Vec<String>,
    pub network: bool,
}

const PROTECTED_NAMES: &[&str] = &[".git", ".mehh", ".agents", ".codex"];
const MAX_ENV_FILES: usize = 256;
const MAX_WALK_DEPTH: usize = 6;

impl SandboxPlan {
    pub fn new(policy: SandboxPolicy, cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let mut protected_paths: Vec<PathBuf> = PROTECTED_NAMES
            .iter()
            .map(|name| cwd.join(name))
            .collect();
        let mut deny_read_paths = Vec::new();

        if let Some(home) = home_dir() {
            deny_read_paths.push(home.join(".ssh"));
            deny_read_paths.push(home.join(".aws"));
            deny_read_paths.push(home.join(".mehh").join("agent").join("auth.json"));
            protected_paths.push(home.join(".mehh").join("agent").join("permissions.json"));
        }
        deny_read_paths.extend(find_env_files(&cwd));

        Self {
            policy,
            cwd,
            protected_paths,
            deny_read_paths,
            env_denylist: default_env_denylist(),
            network: matches!(policy, SandboxPolicy::DangerFullAccess),
        }
    }

    /// Fail-closed check before any ACL or mount is handed to a child.
    pub fn prepare(&self) -> Result<PreparedSandbox, SandboxError> {
        let cwd = self
            .cwd
            .canonicalize()
            .map_err(|e| SandboxError::new(format!("cannot canonicalize cwd: {e}")))?;
        if !cwd.is_dir() {
            return Err(SandboxError::new("cwd is not a directory"));
        }
        for path in self
            .protected_paths
            .iter()
            .chain(self.deny_read_paths.iter())
        {
            if path_through_writable_symlink(path, &cwd)? {
                return Err(SandboxError::new(format!(
                    "refusing sandbox: protected path {} is reachable through a writable symlink",
                    path.display()
                )));
            }
        }
        Ok(PreparedSandbox { cwd })
    }
}

pub struct PreparedSandbox {
    cwd: PathBuf,
}

impl PreparedSandbox {
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Walk a bounded tree for `.env` files. Bounded depth and match count keep
/// startup predictable on large repositories.
fn find_env_files(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_WALK_DEPTH || out.len() >= MAX_ENV_FILES {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s == ".env" || name_s.ends_with(".env") {
                out.push(path.clone());
                if out.len() >= MAX_ENV_FILES {
                    return;
                }
            }
            if path.is_dir() && !path.is_symlink() {
                visit(&path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(root, 0, &mut out);
    out
}

/// A protected path that crosses a symlink located inside the writable root
/// can be redirected by the child after policy evaluation (TOCTTOU).
fn path_through_writable_symlink(path: &Path, writable_root: &Path) -> Result<bool, SandboxError> {
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(cursor)
                    .map_err(|e| SandboxError::new(format!("cannot read symlink {}: {e}", cursor.display())))?;
                let resolved = if target.is_absolute() {
                    target
                } else {
                    cursor.parent().unwrap_or(Path::new(".")).join(target)
                };
                if resolved.starts_with(writable_root) {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(SandboxError::new(format!(
                    "cannot inspect {}: {e}",
                    cursor.display()
                )))
            }
        }
        if cursor == writable_root || cursor.parent() == Some(writable_root) {
            break;
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    Ok(false)
}

pub fn default_env_denylist() -> Vec<String> {
    vec![
        "*_API_KEY".into(),
        "*_SECRET".into(),
        "*_TOKEN".into(),
        "AWS_*".into(),
        "GITHUB_TOKEN".into(),
        "GH_TOKEN".into(),
        "OPENAI_API_KEY".into(),
        "ANTHROPIC_API_KEY".into(),
        "GOOGLE_API_KEY".into(),
    ]
}

/// Keys matching the denylist are removed before a child is spawned so build
/// scripts and `npm test` cannot exfiltrate the agent's credentials.
pub fn is_secret_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_SECRET")
        || upper.ends_with("_TOKEN")
        || upper.starts_with("AWS_")
        || matches!(
            upper.as_str(),
            "GITHUB_TOKEN"
                | "GH_TOKEN"
                | "OPENAI_API_KEY"
                | "ANTHROPIC_API_KEY"
                | "GOOGLE_API_KEY"
                | "MEHH_API_KEY"
        )
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn linux_wrapper(plan: &SandboxPlan, prepared: &PreparedSandbox) -> Option<(String, Vec<String>)> {
    let bwrap = find_in_path("bwrap")?;
    let mut args = vec![
        "--new-session".to_string(),
        "--die-with-parent".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
    ];
    if !plan.network {
        args.push("--unshare-net".to_string());
    }
    args.push("--cap-drop".to_string());
    args.push("ALL".to_string());
    args.push("--ro-bind".to_string());
    args.push("/".to_string());
    args.push("/".to_string());
    args.push("--dev".to_string());
    args.push("/dev".to_string());
    args.push("--bind".to_string());
    let cwd = prepared.cwd().to_string_lossy().into_owned();
    args.push(cwd.clone());
    args.push(cwd.clone());

    for path in &plan.protected_paths {
        let rendered = path.to_string_lossy().into_owned();
        if path.exists() {
            args.push("--ro-bind".to_string());
            args.push(rendered.clone());
            args.push(rendered);
        } else {
            // Mask paths that do not exist yet: without this the child can
            // create `.git/hooks` or a fake permissions file itself.
            args.push("--tmpfs".to_string());
            args.push(rendered);
        }
    }
    for path in &plan.deny_read_paths {
        let rendered = path.to_string_lossy().into_owned();
        args.push("--tmpfs".to_string());
        args.push(rendered);
    }
    // WSL interop lets a Linux process escape by calling powershell.exe.
    for wsl_path in ["/run/WSL", "/mnt/wslg/distro"] {
        if Path::new(wsl_path).exists() {
            args.push("--tmpfs".to_string());
            args.push(wsl_path.to_string());
        }
    }
    args.push("--".to_string());
    Some((bwrap.to_string_lossy().into_owned(), args))
}

/// Apply the plan to a command. Never silently drops restrictions: if no
/// kernel backend is available the returned status is `Degraded`.
pub fn wrap(
    plan: &SandboxPlan,
    program: &str,
    args: &[String],
) -> Result<SandboxInvocation, SandboxError> {
    if plan.policy == SandboxPolicy::DangerFullAccess {
        return Ok(SandboxInvocation {
            program: program.to_string(),
            args: args.to_vec(),
            status: SandboxStatus::Degraded("DangerFullAccess: sandbox disabled by user".into()),
        });
    }

    let prepared = plan.prepare()?;
    #[cfg(target_os = "linux")]
    let wrapper: Option<(String, Vec<String>)> = linux_wrapper(plan, &prepared);
    #[cfg(not(target_os = "linux"))]
    let wrapper: Option<(String, Vec<String>)> = None;

    if let Some((wrapper_program, mut prefix)) = wrapper {
        prefix.push(program.to_string());
        prefix.extend(args.iter().cloned());
        return Ok(SandboxInvocation {
            program: wrapper_program,
            args: prefix,
            status: SandboxStatus::Active("bwrap user namespaces + capability drop".into()),
        });
    }

    #[cfg(target_os = "macos")]
    if let Some(profile) = seatbelt_profile(plan) {
        let path = std::env::temp_dir().join(format!(
            "mehh-sandbox-{}.sbp",
            std::process::id()
        ));
        std::fs::write(&path, profile)
            .map_err(|e| SandboxError::new(format!("cannot write sandbox profile: {e}")))?;
        let mut seatbelt = vec!["-f".to_string(), path.to_string_lossy().into_owned(), "--".to_string()];
        seatbelt.push(program.to_string());
        seatbelt.extend(args.iter().cloned());
        return Ok(SandboxInvocation {
            program: "sandbox-exec".to_string(),
            args: seatbelt,
            status: SandboxStatus::Active("Seatbelt sandbox-exec profile".into()),
        });
    }

    // Windows: no privileged service or WFP driver here. Report degraded
    // instead of pretending a token restriction is kernel isolation.
    Ok(SandboxInvocation {
        program: program.to_string(),
        args: args.to_vec(),
        status: SandboxStatus::Degraded(
            "no OS sandbox backend available; environment credentials were scrubbed".into(),
        ),
    })
}

/// Escape a value for a quoted SBPL string literal. Seatbelt uses C-style
/// escapes; an unescaped quote or backslash in a path (for example an
/// attacker-controlled `.env` filename found under the workspace) could close
/// the literal and inject extra `(allow ...)` rules.
#[cfg(any(target_os = "macos", test))]
fn escape_sbpl_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(plan: &SandboxPlan) -> Option<String> {
    let cwd = plan.cwd.canonicalize().ok()?;
    let cwd_s = escape_sbpl_string(&cwd.to_string_lossy());
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n\
         (allow sysctl-read)\n(allow file-read* (subpath \"/usr\") (subpath \"/System\"))\n\
         (allow file-write* (subpath \"",
    );
    profile.push_str(&cwd_s);
    profile.push_str("\"))\n");
    for path in plan.protected_paths.iter().chain(plan.deny_read_paths.iter()) {
        let escaped = escape_sbpl_string(&path.to_string_lossy());
        profile.push_str("(deny file-read* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n(deny file-write* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n");
    }
    Some(profile)
}

const VIOLATION_MARKERS: &[&str] = &[
    "Permission denied",
    "Read-only file system",
    "Operation not permitted",
    "Network is unreachable",
    "Bad system call",
    "SIGSYS",
    "bwrap:",
    "sandbox-exec:",
];

/// Best-effort signal that a non-zero exit was caused by the sandbox rather
/// than by the command itself. Used by the TUI to offer a one-time re-run
/// outside the sandbox; it never changes the command result on its own.
pub fn detect_violation(output: &str, exit_code: Option<i32>) -> bool {
    if exit_code == Some(0) || exit_code.is_none() {
        return false;
    }
    VIOLATION_MARKERS.iter().any(|marker| output.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_are_recognized_across_platforms() {
        for key in [
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "MY_SERVICE_SECRET",
        ] {
            assert!(is_secret_env_key(key), "{key}");
        }
        for key in ["PATH", "HOME", "USERPROFILE", "RUSTFLAGS"] {
            assert!(!is_secret_env_key(key), "{key}");
        }
    }

    #[test]
    fn sbpl_paths_cannot_inject_rules() {
        // Anchors the order: backslashes are escaped before quotes.
        assert_eq!(escape_sbpl_string("a\\b\"c"), "a\\\\b\\\"c");
        let crafted = "x.env\" (allow file-read* (subpath \"/))\\y";
        assert_eq!(
            escape_sbpl_string(crafted),
            "x.env\\\" (allow file-read* (subpath \\\"/))\\\\y"
        );
    }

    #[test]
    fn plan_masks_missing_git_directory() {
        let dir = std::env::temp_dir().join(format!("sandbox-plan-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        assert!(plan.protected_paths.contains(&dir.join(".git")));
        assert!(plan.protected_paths.contains(&dir.join(".mehh")));
        assert!(plan.prepare().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dangerous_full_access_is_reported_degraded() {
        let dir = std::env::temp_dir();
        let plan = SandboxPlan::new(SandboxPolicy::DangerFullAccess, dir.clone());
        let inv = wrap(&plan, "echo", &["hi".to_string()]).unwrap();
        assert_eq!(inv.program, "echo");
        assert!(!inv.status.is_active());
    }

    #[test]
    fn violation_needs_failure_and_marker() {
        assert!(detect_violation("Permission denied", Some(1)));
        assert!(!detect_violation("Permission denied", Some(0)));
        assert!(!detect_violation("all good", Some(1)));
    }
}
