//! User and project settings management.

use anyhow::Context;
use std::path::{Path, PathBuf};

static ATOMIC_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Strips Windows verbatim path prefixes (`\\?\` and `\\?\UNC\`, plus forward-slash
/// `//?/` / `//?/UNC/` equivalents) while preserving the underlying drive or UNC path.
///
/// Maps `\\?\UNC\server\share` -> `\\server\share` and `\\?\C:\...` -> `C:\...`.
pub(crate) fn strip_windows_verbatim_prefix(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        if let Some(unc_rest) = rest.get(..4).filter(|p| p.eq_ignore_ascii_case(r"unc\")) {
            let _ = unc_rest;
            return std::borrow::Cow::Owned(format!(r"\\{}", &rest[4..]));
        }
        if rest.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("unc/")) {
            return std::borrow::Cow::Owned(format!(r"\\{}", &rest[4..]));
        }
        return std::borrow::Cow::Borrowed(rest);
    }
    if let Some(rest) = path.strip_prefix("//?/") {
        if rest.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("unc/")) {
            return std::borrow::Cow::Owned(format!("//{}", &rest[4..]));
        }
        if rest.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"unc\")) {
            return std::borrow::Cow::Owned(format!(r"\\{}", &rest[4..]));
        }
        return std::borrow::Cow::Borrowed(rest);
    }
    std::borrow::Cow::Borrowed(path)
}

/// Replaces the DACL on a freshly created temp file with a single entry for the
/// current process user. On Windows there is no std API for this; without it
/// `restricted` writes (credentials, permissions) inherit the parent ACL.
#[cfg(windows)]
fn restrict_to_current_user_windows(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT,
        TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            anyhow::bail!("OpenProcessToken failed");
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        if len == 0 {
            CloseHandle(token);
            anyhow::bail!("GetTokenInformation returned no token size");
        }
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            anyhow::bail!("GetTokenInformation failed");
        }
        let user = buf.as_ptr().cast::<TOKEN_USER>().read_unaligned();
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid as windows_sys::core::PWSTR,
        };
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: trustee,
        };
        let mut acl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
        let status = SetEntriesInAclW(1, &explicit, std::ptr::null(), &mut acl);
        if status != ERROR_SUCCESS {
            anyhow::bail!("SetEntriesInAclW failed with {status}");
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let status = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        );
        LocalFree(acl.cast());
        if status != ERROR_SUCCESS {
            anyhow::bail!("SetNamedSecurityInfoW failed with {status}");
        }
    }
    Ok(())
}

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

    // Restrict Windows DACL right after creating the temp file and before writing sensitive bytes.
    #[cfg(windows)]
    if restricted {
        if let Err(e) = restrict_to_current_user_windows(&tmp_path) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).with_context(|| {
                format!("Failed to restrict permissions on: {}", tmp_path.display())
            });
        }
    }

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

    #[cfg(windows)]
    let rename_res = {
        let delays_ms = [5u64, 15, 30, 60];
        let mut attempt = 0usize;
        loop {
            match std::fs::rename(&tmp_path, path) {
                Ok(()) => break Ok(()),
                Err(e)
                    if attempt < delays_ms.len()
                        && (e.kind() == std::io::ErrorKind::PermissionDenied
                            || matches!(e.raw_os_error(), Some(32 | 33))) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(delays_ms[attempt]));
                    attempt += 1;
                }
                Err(e) => break Err(e),
            }
        }
    };

    #[cfg(not(windows))]
    let rename_res = std::fs::rename(&tmp_path, path);

    if let Err(e) = rename_res {
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

/// Validated web search/fetch provider selection in `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebProviderSetting {
    Auto,
    Disabled,
    Mehh,
    Exa,
    Parallel,
    Firecrawl,
    Tavily,
    TinyFish,
    Keenable,
}

impl From<WebProviderSetting> for mehh_tools::web::WebProviderChoice {
    fn from(value: WebProviderSetting) -> Self {
        use mehh_tools::web::{ProviderId, WebProviderChoice};
        match value {
            WebProviderSetting::Auto => WebProviderChoice::Auto,
            WebProviderSetting::Disabled => WebProviderChoice::Disabled,
            WebProviderSetting::Mehh => WebProviderChoice::Fixed(ProviderId::Mehh),
            WebProviderSetting::Exa => WebProviderChoice::Fixed(ProviderId::Exa),
            WebProviderSetting::Parallel => WebProviderChoice::Fixed(ProviderId::Parallel),
            WebProviderSetting::Firecrawl => WebProviderChoice::Fixed(ProviderId::Firecrawl),
            WebProviderSetting::Tavily => WebProviderChoice::Fixed(ProviderId::Tavily),
            WebProviderSetting::TinyFish => WebProviderChoice::Fixed(ProviderId::TinyFish),
            WebProviderSetting::Keenable => WebProviderChoice::Fixed(ProviderId::Keenable),
        }
    }
}

/// Web search & web fetch configuration under `"web"` in `settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<WebProviderSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_private_urls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_fallback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

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
    pub hide_thinking_block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::tool_inspection::PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<WebSettings>,
}

impl Settings {
    /// Converts `settings.web` into `WebConfig` for building web tools.
    pub fn to_web_config(&self) -> mehh_tools::web::WebConfig {
        let mut cfg = mehh_tools::web::WebConfig::default();
        if let Some(ref web) = self.web {
            if let Some(enabled) = web.enabled {
                cfg.enabled = enabled;
            }
            if let Some(provider) = web.provider {
                cfg.provider = provider.into();
            }
            if let Some(allow_private) = web.allow_private_urls {
                cfg.allow_private_urls = allow_private;
            }
            if let Some(fetch_fallback) = web.fetch_fallback {
                cfg.fetch_fallback = fetch_fallback;
            }
            if let Some(timeout_seconds) = web.timeout_seconds {
                cfg.timeout_seconds = timeout_seconds;
            }
        }
        cfg
    }
}

/// Settings loader and merger.
pub struct SettingsManager;

impl SettingsManager {
    /// Returns the global settings file path `~/.mehh/agent/settings.json`.
    pub fn global_path() -> anyhow::Result<PathBuf> {
        let home = crate::user_home_dir().ok_or_else(|| {
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
            hide_thinking_block: overlay.hide_thinking_block.or(base.hide_thinking_block),
            permission_mode: overlay.permission_mode.or(base.permission_mode),
            worker_model: overlay.worker_model.or(base.worker_model),
            lead_turns: overlay.lead_turns.or(base.lead_turns),
            web: merge_web_settings(base.web, overlay.web),
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
        // Escalation guard: a trusted project file may only tighten permissions, never loosen
        // them. Permissiveness ranks Chat < Approve < SmartApprove < Auto. When there is no
        // global preference, SmartApprove is the baseline. A more permissive project value is
        // dropped (neutralized to None) so it cannot override the user's global preference.
        if let Some(project_mode) = local.permission_mode {
            let baseline = global
                .permission_mode
                .unwrap_or(crate::tool_inspection::PermissionMode::SmartApprove);
            if permission_mode_rank(project_mode) > permission_mode_rank(baseline) {
                local.permission_mode = None;
            }
        }
        enforce_web_escalation_guard(&global.web, &mut local.web);
        Ok(Self::merge(global, local))
    }
}

fn merge_web_settings(
    base: Option<WebSettings>,
    overlay: Option<WebSettings>,
) -> Option<WebSettings> {
    match (base, overlay) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(o)) => Some(o),
        (Some(b), Some(o)) => Some(WebSettings {
            enabled: o.enabled.or(b.enabled),
            provider: o.provider.or(b.provider),
            allow_private_urls: o.allow_private_urls.or(b.allow_private_urls),
            fetch_fallback: o.fetch_fallback.or(b.fetch_fallback),
            timeout_seconds: o.timeout_seconds.or(b.timeout_seconds),
        }),
    }
}

fn web_provider_rank(provider: WebProviderSetting) -> u8 {
    match provider {
        WebProviderSetting::Disabled => 0,
        WebProviderSetting::Auto => 2,
        _ => 1,
    }
}

fn enforce_web_escalation_guard(global: &Option<WebSettings>, local: &mut Option<WebSettings>) {
    let Some(ref mut proj) = local else {
        return;
    };
    let g = global.as_ref();
    let global_allow_private = g.and_then(|w| w.allow_private_urls).unwrap_or(false);
    if proj.allow_private_urls == Some(true) && !global_allow_private {
        proj.allow_private_urls = None;
    }
    if proj.enabled == Some(true) && g.and_then(|w| w.enabled) == Some(false) {
        proj.enabled = None;
    }
    if proj.fetch_fallback == Some(true) && g.and_then(|w| w.fetch_fallback) == Some(false) {
        proj.fetch_fallback = None;
    }
    if let Some(proj_prov) = proj.provider {
        let base_prov = g.and_then(|w| w.provider).unwrap_or(WebProviderSetting::Auto);
        let proj_rank = web_provider_rank(proj_prov);
        let base_rank = web_provider_rank(base_prov);
        if proj_rank > base_rank || (proj_rank == 1 && base_rank == 1 && proj_prov != base_prov) {
            proj.provider = None;
        }
    }
}

/// Permissiveness ranking used by the project-settings escalation guard: Chat is the strictest
/// and Auto is the most permissive.
pub(crate) fn permission_mode_rank(mode: crate::tool_inspection::PermissionMode) -> u8 {
    use crate::tool_inspection::PermissionMode::*;
    match mode {
        Chat => 0,
        Approve => 1,
        SmartApprove => 2,
        Auto => 3,
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
            hide_thinking_block: Some(true),
            permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
            worker_model: None,
            lead_turns: None,
            web: None,
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
            hide_thinking_block: Some(false),
            permission_mode: Some(crate::tool_inspection::PermissionMode::SmartApprove),
            worker_model: Some("gemini-2.5-flash".to_string()),
            lead_turns: Some(2),
            web: None,
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
            hide_thinking_block: Some(true),
            permission_mode: Some(crate::tool_inspection::PermissionMode::Auto),
            worker_model: None,
            lead_turns: None,
            web: None,
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
    fn test_project_settings_cannot_escalate_chat_to_smart_approve() {
        let dir = crate::TempTestDir::new("settings_chat_escalation");
        let global_path = dir.path().join("global-settings.json");
        let project_path = dir.path().join("project-settings.json");

        // Global Chat must not be escalated to SmartApprove by a trusted project file.
        std::fs::write(&global_path, r#"{"permissionMode":"chat"}"#).unwrap();
        std::fs::write(&project_path, r#"{"permissionMode":"smart_approve"}"#).unwrap();
        let loaded =
            SettingsManager::try_load_from_paths(&global_path, &project_path, true).unwrap();
        assert_eq!(
            loaded.permission_mode,
            Some(crate::tool_inspection::PermissionMode::Chat),
            "project must not escalate global chat to smart_approve"
        );

        // An equal-or-stricter project value (Approve is stricter than SmartApprove) is preserved.
        std::fs::write(&global_path, r#"{"permissionMode":"smart_approve"}"#).unwrap();
        std::fs::write(&project_path, r#"{"permissionMode":"approve"}"#).unwrap();
        let loaded2 =
            SettingsManager::try_load_from_paths(&global_path, &project_path, true).unwrap();
        assert_eq!(
            loaded2.permission_mode,
            Some(crate::tool_inspection::PermissionMode::Approve),
            "equal-or-stricter project value must be preserved"
        );
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
        // Equal-rank Auto is not an escalation, so the project value is preserved.
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

    #[test]
    fn test_web_settings_roundtrip_and_merge() {
        let dir = crate::TempTestDir::new("web_settings");
        let path = dir.path().join("settings.json");
        let json_raw = r#"{
            "web": {
                "enabled": true,
                "provider": "exa",
                "allowPrivateUrls": false,
                "fetchFallback": true,
                "timeoutSeconds": 45
            }
        }"#;
        std::fs::write(&path, json_raw).unwrap();
        let loaded = SettingsManager::load_file(&path).unwrap();
        let cfg = loaded.to_web_config();
        assert!(cfg.enabled);
        assert_eq!(
            cfg.provider,
            mehh_tools::web::WebProviderChoice::Fixed(mehh_tools::web::ProviderId::Exa)
        );
        assert!(!cfg.allow_private_urls);
        assert!(cfg.fetch_fallback);
        assert_eq!(cfg.timeout_seconds, 45);

        // Unknown provider fails JSON parse closed
        std::fs::write(&path, r#"{"web":{"provider":"unknown_vendor"}}"#).unwrap();
        assert!(SettingsManager::load_file(&path).is_err());

        // Field-by-field merge
        let merged = SettingsManager::merge(
            loaded,
            Settings {
                web: Some(WebSettings {
                    timeout_seconds: Some(60),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let merged_cfg = merged.to_web_config();
        assert_eq!(merged_cfg.timeout_seconds, 60);
        assert_eq!(
            merged_cfg.provider,
            mehh_tools::web::WebProviderChoice::Fixed(mehh_tools::web::ProviderId::Exa)
        );
    }

    #[test]
    fn test_web_settings_project_escalation_guard() {
        let dir = crate::TempTestDir::new("web_settings_guard");
        let global = dir.path().join("global.json");
        let project = dir.path().join("project.json");

        std::fs::write(
            &global,
            r#"{"web":{"enabled":false,"provider":"disabled","allowPrivateUrls":false,"fetchFallback":false}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"web":{"enabled":true,"provider":"auto","allowPrivateUrls":true,"fetchFallback":true}}"#,
        )
        .unwrap();
        let res = SettingsManager::try_load_from_paths(&global, &project, true).unwrap();
        let w = res.web.unwrap();
        assert_eq!(w.enabled, Some(false));
        assert_eq!(w.provider, Some(WebProviderSetting::Disabled));
        assert_eq!(w.allow_private_urls, Some(false));
        assert_eq!(w.fetch_fallback, Some(false));

        // Pinned vendor cannot be switched or widened to auto, but can be tightened to disabled
        std::fs::write(&global, r#"{"web":{"provider":"exa"}}"#).unwrap();
        std::fs::write(&project, r#"{"web":{"provider":"keenable"}}"#).unwrap();
        let switch_attempt =
            SettingsManager::try_load_from_paths(&global, &project, true).unwrap();
        assert_eq!(
            switch_attempt.web.unwrap().provider,
            Some(WebProviderSetting::Exa)
        );

        std::fs::write(&project, r#"{"web":{"provider":"disabled"}}"#).unwrap();
        let tighten = SettingsManager::try_load_from_paths(&global, &project, true).unwrap();
        assert_eq!(
            tighten.web.unwrap().provider,
            Some(WebProviderSetting::Disabled)
        );

        // Mehh provider roundtrip & legacy "searxng" fails closed
        std::fs::write(&global, r#"{"web":{"provider":"mehh"}}"#).unwrap();
        let mehh_loaded = SettingsManager::load_file(&global).unwrap();
        assert_eq!(
            mehh_loaded.to_web_config().provider,
            mehh_tools::web::WebProviderChoice::Fixed(mehh_tools::web::ProviderId::Mehh)
        );

        std::fs::write(&project, r#"{"web":{"provider":"searxng"}}"#).unwrap();
        assert!(SettingsManager::load_file(&project).is_err());
    }

    #[test]
    fn test_strip_windows_verbatim_prefix() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\C:\Users\Admin\repo").as_ref(),
            r"C:\Users\Admin\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\unc\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\Unc\server\share").as_ref(),
            r"\\server\share"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("//?/UNC/server/share/repo").as_ref(),
            "//server/share/repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("//?/C:/Users/Admin").as_ref(),
            "C:/Users/Admin"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("/var/tmp/repo").as_ref(),
            "/var/tmp/repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\server\share\repo").as_ref(),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn test_write_file_atomic_overwrites_existing_with_restricted_acl() {
        let dir = crate::TempTestDir::new("atomic_write");
        let path = dir.path().join("secret.json");
        write_file_atomic(&path, b"first", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_file_atomic(&path, b"second", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }
}
