//! Theme definition, parsing, and color utilities for mehh TUI.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Theme color tokens and styling configuration for TUI components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub name: String,
    pub accent: String,
    pub border: String,
    pub border_accent: String,
    pub border_muted: String,
    pub success: String,
    pub error: String,
    pub warning: String,
    pub muted: String,
    pub dim: String,
    pub text: String,
    pub thinking_text: String,
    pub user_prefix: String,
    /// Soft background block behind a completed user turn.
    #[serde(default)]
    pub user_bg: String,
    /// Raised surface for modals, code blocks, and inline code.
    #[serde(default)]
    pub surface_elevated: String,
    /// Full-width background for added/removed diff lines.
    #[serde(default)]
    pub diff_add_bg: String,
    #[serde(default)]
    pub diff_del_bg: String,
    pub tool_pending: String,
    pub tool_success: String,
    pub tool_error: String,
    pub selected_bg: String,
    pub md_heading: String,
    pub md_code: String,
    pub md_quote: String,
    pub md_link: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Parse a 3- or 6-digit hex string into (r, g, b) bytes.
fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let clean = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if !clean.is_ascii() {
        return None;
    }
    if clean.len() == 6 {
        let r = u8::from_str_radix(&clean[0..2], 16).ok()?;
        let g = u8::from_str_radix(&clean[2..4], 16).ok()?;
        let b = u8::from_str_radix(&clean[4..6], 16).ok()?;
        Some((r, g, b))
    } else if clean.len() == 3 {
        let r = u8::from_str_radix(&clean[0..1], 16).ok()?;
        let g = u8::from_str_radix(&clean[1..2], 16).ok()?;
        let b = u8::from_str_radix(&clean[2..3], 16).ok()?;
        Some((r * 17, g * 17, b * 17))
    } else {
        None
    }
}

/// Convert a `#rrggbb` or `#rgb` hex string to an ANSI truecolor 24-bit foreground escape code.
pub fn hex_to_fg_ansi(hex: &str) -> Option<String> {
    let (r, g, b) = parse_hex_color(hex)?;
    Some(format!("\x1b[38;2;{r};{g};{b}m"))
}

/// Convert a `#rrggbb` or `#rgb` hex string to an ANSI truecolor 24-bit background escape code.
pub fn hex_to_bg_ansi(hex: &str) -> Option<String> {
    let (r, g, b) = parse_hex_color(hex)?;
    Some(format!("\x1b[48;2;{r};{g};{b}m"))
}

fn is_valid_sgr(s: &str) -> bool {
    if !s.starts_with("\x1b[") || !s.ends_with('m') {
        return false;
    }
    let mut rest = s;
    while !rest.is_empty() {
        let Some(after_esc) = rest.strip_prefix("\x1b[") else {
            return false;
        };
        let Some(m_idx) = after_esc.find('m') else {
            return false;
        };
        let params = &after_esc[..m_idx];
        if !params.bytes().all(|b| b.is_ascii_digit() || b == b';') {
            return false;
        }
        rest = &after_esc[m_idx + 1..];
    }
    true
}

/// Resolve a color value string: converts hex colors to ANSI SGR escapes, or preserves valid SGR escapes.
fn resolve_color_value(val: &str, is_bg: bool) -> String {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // If it's a valid hex code, convert it
    if let Some(ansi) = if is_bg {
        hex_to_bg_ansi(trimmed)
    } else {
        hex_to_fg_ansi(trimmed)
    } {
        return ansi;
    }

    // Common standard color name mapping
    match trimmed.to_ascii_lowercase().as_str() {
        "black" => (if is_bg { "\x1b[40m" } else { "\x1b[30m" }).to_string(),
        "red" => (if is_bg { "\x1b[41m" } else { "\x1b[31m" }).to_string(),
        "green" => (if is_bg { "\x1b[42m" } else { "\x1b[32m" }).to_string(),
        "yellow" => (if is_bg { "\x1b[43m" } else { "\x1b[33m" }).to_string(),
        "blue" => (if is_bg { "\x1b[44m" } else { "\x1b[34m" }).to_string(),
        "magenta" | "violet" | "purple" => {
            (if is_bg { "\x1b[45m" } else { "\x1b[35m" }).to_string()
        }
        "cyan" => (if is_bg { "\x1b[46m" } else { "\x1b[36m" }).to_string(),
        "white" => (if is_bg { "\x1b[47m" } else { "\x1b[37m" }).to_string(),
        "gray" | "grey" | "muted" => (if is_bg { "\x1b[100m" } else { "\x1b[90m" }).to_string(),
        // Valid raw SGR escapes only; reject unrecognized strings and non-SGR sequences
        _ if is_valid_sgr(trimmed) => trimmed.to_string(),
        _ => String::new(),
    }
}

impl Theme {
    /// Built-in dark theme: low-saturation truecolor palette with a warm accent.
    /// Each token is a complete SGR prefix so components never hardcode colors.
    pub fn dark() -> Self {
        Self {
            name: "dark".to_string(),
            accent: "\x1b[38;2;217;119;87m".to_string(),
            border: "\x1b[38;2;58;61;69m".to_string(),
            border_accent: "\x1b[38;2;217;119;87m".to_string(),
            border_muted: "\x1b[38;2;42;45;53m".to_string(),
            success: "\x1b[38;2;74;222;128m".to_string(),
            error: "\x1b[38;2;248;113;113m".to_string(),
            warning: "\x1b[38;2;251;191;36m".to_string(),
            muted: "\x1b[38;2;161;161;170m".to_string(),
            dim: "\x1b[38;2;142;142;152m".to_string(),
            text: "\x1b[38;2;228;228;231m".to_string(),
            thinking_text: "\x1b[3;38;2;113;113;122m".to_string(),
            user_prefix: "\x1b[1;38;2;217;119;87m".to_string(),
            user_bg: "\x1b[48;2;35;38;46m".to_string(),
            surface_elevated: "\x1b[48;2;30;32;40m".to_string(),
            diff_add_bg: "\x1b[48;2;22;43;32m".to_string(),
            diff_del_bg: "\x1b[48;2;45;24;28m".to_string(),
            tool_pending: "\x1b[38;2;251;191;36m".to_string(),
            tool_success: "\x1b[38;2;74;222;128m".to_string(),
            tool_error: "\x1b[38;2;248;113;113m".to_string(),
            selected_bg: "\x1b[48;2;49;53;64m".to_string(),
            md_heading: "\x1b[1;38;2;217;119;87m".to_string(),
            md_code: "\x1b[38;2;167;178;181m".to_string(),
            md_quote: "\x1b[38;2;113;113;122m".to_string(),
            md_link: "\x1b[4;38;2;122;162;247m".to_string(),
        }
    }

    /// Built-in light theme: matching palette tuned for bright terminals.
    pub fn light() -> Self {
        Self {
            name: "light".to_string(),
            accent: "\x1b[38;2;180;83;9m".to_string(),
            border: "\x1b[38;2;212;212;216m".to_string(),
            border_accent: "\x1b[38;2;180;83;9m".to_string(),
            border_muted: "\x1b[38;2;228;228;231m".to_string(),
            success: "\x1b[38;2;21;128;61m".to_string(),
            error: "\x1b[38;2;185;28;28m".to_string(),
            warning: "\x1b[38;2;161;98;7m".to_string(),
            muted: "\x1b[38;2;113;113;122m".to_string(),
            dim: "\x1b[38;2;100;100;110m".to_string(),
            text: "\x1b[38;2;24;24;27m".to_string(),
            thinking_text: "\x1b[3;38;2;113;113;122m".to_string(),
            user_prefix: "\x1b[1;38;2;180;83;9m".to_string(),
            user_bg: "\x1b[48;2;244;244;245m".to_string(),
            surface_elevated: "\x1b[48;2;244;244;245m".to_string(),
            diff_add_bg: "\x1b[48;2;220;252;231m".to_string(),
            diff_del_bg: "\x1b[48;2;254;226;226m".to_string(),
            tool_pending: "\x1b[38;2;161;98;7m".to_string(),
            tool_success: "\x1b[38;2;21;128;61m".to_string(),
            tool_error: "\x1b[38;2;185;28;28m".to_string(),
            selected_bg: "\x1b[48;2;228;228;231m".to_string(),
            md_heading: "\x1b[1;38;2;180;83;9m".to_string(),
            md_code: "\x1b[38;2;63;63;70m".to_string(),
            md_quote: "\x1b[38;2;113;113;122m".to_string(),
            md_link: "\x1b[4;38;2;29;78;216m".to_string(),
        }
    }

    /// Resolve built-in theme by name ("light" -> light, else dark).
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    fn is_explicit_path(name_or_path: &str) -> bool {
        name_or_path.contains('/')
            || name_or_path.contains('\\')
            || name_or_path.starts_with('.')
            || name_or_path.to_ascii_lowercase().ends_with(".json")
            || Path::new(name_or_path).is_absolute()
    }

    /// Try to load a theme by built-in preset name, `~/.mehh/agent/themes/<name>.json`,
    /// or an explicit file path. Distinguishes missing named presets from IO/parse errors
    /// on explicit paths or existing theme files, and strips any leading UTF-8 BOM.
    pub fn try_load(name_or_path: &str) -> Result<Theme, String> {
        let name_str = name_or_path.trim();
        if name_str.is_empty() {
            return Err("Theme name or path is empty".to_string());
        }

        let mut theme = Self::from_name(name_str);
        let direct_path = Path::new(name_str);
        let explicit = Self::is_explicit_path(name_str);

        let file_path = if explicit {
            direct_path.to_path_buf()
        } else {
            match Self::theme_file_path(name_str) {
                Some(p) => p,
                None => {
                    return if matches!(name_str.to_ascii_lowercase().as_str(), "dark" | "light") {
                        Ok(theme)
                    } else {
                        Err(format!("Theme '{name_str}' not found"))
                    };
                }
            }
        };

        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => {
                return if matches!(name_str.to_ascii_lowercase().as_str(), "dark" | "light") {
                    Ok(theme)
                } else {
                    Err(format!("Theme '{name_str}' not found"))
                };
            }
            Err(e) => {
                return Err(format!(
                    "Failed to read theme '{}': {e}",
                    file_path.display()
                ));
            }
        };

        let clean = content.strip_prefix('\u{feff}').unwrap_or(&content);
        let val: serde_json::Value = serde_json::from_str(clean)
            .map_err(|e| format!("Failed to parse theme '{}': {e}", file_path.display()))?;
        if !val.is_object() {
            return Err(format!(
                "Invalid theme '{}': expected JSON object",
                file_path.display()
            ));
        }

        Self::apply_json_value(&mut theme, &val);
        Ok(theme)
    }

    /// Load a theme by name: checks `~/.mehh/agent/themes/<name>.json` (or a direct path),
    /// supporting `#rrggbb` hex colors converted to `\x1b[38;2;r;g;bm` or valid SGR escapes,
    /// falling back to `Theme::from_name(...)` / `Theme::dark()`.
    pub fn load(name: Option<&str>) -> Self {
        let name_str = match name {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => return Self::dark(),
        };
        Self::try_load(name_str).unwrap_or_else(|_| Self::from_name(name_str))
    }

    fn theme_file_path(name: &str) -> Option<PathBuf> {
        let home = std::env::var_os("USERPROFILE")
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))
            .map(PathBuf::from)?;
        let filename = if name.ends_with(".json") {
            name.to_string()
        } else {
            format!("{name}.json")
        };
        Some(
            home.join(".mehh")
                .join("agent")
                .join("themes")
                .join(filename),
        )
    }

    fn apply_json_value(theme: &mut Theme, val: &serde_json::Value) {
        let obj = match val.as_object() {
            Some(o) => o,
            None => return,
        };

        if let Some(n) = obj.get("name").and_then(|v| v.as_str()) {
            let clean_name = crate::ansi::sanitize_terminal_text(n, false);
            if !clean_name.trim().is_empty() {
                theme.name = clean_name;
            }
        }

        let vars = obj.get("vars").and_then(|v| v.as_object());
        let colors = obj.get("colors").and_then(|v| v.as_object()).unwrap_or(obj);

        let get_color = |camel: &str, snake: &str, is_bg: bool| -> Option<String> {
            let mut raw = colors
                .get(camel)
                .or_else(|| colors.get(snake))
                .and_then(|v| v.as_str())?;

            // If raw references a key in vars, resolve it
            if let Some(v_obj) = vars {
                if let Some(resolved) = v_obj.get(raw).and_then(|v| v.as_str()) {
                    raw = resolved;
                }
            }

            Some(resolve_color_value(raw, is_bg))
        };

        if let Some(c) = get_color("accent", "accent", false) {
            theme.accent = c;
        }
        if let Some(c) = get_color("border", "border", false) {
            theme.border = c;
        }
        if let Some(c) = get_color("borderAccent", "border_accent", false) {
            theme.border_accent = c;
        }
        if let Some(c) = get_color("borderMuted", "border_muted", false) {
            theme.border_muted = c;
        }
        if let Some(c) = get_color("success", "success", false) {
            theme.success = c;
        }
        if let Some(c) = get_color("error", "error", false) {
            theme.error = c;
        }
        if let Some(c) = get_color("warning", "warning", false) {
            theme.warning = c;
        }
        if let Some(c) = get_color("muted", "muted", false) {
            theme.muted = c;
        }
        if let Some(c) = get_color("dim", "dim", false) {
            theme.dim = c;
        }
        if let Some(c) = get_color("text", "text", false) {
            theme.text = c;
        }
        if let Some(c) = get_color("thinkingText", "thinking_text", false) {
            theme.thinking_text = c;
        }
        if let Some(c) = get_color("userPrefix", "user_prefix", false) {
            theme.user_prefix = c;
        }
        if let Some(c) = get_color("userBg", "user_bg", true) {
            theme.user_bg = c;
        }
        if let Some(c) = get_color("diffAddBg", "diff_add_bg", true) {
            theme.diff_add_bg = c;
        }
        if let Some(c) = get_color("diffDelBg", "diff_del_bg", true) {
            theme.diff_del_bg = c;
        }
        if let Some(c) = get_color("toolPending", "tool_pending", false) {
            theme.tool_pending = c;
        }
        if let Some(c) = get_color("toolSuccess", "tool_success", false) {
            theme.tool_success = c;
        }
        if let Some(c) = get_color("toolError", "tool_error", false) {
            theme.tool_error = c;
        }
        if let Some(c) = get_color("selectedBg", "selected_bg", true) {
            theme.selected_bg = c;
        }
        if let Some(c) = get_color("surfaceElevated", "surface_elevated", true) {
            theme.surface_elevated = c;
        }
        if let Some(c) = get_color("mdHeading", "md_heading", false) {
            theme.md_heading = c;
        }
        if let Some(c) = get_color("mdCode", "md_code", false) {
            theme.md_code = c;
        }
        if let Some(c) = get_color("mdQuote", "md_quote", false) {
            theme.md_quote = c;
        }
        if let Some(c) = get_color("mdLink", "md_link", false) {
            theme.md_link = c;
        }
    }

    /// Wrap `text` with `color_code` and reset `\x1b[0m` if `color_code` is non-empty.
    pub fn fg(&self, color_code: &str, text: &str) -> String {
        if color_code.is_empty() {
            text.to_string()
        } else {
            format!("{color_code}{text}\x1b[0m")
        }
    }

    /// Wrap `text` with `color_code` (background) and reset `\x1b[0m` if `color_code` is non-empty.
    pub fn bg(&self, color_code: &str, text: &str) -> String {
        if color_code.is_empty() {
            text.to_string()
        } else {
            format!("{color_code}{text}\x1b[0m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_tokens_match_spec() {
        assert_eq!(Theme::dark().dim, "\x1b[38;2;142;142;152m");
        assert_eq!(Theme::dark().muted, "\x1b[38;2;161;161;170m");
        assert_eq!(Theme::light().dim, "\x1b[38;2;100;100;110m");
        assert_eq!(Theme::dark().surface_elevated, "\x1b[48;2;30;32;40m");
        assert_eq!(Theme::light().surface_elevated, "\x1b[48;2;244;244;245m");
    }

    #[test]
    fn custom_theme_without_surface_elevated_loads() {
        let tmp = std::env::temp_dir().join(format!("mehh-theme-test-{}.json", std::process::id()));
        std::fs::write(&tmp, r##"{"name":"custom","colors":{"dim":"#123456"}}"##).unwrap();
        let t = Theme::load(Some(tmp.to_str().unwrap()));
        assert_eq!(t.name, "custom");
        assert_eq!(t.dim, "\x1b[38;2;18;52;86m");
        assert_eq!(t.surface_elevated, Theme::dark().surface_elevated);
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn theme_load_strips_utf8_bom() {
        let tmp = std::env::temp_dir().join(format!("mehh-theme-bom-{}.json", std::process::id()));
        std::fs::write(&tmp, "\u{feff}{\"name\":\"bommed\",\"accent\":\"#112233\"}").unwrap();
        let t = Theme::load(Some(tmp.to_str().unwrap()));
        assert_eq!(t.name, "bommed");
        assert_eq!(t.accent, "\x1b[38;2;17;34;51m");
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn try_load_distinguishes_presets_from_explicit_path_and_parse_errors() {
        // Built-in named presets succeed without an explicit file unless overridden on disk
        assert!(Theme::try_load("dark").is_ok());
        assert!(Theme::try_load("light").is_ok());

        // Missing explicit path returns Err
        let missing = std::env::temp_dir().join(format!("mehh-missing-theme-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let err = Theme::try_load(missing.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Failed to read theme"), "{err}");

        // Invalid JSON at explicit path returns Err
        let bad_json = std::env::temp_dir().join(format!("mehh-bad-theme-{}.json", std::process::id()));
        std::fs::write(&bad_json, "{not valid json").unwrap();
        let parse_err = Theme::try_load(bad_json.to_str().unwrap()).unwrap_err();
        assert!(parse_err.contains("Failed to parse theme"), "{parse_err}");
        let _ = std::fs::remove_file(bad_json);

        // Unknown non-path name returns Err
        assert!(Theme::try_load("definitely_missing_preset_98765").is_err());
    }

    #[test]
    fn resolve_color_value_rejects_non_sgr_and_arbitrary_strings() {
        assert_eq!(resolve_color_value("\x1b[38;2;10;20;30m", false), "\x1b[38;2;10;20;30m");
        assert_eq!(resolve_color_value("\x1b[1m\x1b[35m", false), "\x1b[1m\x1b[35m");
        assert_eq!(resolve_color_value("\x1b[2J", false), "");
        assert_eq!(resolve_color_value("\x1b]0;pwned\x07", false), "");
        assert_eq!(resolve_color_value("arbitrary-text", false), "");
    }

    #[test]
    fn try_load_ignores_bare_cwd_file_but_allows_explicit_relative_path() {
        static CWD_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        struct CwdGuard {
            orig: PathBuf,
            dir: PathBuf,
        }
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.orig);
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        let _lock = CWD_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let orig = std::env::current_dir().unwrap();
        let dir = std::env::temp_dir().join(format!("mehh-theme-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dark"),
            r##"{"name":"shadowed","accent":"#112233"}"##,
        )
        .unwrap();

        let _guard = CwdGuard {
            orig,
            dir: dir.clone(),
        };
        std::env::set_current_dir(&dir).unwrap();

        let bare = Theme::try_load("dark").expect("bare dark preset should load");
        assert_ne!(bare.name, "shadowed");
        assert_ne!(bare.accent, "\x1b[38;2;17;34;51m");

        let explicit = Theme::try_load("./dark").expect("explicit ./dark path should load");
        assert_eq!(explicit.name, "shadowed");
        assert_eq!(explicit.accent, "\x1b[38;2;17;34;51m");
    }

    #[test]
    fn try_load_sanitizes_theme_name_control_and_escape_sequences() {
        let p1 = std::env::temp_dir().join(format!("mehh-theme-name-1-{}.json", std::process::id()));
        std::fs::write(
            &p1,
            "{\"name\":\"Evil\\u001b]0;pwn\\u0007\\u001b[31mTheme\\u202e\",\"accent\":\"#112233\"}",
        )
        .unwrap();
        let t1 = Theme::try_load(p1.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p1);
        assert_eq!(t1.name, "EvilTheme");

        let p2 = std::env::temp_dir().join(format!("mehh-theme-name-2-{}.json", std::process::id()));
        std::fs::write(
            &p2,
            "{\"name\":\"\\u001b]0;only-osc\\u0007\",\"accent\":\"#112233\"}",
        )
        .unwrap();
        let t2 = Theme::try_load(p2.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p2);
        assert!(!t2.name.trim().is_empty());
        assert!(!t2.name.contains("only-osc"));
    }
}
