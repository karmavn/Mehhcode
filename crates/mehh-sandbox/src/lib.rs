//! Kernel-level sandbox planning for shell tools.
//!
//! The policy is deliberately separate from approval: approval decides
//! whether a command may run, this module decides what it can reach once it
//! runs. Every backend reports whether isolation is actually active; a
//! degraded backend never pretends to be safe.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", test))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Mutex;

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
#[must_use]
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
    pub network: bool,
}

const PROTECTED_NAMES: &[&str] = &[".git", ".mehh", ".agents", ".codex"];
const SKIP_SCAN_DIR_NAMES: &[&str] = &["target", "node_modules", ".git", ".venv", "dist", "build"];
const MAX_ENV_FILES: usize = 256;
const MAX_WALK_DEPTH: usize = 6;

fn strip_verbatim_for_cmp(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        let bytes = s.as_bytes();
        let normalized = if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(br"\\?\UNC\") {
            format!(r"\\{}", &s[8..])
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s.into_owned()
        };
        PathBuf::from(normalized.to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn component_eq_win(a: std::path::Component<'_>, b: std::path::Component<'_>) -> bool {
    let sa = a.as_os_str().to_string_lossy();
    let sb = b.as_os_str().to_string_lossy();
    if sa.is_ascii() && sb.is_ascii() {
        sa.eq_ignore_ascii_case(&sb)
    } else {
        sa.to_lowercase() == sb.to_lowercase()
    }
}

fn path_starts_with_norm(path: &Path, norm_root: &Path) -> bool {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        let bytes = raw.as_bytes();
        let stripped: std::borrow::Cow<'_, str> =
            if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(br"\\?\UNC\") {
                std::borrow::Cow::Owned(format!(r"\\{}", &raw[8..]))
            } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
                match raw {
                    std::borrow::Cow::Borrowed(b) => std::borrow::Cow::Borrowed(&b[4..]),
                    std::borrow::Cow::Owned(_) => std::borrow::Cow::Owned(rest.to_string()),
                }
            } else {
                raw
            };
        let mut p_comps = Path::new(stripped.as_ref()).components();
        for rc in norm_root.components() {
            match p_comps.next() {
                Some(pc) if component_eq_win(pc, rc) => {}
                _ => return false,
            }
        }
        true
    }
    #[cfg(not(windows))]
    {
        path.starts_with(norm_root)
    }
}

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
            deny_read_paths.push(home.join(".mehh").join("agent").join("mcp-auth.json"));
            protected_paths.push(home.join(".mehh").join("agent").join("permissions.json"));
            protected_paths.push(home.join(".mehh").join("agent").join("trust.json"));
        }
        if policy != SandboxPolicy::DangerFullAccess {
            deny_read_paths.extend(find_env_files(&cwd));
        }

        Self {
            policy,
            cwd,
            protected_paths,
            deny_read_paths,
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
        let mut protected_paths = Vec::with_capacity(self.protected_paths.len());
        for path in &self.protected_paths {
            let reanchored = if self.cwd != cwd {
                path.strip_prefix(&self.cwd)
                    .ok()
                    .map(|rel| cwd.join(rel))
            } else {
                None
            };
            let check_path = reanchored.as_deref().unwrap_or(path);
            let (through_symlink, resolved) =
                path_through_writable_symlink_and_resolve(check_path, &cwd)?;
            if through_symlink {
                return Err(SandboxError::new(format!(
                    "refusing sandbox: protected path {} is reachable through a writable symlink",
                    path.display()
                )));
            }
            protected_paths.push(resolved);
        }
        let mut deny_read_paths = Vec::with_capacity(self.deny_read_paths.len());
        for path in &self.deny_read_paths {
            let reanchored = if self.cwd != cwd {
                path.strip_prefix(&self.cwd)
                    .ok()
                    .map(|rel| cwd.join(rel))
            } else {
                None
            };
            let check_path = reanchored.as_deref().unwrap_or(path);
            let (through_symlink, resolved) =
                path_through_writable_symlink_and_resolve(check_path, &cwd)?;
            if through_symlink {
                return Err(SandboxError::new(format!(
                    "refusing sandbox: protected path {} is reachable through a writable symlink",
                    path.display()
                )));
            }
            deny_read_paths.push(resolved);
        }
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            for p in std::iter::once(&cwd)
                .chain(protected_paths.iter())
                .chain(deny_read_paths.iter())
            {
                let Some(s) = p.to_str() else {
                    return Err(SandboxError::new(format!(
                        "refusing sandbox: non-UTF-8 path {}",
                        p.display()
                    )));
                };
                if s.contains('\0') {
                    return Err(SandboxError::new("refusing sandbox: path contains NUL byte"));
                }
            }
        }
        Ok(PreparedSandbox {
            cwd,
            protected_paths,
            deny_read_paths,
        })
    }
}

pub struct PreparedSandbox {
    cwd: PathBuf,
    protected_paths: Vec<PathBuf>,
    deny_read_paths: Vec<PathBuf>,
}

impl PreparedSandbox {
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn protected_paths(&self) -> &[PathBuf] {
        &self.protected_paths
    }
    pub fn deny_read_paths(&self) -> &[PathBuf] {
        &self.deny_read_paths
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()))
            .map(PathBuf::from)
    }
}

#[inline]
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    let sb = s.as_bytes();
    let pb = prefix.as_bytes();
    sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb)
}

#[inline]
fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    let sb = s.as_bytes();
    let xb = suffix.as_bytes();
    sb.len() >= xb.len() && sb[sb.len() - xb.len()..].eq_ignore_ascii_case(xb)
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
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            let is_env = name_s.eq_ignore_ascii_case(".env")
                || starts_with_ignore_ascii_case(&name_s, ".env.")
                || ends_with_ignore_ascii_case(&name_s, ".env");
            let should_recurse = ft.is_dir()
                && !ft.is_symlink()
                && !SKIP_SCAN_DIR_NAMES
                    .iter()
                    .any(|skip| name_s.eq_ignore_ascii_case(skip));
            if !is_env && !should_recurse {
                continue;
            }
            let path = entry.path();
            if should_recurse {
                if is_env {
                    out.push(path.clone());
                    if out.len() >= MAX_ENV_FILES {
                        return;
                    }
                }
                visit(&path, depth + 1, out);
            } else if is_env {
                out.push(path);
                if out.len() >= MAX_ENV_FILES {
                    return;
                }
            }
        }
    }
    let mut out = Vec::new();
    visit(root, 0, &mut out);
    out
}

/// A protected path that crosses a symlink located inside the writable root
/// (or an external symlink pointing into or hopping through the writable root)
/// can be redirected by the child after policy evaluation (TOCTTOU).
fn path_through_writable_symlink_and_resolve(
    path: &Path,
    writable_root: &Path,
) -> Result<(bool, PathBuf), SandboxError> {
    const MAX_SYMLINK_HOPS: usize = 40;

    enum WalkStep {
        Prefix(OsString),
        RootDir,
        ParentDir,
        Normal(OsString),
        ExitSymlink,
    }

    fn collect_steps(p: &Path) -> Vec<WalkStep> {
        let mut steps = Vec::new();
        for comp in p.components() {
            match comp {
                std::path::Component::Prefix(prefix) => {
                    steps.push(WalkStep::Prefix(prefix.as_os_str().to_os_string()));
                }
                std::path::Component::RootDir => steps.push(WalkStep::RootDir),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => steps.push(WalkStep::ParentDir),
                std::path::Component::Normal(seg) => {
                    steps.push(WalkStep::Normal(seg.to_os_string()));
                }
            }
        }
        steps
    }

    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| SandboxError::new(format!("cannot resolve cwd: {e}")))?
    };

    let norm_root = strip_verbatim_for_cmp(writable_root);

    let mut pending: VecDeque<WalkStep> = collect_steps(&anchored).into();
    let mut cur = PathBuf::new();
    let mut missing = false;
    let mut symlink_hops = 0usize;
    let mut active_symlink_depth = 0usize;

    while let Some(step) = pending.pop_front() {
        match step {
            WalkStep::Prefix(p) => cur.push(p),
            WalkStep::RootDir => {
                cur.push(std::path::Component::RootDir.as_os_str());
                if let Ok(canon) = cur.canonicalize() {
                    cur = canon;
                }
            }
            WalkStep::ParentDir => {
                cur.pop();
                if cur.exists() {
                    if let Ok(canon) = cur.canonicalize() {
                        cur = canon;
                        missing = false;
                    }
                }
                if active_symlink_depth > 0 && path_starts_with_norm(&cur, &norm_root) {
                    return Ok((true, cur));
                }
            }
            WalkStep::ExitSymlink => {
                if active_symlink_depth > 0 && path_starts_with_norm(&cur, &norm_root) {
                    return Ok((true, cur));
                }
                active_symlink_depth = active_symlink_depth.saturating_sub(1);
            }
            WalkStep::Normal(seg) => {
                let next = cur.join(&seg);
                if active_symlink_depth > 0 && path_starts_with_norm(&next, &norm_root) {
                    return Ok((true, next));
                }
                if missing {
                    cur = next;
                    continue;
                }
                match std::fs::symlink_metadata(&next) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        if path_starts_with_norm(&cur, &norm_root)
                            || path_starts_with_norm(&next, &norm_root)
                        {
                            return Ok((true, next));
                        }
                        symlink_hops += 1;
                        if symlink_hops > MAX_SYMLINK_HOPS {
                            return Err(SandboxError::new(format!(
                                "refusing sandbox: symlink loop or excessive depth at {}",
                                next.display()
                            )));
                        }
                        let target = std::fs::read_link(&next).map_err(|e| {
                            SandboxError::new(format!("cannot read symlink {}: {e}", next.display()))
                        })?;
                        let mut target_steps = collect_steps(&target);
                        if target.is_absolute() || target.has_root() {
                            cur = PathBuf::new();
                            missing = false;
                        }
                        pending.push_front(WalkStep::ExitSymlink);
                        while let Some(s) = target_steps.pop() {
                            pending.push_front(s);
                        }
                        active_symlink_depth += 1;
                    }
                    Ok(_) => {
                        if path_starts_with_norm(&cur, &norm_root) {
                            cur = next;
                        } else {
                            cur = next.canonicalize().unwrap_or(next);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        missing = true;
                        cur = next;
                    }
                    Err(e) => {
                        return Err(SandboxError::new(format!(
                            "cannot inspect {}: {e}",
                            next.display()
                        )));
                    }
                }
            }
        }
    }
    Ok((false, cur))
}

/// Keys matching the denylist are removed before a child is spawned so build
/// scripts and `npm test` cannot exfiltrate the agent's credentials.
pub fn is_secret_env_key(key: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_APIKEY",
        "_SECRET",
        "_SECRET_KEY",
        "_ACCESS_KEY",
        "_STORAGE_KEY",
        "_TOKEN",
        "_PASSWORD",
        "_PASSWD",
        "_PRIVATE_KEY",
        "_CREDENTIALS",
        "_DATABASE_URL",
        "_REDIS_URL",
        "_CONNECTION_STRING",
        "_DSN",
    ];
    const EXACT: &[&str] = &[
        "API_KEY",
        "APIKEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "PRIVATE_KEY",
        "DATABASE_URL",
        "REDIS_URL",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "CONNECTION_STRING",
        "DSN",
        "SSH_AUTH_SOCK",
        "GPG_AGENT_INFO",
        "WSL_INTEROP",
        "DOCKER_AUTH_CONFIG",
    ];

    starts_with_ignore_ascii_case(key, "AWS_")
        || SUFFIXES
            .iter()
            .any(|suffix| ends_with_ignore_ascii_case(key, suffix))
        || EXACT.iter().any(|exact| key.eq_ignore_ascii_case(exact))
}

/// How the cwd is exposed to a child. Derived once so both backends agree.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CwdAccess {
    ReadOnly,
    Writable,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn cwd_access(policy: SandboxPolicy) -> CwdAccess {
    match policy {
        SandboxPolicy::ReadOnly => CwdAccess::ReadOnly,
        // DangerFullAccess is short-circuited in wrap(); mapping it here keeps
        // this helper total without weakening either restricted policy.
        SandboxPolicy::WorkspaceWrite | SandboxPolicy::DangerFullAccess => CwdAccess::Writable,
    }
}

#[cfg(target_os = "linux")]
fn find_in_path(binary: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(binary);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_wrapper(plan: &SandboxPlan, prepared: &PreparedSandbox) -> Option<(String, Vec<String>)> {
    let bwrap = find_in_path("bwrap")?;
    Some((
        bwrap.to_string_lossy().into_owned(),
        linux_wrapper_args(plan, prepared),
    ))
}

#[cfg(any(target_os = "linux", test))]
fn linux_wrapper_args(plan: &SandboxPlan, prepared: &PreparedSandbox) -> Vec<String> {
    let writable_cwd = cwd_access(plan.policy) == CwdAccess::Writable;
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
    args.push("--proc".to_string());
    args.push("/proc".to_string());
    if writable_cwd {
        args.push("--tmpfs".to_string());
        args.push("/tmp".to_string());
    }
    let cwd = prepared.cwd().to_string_lossy().into_owned();
    let cwd_bind = if writable_cwd {
        "--bind"
    } else {
        "--ro-bind"
    };
    args.push(cwd_bind.to_string());
    args.push(cwd.clone());
    args.push(cwd);

    for path in prepared.protected_paths() {
        let rendered = path.to_string_lossy().into_owned();
        if path.exists() {
            args.push("--ro-bind".to_string());
            args.push(rendered.clone());
            args.push(rendered);
        }
    }
    for path in prepared.deny_read_paths() {
        let rendered = path.to_string_lossy().into_owned();
        if path.is_file() {
            args.push("--ro-bind".to_string());
            args.push("/dev/null".to_string());
            args.push(rendered);
        } else if path.is_dir() {
            args.push("--tmpfs".to_string());
            args.push(rendered);
        }
    }
    // WSL interop lets a Linux process escape by calling powershell.exe.
    for wsl_path in ["/run/WSL", "/mnt/wslg/distro"] {
        if Path::new(wsl_path).exists() {
            args.push("--tmpfs".to_string());
            args.push(wsl_path.to_string());
        }
    }
    args.push("--".to_string());
    args
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

    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(unused_variables)
    )]
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
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let profile = seatbelt_profile(plan, &prepared);
        let path = seatbelt_profile_path();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| SandboxError::new(format!("cannot create sandbox profile: {e}")))?;
        if let Err(e) = file.write_all(profile.as_bytes()) {
            let _ = std::fs::remove_file(&path);
            return Err(SandboxError::new(format!("cannot write sandbox profile: {e}")));
        }
        {
            // The caller spawns after wrap() returns, so profiles stay guarded
            // for a short grace period instead of being deleted too early.
            let now = std::time::Instant::now();
            let mut guards = PROFILE_GUARDS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guards.retain(|(created, _)| now.duration_since(*created) < PROFILE_RETAIN);
            guards.push((now, ProfileGuard(path.clone())));
        }
        #[cfg(target_os = "macos")]
        {
            static LAST_PRUNE_EPOCH_SECS: AtomicU64 = AtomicU64::new(0);
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let prev = LAST_PRUNE_EPOCH_SECS.load(Ordering::Relaxed);
            if now_secs.saturating_sub(prev) >= 60
                && LAST_PRUNE_EPOCH_SECS
                    .compare_exchange(prev, now_secs, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                prune_stale_seatbelt_profiles(&std::env::temp_dir(), PROFILE_STALE_AGE);
            }
        }
        let mut seatbelt = vec!["-f".to_string(), path.to_string_lossy().into_owned(), "--".to_string()];
        seatbelt.push(program.to_string());
        seatbelt.extend(args.iter().cloned());
        return Ok(SandboxInvocation {
            program: "/usr/bin/sandbox-exec".to_string(),
            args: seatbelt,
            status: SandboxStatus::Active("Seatbelt sandbox-exec profile".into()),
        });
    }

    // Windows / fallback: no privileged service or WFP driver here. Report
    // degraded instead of pretending a token restriction is kernel isolation.
    #[cfg(not(target_os = "macos"))]
    Ok(SandboxInvocation {
        program: program.to_string(),
        args: args.to_vec(),
        status: SandboxStatus::Degraded("no OS sandbox backend available".into()),
    })
}

#[cfg(target_os = "macos")]
const PROFILE_RETAIN: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(target_os = "macos")]
const PROFILE_STALE_AGE: std::time::Duration = std::time::Duration::from_secs(600);

#[cfg(any(target_os = "macos", test))]
fn prune_stale_seatbelt_profiles(dir: &Path, max_age: std::time::Duration) {
    let my_pid = std::process::id();
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(max_age) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(rest) = name_str
            .strip_prefix("mehh-sandbox-")
            .and_then(|s| s.strip_suffix(".sbp"))
        else {
            continue;
        };
        let Some((pid_str, _counter)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        if meta.modified().is_ok_and(|m| m <= cutoff) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
static PROFILE_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static PROFILE_GUARDS: Mutex<Vec<(std::time::Instant, ProfileGuard)>> = Mutex::new(Vec::new());

#[cfg(any(target_os = "macos", test))]
struct ProfileGuard(PathBuf);

#[cfg(any(target_os = "macos", test))]
impl Drop for ProfileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Each call gets its own path so concurrent spawns cannot swap allowlists.
#[cfg(any(target_os = "macos", test))]
fn seatbelt_profile_path() -> PathBuf {
    let id = PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mehh-sandbox-{}-{id}.sbp", std::process::id()))
}

/// Escape a value for a quoted SBPL string literal. Seatbelt uses C-style
/// escapes; an unescaped quote, backslash, or control character in a path
/// could close the literal or corrupt the profile.
#[cfg(any(target_os = "macos", test))]
fn escape_sbpl_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(any(target_os = "macos", test))]
fn seatbelt_profile(plan: &SandboxPlan, prepared: &PreparedSandbox) -> String {
    let cwd_s = escape_sbpl_string(&prepared.cwd().to_string_lossy());
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow signal (target self))\n\
         (allow file-read-metadata)\n\
         (allow file-read* (literal \"/tmp\") (literal \"/etc\") (literal \"/var\"))\n\
         (allow file-read* (subpath \"/usr\") (subpath \"/System\") \
         (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") \
         (subpath \"/Library\") (subpath \"/private/etc\") \
         (subpath \"/private/tmp\") (subpath \"/opt/homebrew\"))\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\"))\n",
    );
    profile.push_str("(allow file-read* (subpath \"");
    profile.push_str(&cwd_s);
    profile.push_str("\"))\n");
    if cwd_access(plan.policy) == CwdAccess::Writable {
        profile.push_str("(allow file-write* (subpath \"");
        profile.push_str(&cwd_s);
        profile.push_str("\") (subpath \"/private/tmp\"))\n");
    }
    if plan.network {
        profile.push_str("(allow network*)\n");
    }
    for path in prepared.protected_paths() {
        let escaped = escape_sbpl_string(&path.to_string_lossy());
        profile.push_str("(deny file-write* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n");
    }
    for path in prepared.deny_read_paths() {
        let escaped = escape_sbpl_string(&path.to_string_lossy());
        profile.push_str("(deny file-read* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n(deny file-write* (subpath \"");
        profile.push_str(&escaped);
        profile.push_str("\"))\n");
    }
    profile
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
    if exit_code == Some(0) {
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
        for key in [
            "DB_PASSWORD",
            "SSH_PASSWD",
            "SSH_PRIVATE_KEY",
            "DATABASE_URL",
            "REDIS_URL",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CONNECTION_STRING",
        ] {
            assert!(is_secret_env_key(key), "secret key not recognized: {key}");
        }
    }

    #[test]
    fn bare_secret_keys_are_recognized() {
        for key in [
            "API_KEY",
            "SECRET",
            "TOKEN",
            "PASSWORD",
            "PASSWD",
            "PRIVATE_KEY",
            "api_key",
            "Token",
        ] {
            assert!(is_secret_env_key(key), "bare secret key not recognized: {key}");
        }
        assert!(!is_secret_env_key("PATH"));
    }

    #[test]
    fn sandbox_find_env_files_catches_env_local_and_skips_target_dir() {
        let root = std::env::temp_dir().join(format!("sandbox_env_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for rel in [
            ".env",
            ".env.local",
            ".env.production",
            ".env.development",
            "app.env",
            "target/.env",
            "node_modules/.env",
            ".git/.env",
            ".venv/.env",
            "dist/.env",
            "build/.env",
            "src/nested/.env.local",
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"SECRET=1").unwrap();
        }

        let found = find_env_files(&root);
        let rels: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        for expected in [
            ".env",
            ".env.local",
            ".env.production",
            ".env.development",
            "app.env",
            "src/nested/.env.local",
        ] {
            assert!(rels.iter().any(|r| r == expected), "missing {expected} in {rels:?}");
        }
        for skipped in [
            "target/.env",
            "node_modules/.env",
            ".git/.env",
            ".venv/.env",
            "dist/.env",
            "build/.env",
        ] {
            assert!(
                !rels.iter().any(|r| r == skipped),
                "build/dependency directory was scanned: {skipped}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prepared_sandbox_mount_targets_are_canonical() {
        let cwd = std::env::temp_dir().join(format!(
            "sandbox_canon_paths_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(cwd.join(".git")).unwrap();

        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, cwd.clone());
        let prepared = plan.prepare().unwrap();
        let expected = cwd.join(".git").canonicalize().unwrap();

        assert!(
            prepared.protected_paths().contains(&expected),
            "protected mount source must match the canonicalized cwd: {:?}",
            prepared.protected_paths()
        );
        let _ = std::fs::remove_dir_all(&cwd);
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
    fn seatbelt_profile_paths_are_unique_per_call() {
        let first = seatbelt_profile_path();
        let second = seatbelt_profile_path();
        assert_ne!(
            first, second,
            "two spawns in one process must not reuse a profile path"
        );
    }

    #[test]
    fn profile_guard_removes_profile_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "mehh-sandbox-guard-{}-{}.sbp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"(version 1)").unwrap();
        {
            let _guard = ProfileGuard(path.clone());
            assert!(path.exists());
        }
        assert!(!path.exists(), "profile file must not leak after use");
    }

    #[test]
    fn cwd_access_follows_policy() {
        assert_eq!(cwd_access(SandboxPolicy::ReadOnly), CwdAccess::ReadOnly);
        assert_eq!(cwd_access(SandboxPolicy::WorkspaceWrite), CwdAccess::Writable);
        assert_eq!(cwd_access(SandboxPolicy::DangerFullAccess), CwdAccess::Writable);
    }

    #[test]
    fn linux_read_only_wrapper_mounts_cwd_read_only() {
        let dir = std::env::temp_dir().join(format!("sandbox-ro-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ro_plan = SandboxPlan::new(SandboxPolicy::ReadOnly, dir.clone());
        let rw_plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        let prepared = ro_plan.prepare().unwrap();
        let cwd = dir.canonicalize().unwrap().to_string_lossy().into_owned();
        let ro_mount = vec!["--ro-bind".to_string(), cwd.clone(), cwd.clone()];
        let rw_mount = vec!["--bind".to_string(), cwd.clone(), cwd.clone()];

        let ro_args = linux_wrapper_args(&ro_plan, &prepared);
        let rw_args = linux_wrapper_args(&rw_plan, &prepared);
        assert!(ro_args.windows(3).any(|w| w == ro_mount.as_slice()), "{ro_args:?}");
        assert!(!ro_args.windows(3).any(|w| w == rw_mount.as_slice()), "{ro_args:?}");
        assert!(rw_args.windows(3).any(|w| w == rw_mount.as_slice()), "{rw_args:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn seatbelt_read_only_policy_drops_cwd_write_rule() {
        let dir = std::env::temp_dir().join(format!("sandbox-ro-seatbelt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ro_plan = SandboxPlan::new(SandboxPolicy::ReadOnly, dir.clone());
        let rw_plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        let ro_prepared = ro_plan.prepare().unwrap();
        let rw_prepared = rw_plan.prepare().unwrap();
        let ro_profile = seatbelt_profile(&ro_plan, &ro_prepared);
        let rw_profile = seatbelt_profile(&rw_plan, &rw_prepared);

        let cwd_s = escape_sbpl_string(&ro_prepared.cwd().to_string_lossy());
        assert!(
            !ro_profile.contains("(allow file-write* (subpath"),
            "ReadOnly Seatbelt must not allow writing cwd or /private/tmp: {ro_profile}"
        );
        assert!(
            ro_profile.contains("(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\"))"),
            "Seatbelt must allow writing /dev/null and /dev/zero: {ro_profile}"
        );
        assert!(
            rw_profile.contains(&format!("(allow file-write* (subpath \"{cwd_s}\") (subpath \"/private/tmp\"))")),
            "{rw_profile}"
        );
        assert!(
            ro_profile.contains("(subpath \"/bin\")"),
            "Seatbelt must allow reading /bin for sh: {ro_profile}"
        );
        assert!(
            ro_profile.contains(&format!("(allow file-read* (subpath \"{cwd_s}\"))")),
            "Seatbelt must allow reading cwd: {ro_profile}"
        );
        let git_s = escape_sbpl_string(&ro_prepared.cwd().join(".git").to_string_lossy());
        assert!(
            !ro_profile.contains(&format!("(deny file-read* (subpath \"{git_s}\"))")),
            "Seatbelt must NOT block reading .git: {ro_profile}"
        );
        assert!(
            ro_profile.contains(&format!("(deny file-write* (subpath \"{git_s}\"))")),
            "Seatbelt must block writing .git: {ro_profile}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn violation_needs_failure_and_marker() {
        assert!(detect_violation("Permission denied", Some(1)));
        assert!(!detect_violation("Permission denied", Some(0)));
        assert!(!detect_violation("all good", Some(1)));
        assert!(
            detect_violation("Bad system call (SIGSYS)", None),
            "signal termination (exit_code = None) must still detect SIGSYS"
        );
    }

    #[test]
    fn secret_env_keys_cover_ssh_agent_and_prefixed_db_vars() {
        for key in [
            "SSH_AUTH_SOCK",
            "GPG_AGENT_INFO",
            "WSL_INTEROP",
            "DOCKER_AUTH_CONFIG",
            "MY_APIKEY",
            "SERVICE_CREDENTIALS",
            "APP_DATABASE_URL",
            "CACHE_REDIS_URL",
            "PG_CONNECTION_STRING",
            "SENTRY_DSN",
        ] {
            assert!(is_secret_env_key(key), "missing secret env key: {key}");
        }
    }

    #[test]
    fn sbpl_escapes_control_characters() {
        assert_eq!(escape_sbpl_string("a\nb\rc\0d"), "a\\nb\\rc\\0d");
    }

    #[test]
    fn linux_wrapper_mounts_proc_and_uses_dev_null_for_regular_files() {
        let dir = std::env::temp_dir().join(format!(
            "sandbox-bwrap-files-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), b"SECRET=1").unwrap();

        let rw_plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        let prepared = rw_plan.prepare().unwrap();
        let args = linux_wrapper_args(&rw_plan, &prepared);

        assert!(
            args.windows(2).any(|w| w == ["--proc", "/proc"]),
            "bwrap must mount fresh /proc: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--tmpfs", "/tmp"]),
            "bwrap WorkspaceWrite must mount private /tmp: {args:?}"
        );
        let env_str = prepared
            .deny_read_paths()
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(".env"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            args.windows(3).any(|w| w == ["--ro-bind", "/dev/null", env_str.as_str()]),
            "existing regular file .env must use --ro-bind /dev/null, not --tmpfs: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w == ["--tmpfs", env_str.as_str()]),
            "existing regular file .env must NOT use --tmpfs: {args:?}"
        );

        let ro_plan = SandboxPlan::new(SandboxPolicy::ReadOnly, dir.clone());
        let ro_args = linux_wrapper_args(&ro_plan, &prepared);
        let missing_git = prepared.cwd().join(".git").to_string_lossy().into_owned();
        assert!(
            !ro_args.windows(2).any(|w| w == ["--tmpfs", missing_git.as_str()]),
            "ReadOnly mount must not pass --tmpfs for non-existent .git: {ro_args:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writable_symlink_pointing_outside_cwd_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "sandbox-symlink-out-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = cwd.join(".git");

        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();

        if !linked {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, cwd);
        assert!(
            plan.prepare().is_err(),
            "symlink .git inside writable cwd pointing outside must be rejected"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wrap_propagates_prepare_failure() {
        // wrap() must run the fail-closed prepare() check even on backends
        // that do not consume the PreparedSandbox (non-Linux), so an invalid
        // cwd still aborts the spawn instead of yielding a degraded invocation.
        let missing = std::env::temp_dir()
            .join(format!("sandbox-missing-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, missing);
        assert!(wrap(&plan, "echo", &["hi".to_string()]).is_err());
    }

    #[test]
    fn plan_protects_mcp_auth_and_trust_json_and_skips_env_scan_for_danger_full_access() {
        let root = std::env::temp_dir().join(format!("sandbox-creds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), b"SECRET=1").unwrap();

        let ws_plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, root.clone());
        if let Some(home) = home_dir() {
            assert!(
                ws_plan
                    .deny_read_paths
                    .contains(&home.join(".mehh").join("agent").join("mcp-auth.json")),
                "mcp-auth.json must be in deny_read_paths"
            );
            assert!(
                ws_plan
                    .protected_paths
                    .contains(&home.join(".mehh").join("agent").join("trust.json")),
                "trust.json must be in protected_paths"
            );
        }
        assert!(
            ws_plan.deny_read_paths.contains(&root.join(".env")),
            "WorkspaceWrite must scan .env files"
        );

        let full_plan = SandboxPlan::new(SandboxPolicy::DangerFullAccess, root.clone());
        assert!(
            !full_plan.deny_read_paths.contains(&root.join(".env")),
            "DangerFullAccess must skip workspace .env tree scan"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_symlink_hopping_through_writable_cwd_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "sandbox-symlink-hop-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("workspace");
        let real_ssh = root.join("real_ssh");
        let fake_home = root.join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&real_ssh).unwrap();
        std::fs::create_dir_all(&fake_home).unwrap();

        let hop = cwd.join("hop");
        let ext_ssh = fake_home.join(".ssh");

        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&real_ssh, &hop).is_ok()
            && std::os::unix::fs::symlink(&hop, &ext_ssh).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&real_ssh, &hop).is_ok()
            && std::os::windows::fs::symlink_dir(&hop, &ext_ssh).is_ok();

        if !linked {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let mut plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, cwd);
        plan.deny_read_paths.push(ext_ssh);
        assert!(
            plan.prepare().is_err(),
            "external symlink hopping through writable cwd must be rejected"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_stale_seatbelt_profiles_removes_foreign_pid_only() {
        let dir = std::env::temp_dir().join(format!("sandbox-prune-sbp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let other_pid = std::process::id().wrapping_add(1).max(1);
        let foreign = dir.join(format!("mehh-sandbox-{other_pid}-0.sbp"));
        let own = dir.join(format!("mehh-sandbox-{}-0.sbp", std::process::id()));
        let unrelated = dir.join("other.txt");
        std::fs::write(&foreign, b"(version 1)").unwrap();
        std::fs::write(&own, b"(version 1)").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        prune_stale_seatbelt_profiles(&dir, std::time::Duration::ZERO);
        assert!(!foreign.exists(), "foreign stale profile must be pruned");
        assert!(own.exists(), "own PID profile must not be pruned");
        assert!(unrelated.exists(), "unrelated file must not be pruned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_cwd_itself_prepares_successfully() {
        let root = std::env::temp_dir().join(format!(
            "sandbox-symlink-cwd-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let real_cwd = root.join("real_workspace");
        let link_cwd = root.join("link_workspace");
        std::fs::create_dir_all(&real_cwd).unwrap();

        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&real_cwd, &link_cwd).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&real_cwd, &link_cwd).is_ok();

        if !linked {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, link_cwd);
        let prepared = plan
            .prepare()
            .expect("prepare() must succeed when cwd itself is a symlink to a real directory");
        assert_eq!(prepared.cwd(), real_cwd.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn linux_wrapper_skips_tmpfs_for_missing_config_files_when_cwd_is_home() {
        let home = std::env::temp_dir().join(format!(
            "sandbox-cwd-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let make_home_plan = || {
            let mut protected_paths: Vec<PathBuf> = PROTECTED_NAMES
                .iter()
                .map(|name| home.join(name))
                .collect();
            protected_paths.push(home.join(".mehh").join("agent").join("permissions.json"));
            protected_paths.push(home.join(".mehh").join("agent").join("trust.json"));
            SandboxPlan {
                policy: SandboxPolicy::WorkspaceWrite,
                cwd: home.clone(),
                protected_paths,
                deny_read_paths: Vec::new(),
                network: false,
            }
        };

        // Case 1: home/.mehh does not exist.
        let plan_no_mehh = make_home_plan();
        let prep_no_mehh = plan_no_mehh.prepare().unwrap();
        let args_no_mehh = linux_wrapper_args(&plan_no_mehh, &prep_no_mehh);
        let mehh_str = prep_no_mehh.cwd().join(".mehh").to_string_lossy().into_owned();
        let perms_str = prep_no_mehh
            .cwd()
            .join(".mehh")
            .join("agent")
            .join("permissions.json")
            .to_string_lossy()
            .into_owned();
        let trust_str = prep_no_mehh
            .cwd()
            .join(".mehh")
            .join("agent")
            .join("trust.json")
            .to_string_lossy()
            .into_owned();

        assert!(
            !args_no_mehh
                .windows(2)
                .any(|w| w == ["--tmpfs", mehh_str.as_str()]),
            "missing .mehh directory must NOT get --tmpfs: {args_no_mehh:?}"
        );
        assert!(
            !args_no_mehh
                .windows(2)
                .any(|w| w == ["--tmpfs", perms_str.as_str()]),
            "missing permissions.json must NOT get --tmpfs: {args_no_mehh:?}"
        );
        assert!(
            !args_no_mehh
                .windows(2)
                .any(|w| w == ["--tmpfs", trust_str.as_str()]),
            "missing trust.json must NOT get --tmpfs: {args_no_mehh:?}"
        );

        // Case 2: home/.mehh exists, while agent/permissions.json and agent/trust.json are missing.
        std::fs::create_dir_all(home.join(".mehh")).unwrap();
        let plan_with_mehh = make_home_plan();
        let prep_with_mehh = plan_with_mehh.prepare().unwrap();
        let args_with_mehh = linux_wrapper_args(&plan_with_mehh, &prep_with_mehh);
        assert!(
            args_with_mehh
                .windows(3)
                .any(|w| w == ["--ro-bind", mehh_str.as_str(), mehh_str.as_str()]),
            "existing .mehh directory should be mounted --ro-bind: {args_with_mehh:?}"
        );
        assert!(
            !args_with_mehh
                .windows(2)
                .any(|w| w == ["--tmpfs", perms_str.as_str()]),
            "missing permissions.json under --ro-bind .mehh must NOT get --tmpfs: {args_with_mehh:?}"
        );
        assert!(
            !args_with_mehh
                .windows(2)
                .any(|w| w == ["--tmpfs", trust_str.as_str()]),
            "missing trust.json under --ro-bind .mehh must NOT get --tmpfs: {args_with_mehh:?}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn seatbelt_profile_includes_metadata_literals_dev_writes_and_self_signal() {
        let dir = std::env::temp_dir().join(format!("sandbox-c6-seatbelt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ro_plan = SandboxPlan::new(SandboxPolicy::ReadOnly, dir.clone());
        let prepared = ro_plan.prepare().unwrap();
        let profile = seatbelt_profile(&ro_plan, &prepared);

        assert!(profile.contains("(allow file-read-metadata)"), "{profile}");
        assert!(
            profile.contains("(allow file-read* (literal \"/tmp\") (literal \"/etc\") (literal \"/var\"))"),
            "{profile}"
        );
        assert!(
            profile.contains("(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\"))"),
            "{profile}"
        );
        assert!(profile.contains("(allow signal (target self))"), "{profile}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_wrapper_never_emits_tmpfs_for_missing_protected_paths_in_writable_cwd() {
        let dir = std::env::temp_dir().join(format!("sandbox-c5-bwrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let plan = SandboxPlan::new(SandboxPolicy::WorkspaceWrite, dir.clone());
        let prepared = plan.prepare().unwrap();
        let args = linux_wrapper_args(&plan, &prepared);

        let existing_git = prepared.cwd().join(".git").to_string_lossy().into_owned();
        let missing_mehh = prepared.cwd().join(".mehh").to_string_lossy().into_owned();
        assert!(
            args.windows(3)
                .any(|w| w == ["--ro-bind", existing_git.as_str(), existing_git.as_str()]),
            "existing .git must still be --ro-bind: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w == ["--tmpfs", missing_mehh.as_str()]),
            "missing .mehh inside writable cwd must NOT get --tmpfs: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
