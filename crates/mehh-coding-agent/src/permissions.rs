//! Persisted tool permission store and granular rule evaluation.

use crate::tool_inspection::{InspectionVerdict, ToolInspectionContext};
use anyhow::{Context, Result};
use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

static PERMISSION_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn permission_lock_mutex(path: &Path) -> Arc<Mutex<()>> {
    let locks = PERMISSION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = mehh_ai::normalize_lock_path(path);
    let mut guard = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn with_permissions_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create permissions directory for '{}'",
                path.display()
            )
        })?;
    }
    let mut lock_os = path.as_os_str().to_os_string();
    lock_os.push(".lock");
    let lock_path = PathBuf::from(lock_os);

    let mutex = permission_lock_mutex(&lock_path);
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
        .with_context(|| format!("Failed to open lock file '{}'", lock_path.display()))?;
    lock_file
        .lock()
        .with_context(|| format!("Failed to acquire lock on '{}'", lock_path.display()))?;

    let result = f();
    let _ = lock_file.unlock();
    result
}

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
    /// Workspace `NeverAllow` rules kept strictly isolated from user/session rules
    /// so workspace config can never weaken global rules and session/user additions
    /// cannot overwrite workspace denies.
    workspace_deny_rules: Vec<PermissionRule>,
    /// In-memory session-only rules kept separate so they cannot overwrite global `NeverAllow`s.
    session_rules: Vec<PermissionRule>,
    /// Exact load/parse failure when `load_fail_closed` fell back to `deny_all`.
    load_error: Option<String>,
    /// Precompiled glob matchers keyed by rule `path_glob`.
    glob_cache: Arc<Mutex<HashMap<String, Arc<globset::GlobMatcher>>>>,
}

/// Returns default user permissions path `~/.mehh/agent/permissions.json`.
pub fn default_permissions_path() -> Result<PathBuf> {
    let home = crate::user_home_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot determine default permissions path: neither USERPROFILE nor HOME is set"
        )
    })?;
    Ok(home.join(".mehh").join("agent").join("permissions.json"))
}

type RuleSpecificity = (bool, bool, bool, usize, u8);

fn is_path_bearing_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "read" | "write" | "edit" | "undo_edit" | "ls" | "find" | "grep" | "analyze"
    )
}

fn is_non_compound_path_prefix(prefix: &str) -> bool {
    let trimmed = prefix.trim();
    let looks_like_path = trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("~\\")
        || trimmed.starts_with("./")
        || trimmed.starts_with(".\\")
        || trimmed.starts_with("../")
        || trimmed.starts_with("..\\")
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && matches!(trimmed.as_bytes()[2], b'/' | b'\\'));
    let has_shell_operator = trimmed.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '\n' | '\r' | '`' | '$' | '<' | '>' | '{' | '}' | '\'' | '"' | '#'
        )
    });
    looks_like_path && !has_shell_operator
}

fn normalize_path_prefix_for_match(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = crate::settings::strip_windows_verbatim_prefix(trimmed);
    let mut norm = stripped.replace('\\', "/");
    while norm.len() > 1 && norm.ends_with('/') {
        if norm.len() == 3 && norm.as_bytes()[1] == b':' {
            break;
        }
        norm.pop();
    }
    norm
}

fn path_matches_prefix(candidate: &str, raw_prefix: &str) -> bool {
    let norm_pfx = normalize_path_prefix_for_match(raw_prefix);
    if norm_pfx.is_empty() {
        return false;
    }
    let norm_cand_raw = crate::settings::strip_windows_verbatim_prefix(candidate).replace('\\', "/");
    let (cand, pfx) = if cfg!(windows) {
        (
            norm_cand_raw.to_ascii_lowercase(),
            norm_pfx.to_ascii_lowercase(),
        )
    } else {
        (norm_cand_raw, norm_pfx)
    };

    if pfx == "/" || (pfx.len() == 3 && pfx.as_bytes()[1] == b':' && pfx.ends_with('/')) {
        cand.starts_with(&pfx)
    } else {
        cand == pfx
            || cand
                .strip_prefix(&pfx)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Path rules are matched against resolved paths, not the raw argument: a
/// `..` segment or a symlink must not let `src/**/*.rs` cover `secret.rs`.
/// Returns `None` when resolution fails so callers fail closed.
fn canonical_rule_paths(ctx: &ToolInspectionContext, raw: &str) -> Option<Vec<String>> {
    let resolved = crate::tool_inspection::resolve_existing_path(&ctx.cwd, raw)?;
    let cwd = crate::tool_inspection::resolve_existing_path(&ctx.cwd, "")?;
    let text = resolved.to_string_lossy();
    let stripped = crate::settings::strip_windows_verbatim_prefix(&text);
    let absolute = stripped.replace('\\', "/");

    let mut candidates = vec![absolute.clone()];
    if let Ok(relative) = resolved.strip_prefix(&cwd) {
        candidates.push(relative.to_string_lossy().replace('\\', "/"));
    }
    // Rules written as `/etc/**` have no drive letter; on Windows expose the
    // drive-root-relative form as well so they still match their intended tree.
    if let Some(idx) = absolute.find(":/") {
        candidates.push(absolute[idx + 1..].to_string());
    }
    Some(candidates)
}

fn is_rfc3986_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn normalize_url_path_for_prefix(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                let byte = ((h << 4) | l) as u8;
                if is_rfc3986_unreserved(byte) || byte == b'/' {
                    decoded.push(byte);
                } else {
                    decoded.push(b'%');
                    decoded.push(bytes[i + 1].to_ascii_uppercase());
                    decoded.push(bytes[i + 2].to_ascii_uppercase());
                }
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let decoded_str = String::from_utf8_lossy(&decoded);
    let had_trailing_slash = decoded_str != "/"
        && (decoded_str.ends_with('/')
            || decoded_str.ends_with("/.")
            || decoded_str.ends_with("/.."));
    let mut segs: Vec<&str> = Vec::new();
    for seg in decoded_str.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            segs.pop();
        } else {
            segs.push(seg);
        }
    }
    if segs.is_empty() {
        "/".to_string()
    } else if had_trailing_slash {
        format!("/{}/", segs.join("/"))
    } else {
        format!("/{}", segs.join("/"))
    }
}

fn canonical_url_prefix_len(raw: &str) -> usize {
    let Ok(u) = mehh_ai::http_safety::normalize_and_validate_http_url(raw) else {
        return raw.len();
    };
    let host = mehh_ai::http_safety::normalized_url_host(&u).unwrap_or_default();
    let path = normalize_url_path_for_prefix(u.path());
    let canonical_path = if path == "/" {
        "/"
    } else {
        path.trim_end_matches('/')
    };
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    let query = u.query().map(|q| format!("?{q}")).unwrap_or_default();
    format!("{}://{}{}{}{}", u.scheme(), host, port, canonical_path, query).len()
}

fn url_matches_prefix(raw_url: &str, raw_prefix: &str) -> bool {
    let Ok(target) = mehh_ai::http_safety::normalize_and_validate_http_url(raw_url) else {
        return false;
    };
    let Ok(prefix) = mehh_ai::http_safety::normalize_and_validate_http_url(raw_prefix) else {
        return false;
    };
    if target.scheme() != prefix.scheme()
        || mehh_ai::http_safety::normalized_url_host(&target)
            != mehh_ai::http_safety::normalized_url_host(&prefix)
        || target.port_or_known_default() != prefix.port_or_known_default()
    {
        return false;
    }

    let p_path = normalize_url_path_for_prefix(prefix.path());
    let t_path = normalize_url_path_for_prefix(target.path());
    if p_path != "/" || prefix.query().is_some() {
        if let Some(pq) = prefix.query() {
            if t_path != p_path {
                return false;
            }
            let tq = target.query().unwrap_or("");
            if !pq.is_empty()
                && tq != pq
                && !tq.strip_prefix(pq).is_some_and(|rest| rest.starts_with('&'))
            {
                return false;
            }
        } else if p_path.ends_with('/') {
            if !t_path.starts_with(&p_path) {
                return false;
            }
        } else if t_path != p_path
            && !t_path
                .strip_prefix(&p_path)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return false;
        }
    }
    true
}

struct RuleEvalPass<'a> {
    best_match: Option<(RuleSpecificity, &'a PermissionRule)>,
    matched_never_allow: bool,
    matched_non_wildcard_never_allow: bool,
    matched_shell_never_allow: bool,
}

fn match_generic_arg_value(
    ctx: &ToolInspectionContext,
    key: Option<&str>,
    val: &serde_json::Value,
    pfx: &str,
) -> (bool, bool) {
    match val {
        serde_json::Value::String(s) => {
            if key.is_some_and(|k| k.eq_ignore_ascii_case("url")) {
                let ok = url_matches_prefix(s, pfx);
                return (ok, ok);
            }
            if key.is_some_and(|k| k.eq_ignore_ascii_case("path")) {
                let ok = canonical_rule_paths(ctx, s.trim()).is_some_and(|candidates| {
                    candidates.iter().any(|c| path_matches_prefix(c, pfx))
                });
                return (ok, false);
            }
            if url_matches_prefix(s, pfx) {
                return (true, true);
            }
            let trimmed_pfx = pfx.trim();
            if !trimmed_pfx.is_empty() && s.trim().starts_with(trimmed_pfx) {
                return (true, false);
            }
            (false, false)
        }
        serde_json::Value::Array(items) => {
            for item in items {
                let (matched, matched_url) = match_generic_arg_value(ctx, None, item, pfx);
                if matched {
                    return (true, matched_url);
                }
            }
            (false, false)
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let (matched, matched_url) = match_generic_arg_value(ctx, Some(k), v, pfx);
                if matched {
                    return (true, matched_url);
                }
            }
            (false, false)
        }
        _ => (false, false),
    }
}

impl PermissionStore {
    pub fn new(rules: Vec<PermissionRule>, user_rules_path: Option<PathBuf>) -> Self {
        Self {
            user_rules_path,
            rules,
            workspace_deny_rules: Vec::new(),
            session_rules: Vec::new(),
            load_error: None,
            glob_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Loads permissions from disk:
    /// 1. Global user rules at `~/.mehh/agent/permissions.json`
    /// 2. Workspace rules at `<cwd>/.mehh/permissions.json` (only if `trusted` is true)
    ///
    /// Fails closed on unresolvable home directory, non-`NotFound` I/O errors,
    /// invalid UTF-8, or malformed JSON.
    pub fn load(cwd: &Path, trusted: bool) -> Result<Self> {
        let user_path = default_permissions_path()?;
        let ws_path = if trusted {
            Some(cwd.join(".mehh").join("permissions.json"))
        } else {
            None
        };
        Self::load_from_paths(&user_path, ws_path.as_deref())
    }

    /// Loads global user rules from `user_path` and optional trusted workspace rules
    /// from `ws_path`, keeping workspace `NeverAllow` rules isolated.
    pub fn load_from_paths(user_path: &Path, ws_path: Option<&Path>) -> Result<Self> {
        let rules = Self::read_rules_from_file(user_path)?.unwrap_or_default();
        let workspace_deny_rules = match ws_path {
            Some(ws) => Self::read_rules_from_file(ws)?
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.action == PermissionAction::NeverAllow)
                .collect(),
            None => Vec::new(),
        };

        Ok(Self {
            user_rules_path: Some(user_path.to_path_buf()),
            rules,
            workspace_deny_rules,
            session_rules: Vec::new(),
            load_error: None,
            glob_cache: Arc::new(Mutex::new(HashMap::new())),
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
        let file: PermissionsFile = serde_json::from_str(clean)
            .with_context(|| format!("Failed to parse permissions JSON in '{}'", path.display()))?;

        // A malformed glob (or malformed URL arg_prefix on web_fetch) would
        // silently turn a NeverAllow rule into a no-op during evaluate().
        // Reject the whole file instead so load_fail_closed can substitute a deny-all store.
        let source = format!("permissions file '{}'", path.display());
        for rule in &file.rules {
            Self::validate_permission_rule(rule, &source)?;
        }

        Ok(Some(file.rules))
    }

    pub(crate) fn validate_permission_rule(rule: &PermissionRule, source: &str) -> Result<()> {
        if rule.tool_name != rule.tool_name.trim() {
            anyhow::bail!(
                "Rule tool_name cannot have leading or trailing whitespace in {}",
                source
            );
        }
        if rule.tool_name.is_empty() {
            anyhow::bail!("Rule tool_name cannot be empty in {}", source);
        }
        if let Some(ref prefix) = rule.arg_prefix {
            if prefix.trim().is_empty() {
                anyhow::bail!("Rule arg_prefix cannot be empty or whitespace-only in {}", source);
            }
        }
        if let Some(ref pattern) = rule.path_glob {
            if pattern.trim().is_empty() {
                anyhow::bail!("Rule path_glob cannot be empty or whitespace-only in {}", source);
            }
            GlobBuilder::new(pattern)
                .case_insensitive(cfg!(windows))
                .build()
                .with_context(|| format!("Invalid path_glob '{}' in {}", pattern, source))?;
        }
        if rule.tool_name.eq_ignore_ascii_case("web_fetch") {
            if let Some(ref prefix) = rule.arg_prefix {
                mehh_ai::http_safety::normalize_and_validate_http_url(prefix).map_err(|err| {
                    anyhow::anyhow!("Invalid URL arg_prefix '{}' in {}: {err}", prefix, source)
                })?;
            }
        }
        if mehh_execpolicy::is_shell_tool(&rule.tool_name) {
            if let Some(ref prefix) = rule.arg_prefix {
                let eval = mehh_execpolicy::check_prefix(prefix, prefix);
                if !eval.safe_parse || !eval.all_covered {
                    anyhow::bail!(
                        "Invalid or compound shell arg_prefix '{}' in {}",
                        prefix,
                        source
                    );
                }
            }
        } else if rule.tool_name == "*" {
            if let Some(ref prefix) = rule.arg_prefix {
                let valid_url =
                    mehh_ai::http_safety::normalize_and_validate_http_url(prefix).is_ok();
                let eval = mehh_execpolicy::check_prefix(prefix, prefix);
                let valid_shell = eval.safe_parse && eval.all_covered;
                if !valid_url && !valid_shell && !is_non_compound_path_prefix(prefix) {
                    anyhow::bail!(
                        "Invalid or compound arg_prefix '{}' for wildcard rule in {}",
                        prefix,
                        source
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn user_rules_path(&self) -> Option<&Path> {
        self.user_rules_path.as_deref()
    }

    pub(crate) fn persist_rule_to_file(path: &Path, rule: &PermissionRule) -> Result<()> {
        with_permissions_file_lock(path, || {
            let mut file_rules = Self::read_rules_from_file(path)?.unwrap_or_default();
            file_rules.retain(|r| {
                !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                    && r.arg_prefix == rule.arg_prefix
                    && r.path_glob == rule.path_glob)
            });
            file_rules.push(rule.clone());
            let file = PermissionsFile { rules: file_rules };
            let serialized = serde_json::to_string_pretty(&file)?;
            crate::settings::write_file_atomic(path, serialized.as_bytes(), true)
        })
    }

    pub(crate) fn insert_in_memory_rule(&mut self, rule: PermissionRule) {
        self.rules.retain(|r| {
            !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                && r.arg_prefix == rule.arg_prefix
                && r.path_glob == rule.path_glob)
        });
        self.rules.push(rule);
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    pub fn workspace_deny_rules(&self) -> &[PermissionRule] {
        &self.workspace_deny_rules
    }

    pub fn session_rules(&self) -> &[PermissionRule] {
        &self.session_rules
    }

    pub(crate) fn restore_session_rules(&mut self, rules: Vec<PermissionRule>) {
        self.session_rules = rules;
    }

    /// Appends a new rule and persists it to the user's global `permissions.json`
    /// under an advisory file lock.
    pub fn add_rule(&mut self, rule: PermissionRule) -> Result<()> {
        Self::validate_permission_rule(&rule, "PermissionStore::add_rule")?;
        if let Some(ref path) = self.user_rules_path {
            Self::persist_rule_to_file(path, &rule)?;
        }
        self.insert_in_memory_rule(rule);
        Ok(())
    }

    /// Appends a validated rule for the current session only (no disk persistence).
    /// Kept isolated from global and workspace `NeverAllow` rules.
    pub fn add_session_rule(&mut self, rule: PermissionRule) -> Result<()> {
        Self::validate_permission_rule(&rule, "PermissionStore::add_session_rule")?;
        self.session_rules.retain(|r| {
            !(r.tool_name.eq_ignore_ascii_case(&rule.tool_name)
                && r.arg_prefix == rule.arg_prefix
                && r.path_glob == rule.path_glob)
        });
        self.session_rules.push(rule);
        Ok(())
    }

    fn compiled_glob(&self, pattern: &str) -> Option<Arc<globset::GlobMatcher>> {
        {
            let cache = self
                .glob_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(matcher) = cache.get(pattern) {
                return Some(Arc::clone(matcher));
            }
        }
        let matcher = Arc::new(
            GlobBuilder::new(pattern)
                .case_insensitive(cfg!(windows))
                .build()
                .ok()?
                .compile_matcher(),
        );
        let mut cache = self
            .glob_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(pattern.to_string(), Arc::clone(&matcher));
        Some(matcher)
    }

    fn resolve_canon_paths_cached<'c>(
        cache: &'c mut Option<Option<Vec<String>>>,
        ctx: &ToolInspectionContext,
    ) -> Option<&'c [String]> {
        cache
            .get_or_insert_with(|| {
                crate::tool_inspection::effective_path_arg(ctx)
                    .and_then(|path| canonical_rule_paths(ctx, path.trim()))
            })
            .as_deref()
    }

    fn evaluate_rule_slice<'a>(
        &self,
        rules: &'a [PermissionRule],
        ctx: &ToolInspectionContext,
        is_shell: bool,
        parsed_shell: Option<&mehh_execpolicy::ParsedCommand>,
        canon_paths_cache: &mut Option<Option<Vec<String>>>,
    ) -> RuleEvalPass<'a> {
        let mut best_match: Option<(RuleSpecificity, &'a PermissionRule)> = None;
        let mut matched_never_allow = false;
        let mut matched_non_wildcard_never_allow = false;
        let mut matched_shell_never_allow = false;
        let is_path_tool = is_path_bearing_tool(&ctx.tool_name);

        for rule in rules {
            if !rule.tool_name.eq_ignore_ascii_case(&ctx.tool_name) && rule.tool_name != "*" {
                continue;
            }

            let (matches_prefix, matched_url) = if let Some(ref pfx) = rule.arg_prefix {
                if pfx.trim().is_empty() {
                    (false, false)
                } else if ctx.tool_name.eq_ignore_ascii_case("web_fetch") {
                    let ok = ctx
                        .args
                        .get("url")
                        .and_then(|v| v.as_str())
                        .is_some_and(|url| url_matches_prefix(url, pfx));
                    (ok, ok)
                } else if ctx.tool_name.eq_ignore_ascii_case("web_search") {
                    let trimmed_pfx = pfx.trim();
                    let ok = ctx
                        .args
                        .get("query")
                        .and_then(|v| v.as_str())
                        .is_some_and(|q| q.trim().starts_with(trimmed_pfx));
                    (ok, false)
                } else if is_shell {
                    let ok = parsed_shell.is_some_and(|cmd| cmd.check_prefix(pfx).matched);
                    (ok, false)
                } else if is_path_tool {
                    let ok = Self::resolve_canon_paths_cached(canon_paths_cache, ctx).is_some_and(
                        |candidates| {
                            candidates
                                .iter()
                                .any(|candidate| path_matches_prefix(candidate, pfx))
                        },
                    );
                    (ok, false)
                } else {
                    let mut found = (false, false);
                    for (k, v) in &ctx.args {
                        let m = match_generic_arg_value(ctx, Some(k), v, pfx);
                        if m.0 {
                            found = m;
                            break;
                        }
                    }
                    found
                }
            } else {
                (true, false)
            };
            if !matches_prefix {
                continue;
            }

            if let Some(ref glob_pattern) = rule.path_glob {
                let matches_glob = if is_path_tool {
                    if let Some(matcher) = self.compiled_glob(glob_pattern) {
                        Self::resolve_canon_paths_cached(canon_paths_cache, ctx)
                            .is_some_and(|paths| paths.iter().any(|p| matcher.is_match(p)))
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

            if rule.action == PermissionAction::NeverAllow {
                matched_never_allow = true;
                let is_catch_all_wildcard = rule.tool_name == "*"
                    && rule.arg_prefix.is_none()
                    && rule.path_glob.is_none();
                if !is_catch_all_wildcard {
                    matched_non_wildcard_never_allow = true;
                }
                if is_shell && parsed_shell.is_some() {
                    matched_shell_never_allow = true;
                }
            }

            let is_filtered = rule.arg_prefix.is_some() || rule.path_glob.is_some();
            let both_filtered = rule.arg_prefix.is_some() && rule.path_glob.is_some();
            let exact_tool = !rule.tool_name.eq_ignore_ascii_case("*");
            let prefix_len = rule.arg_prefix.as_ref().map_or(0, |s| {
                if matched_url {
                    canonical_url_prefix_len(s)
                } else {
                    s.len()
                }
            });
            // Strictest action wins at equal specificity.
            let action_rank = match rule.action {
                PermissionAction::NeverAllow => 2,
                PermissionAction::AskBefore => 1,
                PermissionAction::AlwaysAllow => 0,
            };
            let specificity = (
                is_filtered,
                both_filtered,
                exact_tool,
                prefix_len,
                action_rank,
            );

            if let Some((best_spec, _)) = best_match {
                if specificity > best_spec {
                    best_match = Some((specificity, rule));
                }
            } else {
                best_match = Some((specificity, rule));
            }
        }

        RuleEvalPass {
            best_match,
            matched_never_allow,
            matched_non_wildcard_never_allow,
            matched_shell_never_allow,
        }
    }

    /// Evaluates active rules against the inspection context.
    ///
    /// Precedence:
    /// 1. Workspace `NeverAllow` rules (`workspace_deny_rules`) cannot be overridden.
    /// 2. Global `NeverAllow` rules override all session rules and all allow rules.
    /// 3. Specific rules (those with arg_prefix or path_glob, longer prefix, or exact tool names)
    ///    take precedence over broad rules.
    pub fn evaluate(&self, ctx: &ToolInspectionContext) -> Option<InspectionVerdict> {
        if let Some(ref err) = self.load_error {
            return Some(InspectionVerdict::Deny {
                reason: format!(
                    "Permissions store failed to load: {err} (denying tool '{}')",
                    ctx.tool_name
                ),
            });
        }

        let is_shell = mehh_execpolicy::is_shell_tool(&ctx.tool_name);
        let shell_cmd = if is_shell {
            ctx.args.get("command").and_then(|v| v.as_str())
        } else {
            None
        };
        let parsed_shell = shell_cmd.map(mehh_execpolicy::ParsedCommand::parse);
        let shell_safe_parse = parsed_shell
            .as_ref()
            .is_none_or(mehh_execpolicy::ParsedCommand::is_safe_parse);

        let mut canon_paths_cache: Option<Option<Vec<String>>> = None;
        let ws_pass = self.evaluate_rule_slice(
            &self.workspace_deny_rules,
            ctx,
            is_shell,
            parsed_shell.as_ref(),
            &mut canon_paths_cache,
        );
        if ws_pass.matched_never_allow {
            return Some(InspectionVerdict::Deny {
                reason: format!("Workspace permission rule forbids tool '{}'", ctx.tool_name),
            });
        }

        let global_pass = self.evaluate_rule_slice(
            &self.rules,
            ctx,
            is_shell,
            parsed_shell.as_ref(),
            &mut canon_paths_cache,
        );
        let session_pass = self.evaluate_rule_slice(
            &self.session_rules,
            ctx,
            is_shell,
            parsed_shell.as_ref(),
            &mut canon_paths_cache,
        );
        let matched_shell_never_allow =
            global_pass.matched_shell_never_allow || session_pass.matched_shell_never_allow;

        if !shell_safe_parse {
            if matched_shell_never_allow {
                return Some(InspectionVerdict::Deny {
                    reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
                });
            }
            return Some(InspectionVerdict::RequireApproval {
                reason: format!(
                    "Shell command for '{}' could not be safely parsed and requires approval",
                    ctx.tool_name
                ),
                preview: shell_cmd.map(str::to_string),
            });
        }

        if session_pass.matched_never_allow {
            return Some(InspectionVerdict::Deny {
                reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
            });
        }

        let rule: &PermissionRule = if global_pass.matched_never_allow {
            match global_pass.best_match {
                Some((_, r))
                    if r.action == PermissionAction::AskBefore
                        && !global_pass.matched_non_wildcard_never_allow =>
                {
                    r
                }
                _ => {
                    return Some(InspectionVerdict::Deny {
                        reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
                    });
                }
            }
        } else {
            match (global_pass.best_match, session_pass.best_match) {
                (Some((gs, gr)), Some((ss, sr))) => {
                    if ss > gs {
                        sr
                    } else {
                        gr
                    }
                }
                (Some((_, gr)), None) => gr,
                (None, Some((_, sr))) => sr,
                (None, None) => return None,
            }
        };

        // Shell rules are decided by the whole parsed command line, not by a
        // raw string prefix. Every sub-command must be covered before an
        // allow rule applies; anything unparseable fails closed to a prompt.
        // AskBefore is not an allow/deny decision: its `false` would become a
        // hard deny in `verdict_for`, so let the action match below handle it.
        if rule.action != PermissionAction::AskBefore && is_shell {
            if let (Some(pfx), Some(cmd), Some(parsed)) =
                (rule.arg_prefix.as_deref(), shell_cmd, parsed_shell.as_ref())
            {
                let eval = parsed.check_prefix(pfx);
                let prompt = || InspectionVerdict::RequireApproval {
                    reason: format!(
                        "Permission rule for '{}' cannot safely cover this command",
                        ctx.tool_name
                    ),
                    preview: Some(cmd.to_string()),
                };
                if !eval.safe_parse {
                    // A hard deny must stay a hard deny even when the command
                    // cannot be parsed; only allow rules degrade to a prompt.
                    return Some(if rule.action == PermissionAction::NeverAllow {
                        InspectionVerdict::Deny {
                            reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
                        }
                    } else {
                        prompt()
                    });
                }
                let verdict =
                    mehh_execpolicy::verdict_for(eval, rule.action == PermissionAction::AlwaysAllow);
                return Some(match verdict {
                    mehh_execpolicy::Verdict::Allow => InspectionVerdict::Allow,
                    mehh_execpolicy::Verdict::Prompt => {
                        if matched_shell_never_allow {
                            InspectionVerdict::Deny {
                                reason: format!(
                                    "Permission rule forbids tool '{}': '{cmd}'",
                                    ctx.tool_name
                                ),
                            }
                        } else {
                            prompt()
                        }
                    }
                    mehh_execpolicy::Verdict::Deny => InspectionVerdict::Deny {
                        reason: format!("Permission rule forbids tool '{}': '{cmd}'", ctx.tool_name),
                    },
                });
            }
        }

        if is_shell && matched_shell_never_allow && rule.action == PermissionAction::AskBefore {
            if let (Some(cmd), Some(parsed)) = (shell_cmd, parsed_shell.as_ref()) {
                let uncovered = match rule.arg_prefix.as_deref() {
                    Some(pfx) => !parsed.check_prefix(pfx).all_covered,
                    None => rule.path_glob.is_some(),
                };
                if uncovered {
                    return Some(InspectionVerdict::Deny {
                        reason: format!("Permission rule forbids tool '{}': '{cmd}'", ctx.tool_name),
                    });
                }
            }
        }

        match rule.action {
            PermissionAction::AlwaysAllow => Some(InspectionVerdict::Allow),
            PermissionAction::AskBefore => {
                let preview_val = if ctx.tool_name.eq_ignore_ascii_case("web_fetch") {
                    ctx.args.get("url")
                } else if is_shell {
                    ctx.args.get("command")
                } else if ctx.tool_name.eq_ignore_ascii_case("web_search") {
                    ctx.args.get("query")
                } else {
                    ctx.args
                        .get("command")
                        .or_else(|| ctx.args.get("path"))
                        .or_else(|| ctx.args.get("url"))
                        .or_else(|| ctx.args.get("query"))
                };
                Some(InspectionVerdict::RequireApproval {
                    reason: format!(
                        "Permission rule requires approval for tool '{}'",
                        ctx.tool_name
                    ),
                    preview: preview_val.and_then(|v| v.as_str()).map(str::to_string),
                })
            }
            PermissionAction::NeverAllow => Some(InspectionVerdict::Deny {
                reason: format!("Permission rule forbids tool '{}'", ctx.tool_name),
            }),
        }
    }

    /// Fail-closed loader: a corrupt/unreadable permissions file must never
    /// become an empty, permissive store. Deny everything until it is repaired.
    pub fn load_fail_closed(cwd: &Path, trusted: bool) -> Self {
        let user_rules_path = default_permissions_path().ok();
        match Self::load(cwd, trusted) {
            Ok(store) => store,
            Err(err) => Self::deny_all(user_rules_path, format!("{err:#}")),
        }
    }

    fn deny_all(user_rules_path: Option<PathBuf>, load_error: String) -> Self {
        Self {
            user_rules_path,
            rules: vec![PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::NeverAllow,
            }],
            workspace_deny_rules: Vec::new(),
            session_rules: Vec::new(),
            load_error: Some(load_error),
            glob_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(tool: &str, command: Option<&str>, path: Option<&str>) -> ToolInspectionContext {
        let mut args = serde_json::Map::new();
        if let Some(cmd) = command {
            args.insert(
                "command".to_string(),
                serde_json::Value::String(cmd.to_string()),
            );
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
            origin: None,
        }
    }

    #[test]
    fn permission_store_ask_before_shell_rule_returns_require_approval_not_deny() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git status".to_string()),
                path_glob: None,
                action: PermissionAction::AskBefore,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("git status --short"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::RequireApproval { .. }) => {}
            other => panic!("AskBefore with arg_prefix must require approval, got {other:?}"),
        }

        let chained = make_ctx("bash", Some("git status && rm -rf /"), None);
        match store.evaluate(&chained) {
            Some(InspectionVerdict::RequireApproval { .. }) => {}
            other => panic!("AskBefore must not deny a chained command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn permission_store_workspace_cannot_override_global_never_allow_and_corrupt_file_fails_closed() {
        let _env_lock = crate::ENV_MUTEX.lock().await;
        struct HomeGuard {
            prev: Option<std::ffi::OsString>,
        }
        impl HomeGuard {
            fn set(home: &std::path::Path) -> Self {
                let prev = std::env::var_os("USERPROFILE");
                std::env::set_var("USERPROFILE", home);
                Self { prev }
            }
        }
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
        }

        let dir = crate::TempTestDir::new("perm_escalation");
        let home = dir.path().join("home");
        let agent_dir = home.join(".mehh").join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let global = agent_dir.join("permissions.json");
        std::fs::write(
            &global,
            r#"{"rules":[{"tool_name":"bash","action":"never_allow"}]}"#,
        )
        .unwrap();

        let cwd = dir.path().join("workspace");
        std::fs::create_dir_all(cwd.join(".mehh")).unwrap();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            cwd.join(".mehh").join("permissions.json"),
            r#"{"rules":[
                {"tool_name":"*","action":"always_allow"},
                {"tool_name":"bash","action":"always_allow"}
            ]}"#,
        )
        .unwrap();

        let _home = HomeGuard::set(&home);

        let store = PermissionStore::load(&cwd, true).unwrap();
        assert!(
            store.rules().iter().all(|r| r.action != PermissionAction::AlwaysAllow),
            "workspace AlwaysAllow rules must be rejected: {:?}",
            store.rules()
        );
        let bash_ctx = make_ctx("bash", Some("cargo check"), None);
        assert!(
            matches!(store.evaluate(&bash_ctx), Some(InspectionVerdict::Deny { .. })),
            "global NeverAllow must survive workspace escalation"
        );

        let tie = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".into(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "bash".into(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        assert!(
            matches!(tie.evaluate(&bash_ctx), Some(InspectionVerdict::Deny { .. })),
            "NeverAllow must beat AlwaysAllow at equal specificity"
        );

        std::fs::write(&global, "{not json").unwrap();
        let settings = crate::Settings {
            permission_mode: Some(crate::tool_inspection::PermissionMode::Auto),
            ..crate::Settings::default()
        };
        let session = crate::AgentSession::new_with_trust(
            cwd.clone(),
            mehh_ai::faux_model(),
            std::sync::Arc::new(mehh_ai::FauxProvider::new()),
            crate::SessionManager::in_memory(&cwd),
            settings,
            true,
        );
        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::json!("src/main.rs"));
        let edit_ctx = crate::tool_inspection::ToolInspectionContext {
            tool_call_id: "call_perm_fail_closed".into(),
            tool_name: "edit".into(),
            args,
            is_read_only: Some(false),
            cwd: cwd.clone(),
            origin: None,
        };
        assert!(
            matches!(
                session.inspection_manager.inspect(&edit_ctx).await,
                InspectionVerdict::Deny { .. }
            ),
            "corrupt permissions file must fail closed, not auto-allow"
        );
    }

    #[tokio::test]
    async fn missing_home_fails_closed_for_global_permissions() {
        let _env_lock = crate::ENV_MUTEX.lock().await;
        struct EnvGuard {
            userprofile: Option<std::ffi::OsString>,
            home: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn clear() -> Self {
                let guard = Self {
                    userprofile: std::env::var_os("USERPROFILE"),
                    home: std::env::var_os("HOME"),
                };
                std::env::remove_var("USERPROFILE");
                std::env::remove_var("HOME");
                guard
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.userprofile {
                    Some(v) => std::env::set_var("USERPROFILE", v),
                    None => std::env::remove_var("USERPROFILE"),
                }
                match &self.home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }

        let _guard = EnvGuard::clear();
        let store = PermissionStore::load_fail_closed(Path::new("."), false);
        let ctx = make_ctx("read", None, Some("notes.txt"));
        assert!(
            matches!(store.evaluate(&ctx), Some(InspectionVerdict::Deny { .. })),
            "unresolvable global rules path must fail closed, got {:?}",
            store.rules()
        );
    }

    #[test]
    fn session_rule_stays_in_memory() {
        let path = std::env::temp_dir().join(format!("session_rule_{}", uuid::Uuid::now_v7()));
        let mut store = PermissionStore::new(Vec::new(), Some(path.clone()));
        store
            .add_session_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .unwrap();
        assert_eq!(store.session_rules().len(), 1);
        assert!(store.rules().is_empty());
        assert!(!path.exists(), "session-only rule must not touch disk");
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
    fn shell_chain_cannot_inherit_an_allow_rule() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git status".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("git status && rm -rf /"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::RequireApproval { .. }) => {}
            other => panic!("chained command must require approval, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_command_keeps_hard_deny() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("rm -rf".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("rm -rf $(echo /)"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::Deny { .. }) => {}
            other => panic!("hard deny must survive unparseable input, got {other:?}"),
        }
    }

    #[test]
    fn absolute_path_cannot_bypass_a_basename_deny_rule() {
        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git push".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            }],
            None,
        );
        let ctx = make_ctx("bash", Some("/usr/bin/git push --force origin main"), None);
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::Deny { .. }) => {}
            other => panic!("absolute path must still be denied, got {other:?}"),
        }
    }

    #[test]
    fn path_glob_rule_cannot_match_parent_dir_traversal() {
        let dir = crate::TempTestDir::new("perm_path_traversal");
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("src").join("sub")).unwrap();
        std::fs::write(cwd.join("src").join("sub").join("main.rs"), "").unwrap();
        std::fs::write(cwd.join("secret.rs"), "").unwrap();

        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "edit".to_string(),
                arg_prefix: None,
                path_glob: Some("src/**/*.rs".to_string()),
                action: PermissionAction::AlwaysAllow,
            }],
            None,
        );

        let mut traversal = make_ctx("edit", None, Some("src/../secret.rs"));
        traversal.cwd = cwd.to_path_buf();
        assert_ne!(
            store.evaluate(&traversal),
            Some(InspectionVerdict::Allow),
            "traversal must not match a rule scoped to src/"
        );

        let mut legit = make_ctx("edit", None, Some("src/sub/main.rs"));
        legit.cwd = cwd.to_path_buf();
        assert_eq!(
            store.evaluate(&legit),
            Some(InspectionVerdict::Allow),
            "a real path under src/ must still match"
        );
    }

    #[cfg(windows)]
    #[test]
    fn path_glob_rule_is_case_insensitive_on_windows() {
        let dir = crate::TempTestDir::new("perm_path_case");
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("SRC")).unwrap();
        std::fs::write(cwd.join("SRC").join("Main.RS"), "").unwrap();

        let store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "edit".to_string(),
                arg_prefix: None,
                path_glob: Some("src/**/*.rs".to_string()),
                action: PermissionAction::AlwaysAllow,
            }],
            None,
        );

        let mut ctx = make_ctx("edit", None, Some("SRC/Main.RS"));
        ctx.cwd = cwd.to_path_buf();
        assert_eq!(
            store.evaluate(&ctx),
            Some(InspectionVerdict::Allow),
            "Windows path rules must ignore case"
        );
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

    #[test]
    fn never_allow_beats_longer_always_allow_on_unparseable_and_chained_shell_commands() {
        let store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("git status".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("rm".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );

        let safe_git = make_ctx("bash", Some("git status --short"), None);
        assert_eq!(store.evaluate(&safe_git), Some(InspectionVerdict::Allow));

        let unparseable_rm = make_ctx("bash", Some("rm -rf / $(id)"), None);
        assert!(
            matches!(store.evaluate(&unparseable_rm), Some(InspectionVerdict::Deny { .. })),
            "unparseable rm must remain Deny even when longer AlwaysAllow exists"
        );

        let chained_rm = make_ctx("bash", Some("git status && rm -rf /"), None);
        assert!(
            matches!(store.evaluate(&chained_rm), Some(InspectionVerdict::Deny { .. })),
            "chained NeverAllow command must be Deny, not Prompt"
        );

        let blanket_deny = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("git status".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        let unparseable_git = make_ctx("bash", Some("git status $(id)"), None);
        assert!(
            matches!(blanket_deny.evaluate(&unparseable_git), Some(InspectionVerdict::Deny { .. })),
            "blanket NeverAllow must deny unparseable input despite filtered AlwaysAllow"
        );
        #[cfg(windows)]
        {
            let win_quote = make_ctx("bash", Some(r#"git status "x\""#), None);
            assert!(
                matches!(blanket_deny.evaluate(&win_quote), Some(InspectionVerdict::Deny { .. })),
                "blanket NeverAllow must deny ambiguous Windows quote+backslash input"
            );
        }

        let ask_vs_never = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("git status".to_string()),
                    path_glob: None,
                    action: PermissionAction::AskBefore,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("rm".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        let chained_ask = make_ctx("bash", Some("git status && rm -rf /"), None);
        assert!(
            matches!(ask_vs_never.evaluate(&chained_ask), Some(InspectionVerdict::Deny { .. })),
            "AskBefore + NeverAllow on chained command must Deny, not prompt"
        );
        let safe_ask = make_ctx("bash", Some("git status --short"), None);
        assert!(
            matches!(
                ask_vs_never.evaluate(&safe_ask),
                Some(InspectionVerdict::RequireApproval { .. })
            ),
            "safe AskBefore command must still require approval"
        );

        let wildcard_deny_with_bash_ask = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "*".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::AskBefore,
                },
            ],
            None,
        );
        assert!(
            matches!(
                wildcard_deny_with_bash_ask.evaluate(&make_ctx("bash", Some("echo hi"), None)),
                Some(InspectionVerdict::RequireApproval { .. })
            ),
            "unfiltered bash AskBefore override must still beat wildcard NeverAllow"
        );
    }

    #[test]
    fn test_url_arg_prefix_boundary_matching() {
        let make_url_ctx = |url: &str| {
            let mut args = serde_json::Map::new();
            args.insert("url".to_string(), serde_json::Value::String(url.to_string()));
            ToolInspectionContext {
                tool_call_id: "u1".to_string(),
                tool_name: "web_fetch".to_string(),
                args,
                is_read_only: Some(true),
                cwd: PathBuf::from("."),
                origin: None,
            }
        };

        let store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs/reqwest".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );

        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs")),
            Some(InspectionVerdict::Allow)
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/tokio")),
            Some(InspectionVerdict::Allow)
        ));
        // docs.rs/reqwest-evil must match docs.rs (Allow), NOT docs.rs/reqwest (NeverAllow)
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/reqwest-evil")),
            Some(InspectionVerdict::Allow)
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/reqwest")),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("HTTPS://DOCS.RS:443/reqwest/latest#sec")),
            Some(InspectionVerdict::Deny { .. })
        ));
        // Trailing-dot FQDN, repeated slashes, and percent-encoded path forms must NOT bypass NeverAllow.
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs./reqwest")),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs//reqwest/latest")),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/%72eqwest")),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/reqwest%2fsecret")),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_url_ctx("https://docs.rs/public/%2e%2e/reqwest")),
            Some(InspectionVerdict::Deny { .. })
        ));

        // Redundant default port :443 on a broader AlwaysAllow rule must not outrank narrower NeverAllow
        let port_store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs:443/".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs/reqwest".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        assert!(matches!(
            port_store.evaluate(&make_url_ctx("https://docs.rs/reqwest")),
            Some(InspectionVerdict::Deny { .. })
        ));

        // Domain boundary bypasses must NOT match
        assert!(store.evaluate(&make_url_ctx("https://docs.rs.evil.com")).is_none());
        assert!(store.evaluate(&make_url_ctx("https://docs.rs:8443/x")).is_none());
        assert!(store.evaluate(&make_url_ctx("http://docs.rs")).is_none());

        // Malformed web_fetch arg_prefix on disk fails closed
        let dir = crate::TempTestDir::new("perm_bad_url_rule");
        let path = dir.path().join("permissions.json");
        std::fs::write(
            &path,
            r#"{"rules":[{"tool_name":"web_fetch","arg_prefix":"not-a-valid-url","action":"never_allow"}]}"#,
        )
        .unwrap();
        assert!(PermissionStore::read_rules_from_file(&path).is_err());

        // add_rule must also reject malformed web_fetch arg_prefix and path_glob without touching disk
        let clean_path = dir.path().join("clean_permissions.json");
        let mut writable = PermissionStore::new(Vec::new(), Some(clean_path.clone()));
        assert!(writable
            .add_rule(PermissionRule {
                tool_name: "web_fetch".to_string(),
                arg_prefix: Some("not-a-valid-url".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .is_err());
        assert!(!clean_path.exists());
    }

    #[test]
    fn test_web_fetch_url_normalization_specificity_and_extra_arg_bypass_resistance() {
        let make_ctx = |url: &str, extra_cmd: Option<&str>| {
            let mut args = serde_json::Map::new();
            args.insert("url".to_string(), serde_json::Value::String(url.to_string()));
            if let Some(cmd) = extra_cmd {
                args.insert(
                    "command".to_string(),
                    serde_json::Value::String(cmd.to_string()),
                );
            }
            ToolInspectionContext {
                tool_call_id: "u-sec".to_string(),
                tool_name: "web_fetch".to_string(),
                args,
                is_read_only: Some(true),
                cwd: PathBuf::from("."),
                origin: None,
            }
        };

        // 1. AlwaysAllow with trailing slash must NOT outrank NeverAllow without trailing slash.
        let slash_store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs/api/".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs/api".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        assert!(matches!(
            slash_store.evaluate(&make_ctx("https://docs.rs/api/secret", None)),
            Some(InspectionVerdict::Deny { .. })
        ));

        // 2. %2f. and %2f.. preserve directory trailing-slash semantics; %c3%a9 matches %C3%A9.
        let path_store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://docs.rs/reqwest/".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: Some("https://example.com/caf%C3%A9".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        for blocked in [
            "https://docs.rs/reqwest%2f.",
            "https://docs.rs/reqwest/sub%2f..",
            "https://example.com/caf%c3%a9",
        ] {
            assert!(
                matches!(
                    path_store.evaluate(&make_ctx(blocked, None)),
                    Some(InspectionVerdict::Deny { .. })
                ),
                "expected deny for {blocked}"
            );
        }

        // 3. Extra "command" or "path" JSON args on web_fetch must NOT bypass URL rules or spoof preview.
        let ask_store = PermissionStore::new(
            vec![PermissionRule {
                tool_name: "web_fetch".to_string(),
                arg_prefix: Some("https://corp.internal/".to_string()),
                path_glob: None,
                action: PermissionAction::AskBefore,
            }],
            None,
        );
        let verdict = ask_store.evaluate(&make_ctx("https://corp.internal/secret", Some("harmless")));
        match verdict {
            Some(InspectionVerdict::RequireApproval { preview, .. }) => {
                assert_eq!(preview.as_deref(), Some("https://corp.internal/secret"));
            }
            other => panic!("expected RequireApproval with URL preview, got: {other:?}"),
        }
    }

    #[test]
    fn never_allow_beats_session_rules_and_longer_always_allow() {
        let mut store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("git status".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
            ],
            None,
        );
        store
            .add_session_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git status".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .unwrap();
        store
            .add_session_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("echo hi".to_string()),
                path_glob: None,
                action: PermissionAction::AskBefore,
            })
            .unwrap();

        assert!(matches!(
            store.evaluate(&make_ctx("bash", Some("git status --short"), None)),
            Some(InspectionVerdict::Deny { .. })
        ));
        assert!(matches!(
            store.evaluate(&make_ctx("bash", Some("echo hi"), None)),
            Some(InspectionVerdict::Deny { .. })
        ));
    }

    #[test]
    fn workspace_deny_isolation_and_bom_fail_closed_load() {
        let dir = crate::TempTestDir::new("perm_ws_isolation");
        let global_path = dir.path().join("global.json");
        let ws_path = dir.path().join("ws.json");

        std::fs::write(
            &global_path,
            "\u{feff}{\"rules\":[{\"tool_name\":\"read\",\"action\":\"always_allow\"}]}",
        )
        .unwrap();
        std::fs::write(
            &ws_path,
            "\u{feff}{\"rules\":[{\"tool_name\":\"bash\",\"action\":\"never_allow\"},{\"tool_name\":\"edit\",\"action\":\"always_allow\"}]}",
        )
        .unwrap();

        let mut store = PermissionStore::load_from_paths(&global_path, Some(&ws_path)).unwrap();
        assert_eq!(store.workspace_deny_rules().len(), 1);
        assert_eq!(store.rules().len(), 1);

        // User or session rule cannot overwrite isolated workspace NeverAllow.
        store
            .add_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .unwrap();
        store
            .add_session_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .unwrap();
        assert!(matches!(
            store.evaluate(&make_ctx("bash", Some("git status"), None)),
            Some(InspectionVerdict::Deny { .. })
        ));

        // Invalid UTF-8 must fail closed.
        std::fs::write(&global_path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(PermissionStore::load_from_paths(&global_path, None).is_err());
    }

    #[test]
    fn path_glob_only_matches_path_bearing_tools_and_arg_prefix_dispatch() {
        let dir = crate::TempTestDir::new("perm_dispatch");
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(cwd.join("src_evil")).unwrap();
        std::fs::write(cwd.join("src").join("main.rs"), "").unwrap();
        std::fs::write(cwd.join("src_evil").join("secret.rs"), "").unwrap();

        let store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "*".to_string(),
                    arg_prefix: None,
                    path_glob: Some("src/**/*.rs".to_string()),
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "web_search".to_string(),
                    arg_prefix: Some("rust ".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "read".to_string(),
                    arg_prefix: Some("src".to_string()),
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "mcp__db__query".to_string(),
                    arg_prefix: Some("DROP ".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );

        // Non-path tool with injected "path" field must NOT match path_glob.
        let mut spoofed_bash = make_ctx("bash", Some("rm -rf /"), Some("src/main.rs"));
        spoofed_bash.cwd = cwd.to_path_buf();
        assert_eq!(store.evaluate(&spoofed_bash), None);

        // web_search matches query prefix.
        let mut ws_args = serde_json::Map::new();
        ws_args.insert("query".to_string(), serde_json::json!("rust async"));
        let ws_ctx = ToolInspectionContext {
            tool_call_id: "ws".to_string(),
            tool_name: "web_search".to_string(),
            args: ws_args,
            is_read_only: Some(true),
            cwd: cwd.to_path_buf(),
            origin: None,
        };
        assert_eq!(store.evaluate(&ws_ctx), Some(InspectionVerdict::Allow));

        // Segment boundary on path arg_prefix: "src" must NOT match "src_evil/secret.rs".
        let mut evil_read = make_ctx("read", None, Some("src_evil/secret.rs"));
        evil_read.cwd = cwd.to_path_buf();
        assert_eq!(store.evaluate(&evil_read), None);

        // Generic/MCP tool checks all candidate string fields (even if "command" is harmless).
        let mut mcp_args = serde_json::Map::new();
        mcp_args.insert("command".to_string(), serde_json::json!("SELECT 1"));
        mcp_args.insert("sql".to_string(), serde_json::json!("DROP TABLE users"));
        let mcp_ctx = ToolInspectionContext {
            tool_call_id: "mcp1".to_string(),
            tool_name: "mcp__db__query".to_string(),
            args: mcp_args,
            is_read_only: Some(false),
            cwd: cwd.to_path_buf(),
            origin: None,
        };
        assert!(matches!(
            store.evaluate(&mcp_ctx),
            Some(InspectionVerdict::Deny { .. })
        ));
    }

    #[test]
    fn add_session_rule_rejects_empty_invalid_and_degenerate_patterns() {
        let mut store = PermissionStore::default();
        for bad in [
            PermissionRule {
                tool_name: "   ".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            },
            PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("   ".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            },
            PermissionRule {
                tool_name: "edit".to_string(),
                arg_prefix: None,
                path_glob: Some("".to_string()),
                action: PermissionAction::AlwaysAllow,
            },
            PermissionRule {
                tool_name: "edit".to_string(),
                arg_prefix: None,
                path_glob: Some("[invalid".to_string()),
                action: PermissionAction::AlwaysAllow,
            },
            PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("git status && rm -rf /".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            },
        ] {
            assert!(
                store.add_session_rule(bad.clone()).is_err(),
                "expected invalid rule rejected: {bad:?}"
            );
        }
        assert!(store.session_rules().is_empty());
    }

    #[test]
    fn filtered_wildcard_never_allow_cannot_be_downgraded_and_wildcard_compound_prefix_rejected() {
        let mut store = PermissionStore::default();
        // Wildcard compound shell arg_prefix must fail validation.
        assert!(store
            .add_rule(PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: Some("git status && rm -rf /".to_string()),
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .is_err());

        // Filtered wildcard NeverAllow (`*` + path_glob) must NOT be downgraded by exact-tool AskBefore.
        store
            .add_rule(PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: None,
                path_glob: Some("**/.env*".to_string()),
                action: PermissionAction::NeverAllow,
            })
            .unwrap();
        store
            .add_rule(PermissionRule {
                tool_name: "read".to_string(),
                arg_prefix: None,
                path_glob: Some("src/**".to_string()),
                action: PermissionAction::AskBefore,
            })
            .unwrap();

        let cwd = std::env::temp_dir();
        let mut ctx = make_ctx("read", None, Some("src/.env.local"));
        ctx.cwd = cwd;
        assert!(
            matches!(store.evaluate(&ctx), Some(InspectionVerdict::Deny { .. })),
            "filtered wildcard NeverAllow must remain Deny even with exact-tool AskBefore"
        );
    }

    #[test]
    fn session_never_allow_beats_global_wildcard_plus_ask_before() {
        let mut store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "*".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "web_fetch".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::AskBefore,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::AskBefore,
                },
            ],
            None,
        );

        store
            .add_session_rule(PermissionRule {
                tool_name: "web_fetch".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .unwrap();
        store
            .add_session_rule(PermissionRule {
                tool_name: "bash".to_string(),
                arg_prefix: Some("curl".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .unwrap();

        let mut fetch_args = serde_json::Map::new();
        fetch_args.insert("url".to_string(), serde_json::json!("https://example.com"));
        let fetch_ctx = ToolInspectionContext {
            tool_call_id: "t1".to_string(),
            tool_name: "web_fetch".to_string(),
            args: fetch_args,
            is_read_only: Some(true),
            cwd: std::env::temp_dir(),
            origin: None,
        };
        assert!(
            matches!(store.evaluate(&fetch_ctx), Some(InspectionVerdict::Deny { .. })),
            "session NeverAllow on web_fetch must return Deny despite global * NeverAllow + web_fetch AskBefore"
        );

        assert!(
            matches!(
                store.evaluate(&make_ctx("bash", Some("curl https://example.com"), None)),
                Some(InspectionVerdict::Deny { .. })
            ),
            "session NeverAllow on bash must return Deny despite global * NeverAllow + bash AskBefore"
        );
        assert!(
            matches!(
                store.evaluate(&make_ctx("bash", Some("echo ok"), None)),
                Some(InspectionVerdict::RequireApproval { .. })
            ),
            "unmatched bash command should still fall back to global AskBefore"
        );
    }

    #[tokio::test]
    async fn c3_unparseable_shell_never_falsely_denies_unrelated_and_never_silently_allows() {
        // 1. Unrelated NeverAllow("git push --force") must NOT deny "cargo test 2>&1";
        //    even with AlwaysAllow("bash"), unparseable command must fail closed to RequireApproval.
        let store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: None,
                    path_glob: None,
                    action: PermissionAction::AlwaysAllow,
                },
                PermissionRule {
                    tool_name: "bash".to_string(),
                    arg_prefix: Some("git push --force".to_string()),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
            ],
            None,
        );
        let cargo_ctx = make_ctx("bash", Some("cargo test 2>&1"), None);
        assert!(
            matches!(
                store.evaluate(&cargo_ctx),
                Some(InspectionVerdict::RequireApproval { .. })
            ),
            "cargo test 2>&1 must require approval, not Deny or Allow: {:?}",
            store.evaluate(&cargo_ctx)
        );

        // 2. Empty store in PermissionMode::Auto must still require approval for unparseable shell.
        let mgr = crate::tool_inspection::ToolInspectionManager::new_with_store(
            crate::tool_inspection::PermissionMode::Auto,
            PathBuf::from("."),
            PermissionStore::default(),
        );
        assert!(
            matches!(
                mgr.inspect(&cargo_ctx).await,
                InspectionVerdict::RequireApproval { .. }
            ),
            "Auto mode must not silently allow unparseable shell command"
        );
    }

    #[test]
    fn c4_ls_find_grep_without_path_evaluates_dot_against_cwd() {
        let dir = crate::TempTestDir::new("perm_c4_dot");
        let sub = dir.path().join("sub");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let sub_canon = sub.canonicalize().unwrap();
        let sub_prefix = crate::settings::strip_windows_verbatim_prefix(&sub_canon.to_string_lossy())
            .into_owned();

        let store = PermissionStore::new(
            vec![
                PermissionRule {
                    tool_name: "ls".to_string(),
                    arg_prefix: Some(sub_prefix),
                    path_glob: None,
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "find".to_string(),
                    arg_prefix: None,
                    path_glob: Some("**/sub".to_string()),
                    action: PermissionAction::NeverAllow,
                },
                PermissionRule {
                    tool_name: "grep".to_string(),
                    arg_prefix: None,
                    path_glob: Some("**/sub".to_string()),
                    action: PermissionAction::AlwaysAllow,
                },
            ],
            None,
        );

        let mut ls_ctx = make_ctx("LS", None, None);
        ls_ctx.cwd = sub.clone();
        assert!(
            matches!(store.evaluate(&ls_ctx), Some(InspectionVerdict::Deny { .. })),
            "ls without path must match arg_prefix against cwd: {:?}",
            store.evaluate(&ls_ctx)
        );

        let mut find_ctx = make_ctx("Find", None, None);
        find_ctx.cwd = sub.clone();
        assert!(
            matches!(store.evaluate(&find_ctx), Some(InspectionVerdict::Deny { .. })),
            "find without path must match path_glob against cwd: {:?}",
            store.evaluate(&find_ctx)
        );

        let mut grep_sub = make_ctx("grep", None, None);
        grep_sub.cwd = sub;
        assert_eq!(store.evaluate(&grep_sub), Some(InspectionVerdict::Allow));

        let mut grep_other = make_ctx("grep", None, None);
        grep_other.cwd = other;
        assert_eq!(store.evaluate(&grep_other), None);
    }

    #[tokio::test]
    async fn i15_deny_all_surfaces_load_error_and_wildcard_validation_allows_url_path() {
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
        let dir = crate::TempTestDir::new("perm_i15");
        let agent_dir = dir.path().join(".mehh").join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let corrupt = agent_dir.join("permissions.json");
        std::fs::write(&corrupt, "{not valid json").unwrap();
        std::env::set_var("USERPROFILE", dir.path());
        std::env::set_var("HOME", dir.path());
        let _guard = HomeGuard(prev_up, prev_home);

        let store = PermissionStore::load_fail_closed(dir.path(), false);
        let ctx = make_ctx("read", None, Some("notes.txt"));
        match store.evaluate(&ctx) {
            Some(InspectionVerdict::Deny { reason }) => {
                assert!(
                    reason.contains("permissions.json") || reason.contains("Failed to parse"),
                    "deny_all must surface exact load error, got: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        let mut valid_store = PermissionStore::default();
        // Untrimmed tool_name must be rejected
        assert!(valid_store
            .add_session_rule(PermissionRule {
                tool_name: " bash ".to_string(),
                arg_prefix: None,
                path_glob: None,
                action: PermissionAction::AlwaysAllow,
            })
            .is_err());

        // Wildcard with valid HTTP URL (including query `&`) and non-compound path prefix allowed
        valid_store
            .add_session_rule(PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: Some("https://docs.rs/search?q=a&b=c".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .expect("wildcard valid HTTP URL prefix must be accepted");
        valid_store
            .add_session_rule(PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: Some("~/secrets".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .expect("wildcard non-compound path prefix must be accepted");
        // Compound shell operators on wildcard must still be rejected
        assert!(valid_store
            .add_session_rule(PermissionRule {
                tool_name: "*".to_string(),
                arg_prefix: Some("/tmp/a; rm -rf /".to_string()),
                path_glob: None,
                action: PermissionAction::NeverAllow,
            })
            .is_err());
    }
}
