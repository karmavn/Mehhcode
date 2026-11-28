use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Map a crossterm `KeyEvent` into a canonical lowercase key identifier string.
/// E.g. `"ctrl+c"`, `"shift+enter"`, `"alt+left"`, `"enter"`, `"escape"`,
/// `"backspace"`, `"delete"`, `"tab"`, `"backtab"`, `"up"`, `"down"`,
/// `"left"`, `"right"`, `"home"`, `"end"`, `"pageup"`, `"pagedown"`.
pub fn normalize_crossterm_key(event: KeyEvent) -> Option<String> {
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let has_ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = event.modifiers.contains(KeyModifiers::ALT);
    let has_shift = event.modifiers.contains(KeyModifiers::SHIFT);
    let has_super = event.modifiers.contains(KeyModifiers::SUPER);

    let make_mod_str = |base: &str, include_shift: bool| -> String {
        let mut parts = Vec::new();
        if has_ctrl {
            parts.push("ctrl");
        }
        if has_alt {
            parts.push("alt");
        }
        if has_shift && include_shift {
            parts.push("shift");
        }
        if has_super {
            parts.push("super");
        }
        parts.push(base);
        parts.join("+")
    };

    match event.code {
        KeyCode::Char(c) => {
            if cfg!(windows)
                && has_ctrl
                && has_alt
                && !has_super
                && !c.is_control()
                && !c.is_ascii_alphabetic()
                && c != ' '
            {
                Some(c.to_string())
            } else if has_ctrl || has_alt || has_super {
                if c == ' ' {
                    Some(make_mod_str("space", has_shift))
                } else {
                    let lower = c.to_ascii_lowercase().to_string();
                    Some(make_mod_str(&lower, has_shift || c.is_ascii_uppercase()))
                }
            } else {
                Some(c.to_string())
            }
        }
        KeyCode::Enter => Some(make_mod_str("enter", true)),
        KeyCode::Tab => {
            if has_shift {
                Some("backtab".to_string())
            } else {
                Some(make_mod_str("tab", false))
            }
        }
        KeyCode::BackTab => Some("backtab".to_string()),
        KeyCode::Esc => Some(make_mod_str("escape", true)),
        KeyCode::Backspace => Some(make_mod_str("backspace", true)),
        KeyCode::Delete => Some(make_mod_str("delete", true)),
        KeyCode::Left => Some(make_mod_str("left", true)),
        KeyCode::Right => Some(make_mod_str("right", true)),
        KeyCode::Up => Some(make_mod_str("up", true)),
        KeyCode::Down => Some(make_mod_str("down", true)),
        KeyCode::Home => Some(make_mod_str("home", true)),
        KeyCode::End => Some(make_mod_str("end", true)),
        KeyCode::PageUp => Some(make_mod_str("pageup", true)),
        KeyCode::PageDown => Some(make_mod_str("pagedown", true)),
        KeyCode::F(n) => {
            let f = format!("f{n}");
            Some(make_mod_str(&f, true))
        }
        _ => None,
    }
}

const MOD_CTRL: u8 = 1 << 0;
const MOD_ALT: u8 = 1 << 1;
const MOD_SHIFT: u8 = 1 << 2;
const MOD_SUPER: u8 = 1 << 3;

fn canonicalize_key_spec(spec: &str) -> (u8, Option<BTreeSet<String>>, Cow<'_, str>) {
    if spec == " " {
        return (0, None, Cow::Borrowed(" "));
    }
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return (0, None, Cow::Borrowed(""));
    }

    let (mod_prefix, raw_base) = if trimmed == "+" {
        ("", "+")
    } else if let Some(prefix) = trimmed.strip_suffix("++") {
        (prefix, "+")
    } else if let Some(idx) = trimmed.rfind('+') {
        (&trimmed[..idx], &trimmed[idx + 1..])
    } else {
        ("", trimmed)
    };

    let mut mod_mask = 0u8;
    let mut extra_mods: Option<BTreeSet<String>> = None;
    if !mod_prefix.is_empty() {
        for m in mod_prefix.split('+') {
            if m.is_empty() {
                continue;
            }
            if m.eq_ignore_ascii_case("ctrl") || m.eq_ignore_ascii_case("control") {
                mod_mask |= MOD_CTRL;
            } else if m.eq_ignore_ascii_case("alt")
                || m.eq_ignore_ascii_case("opt")
                || m.eq_ignore_ascii_case("option")
            {
                mod_mask |= MOD_ALT;
            } else if m.eq_ignore_ascii_case("shift") {
                mod_mask |= MOD_SHIFT;
            } else if m.eq_ignore_ascii_case("super")
                || m.eq_ignore_ascii_case("cmd")
                || m.eq_ignore_ascii_case("command")
                || m.eq_ignore_ascii_case("win")
                || m.eq_ignore_ascii_case("meta")
            {
                mod_mask |= MOD_SUPER;
            } else {
                extra_mods
                    .get_or_insert_with(BTreeSet::new)
                    .insert(m.to_ascii_lowercase());
            }
        }
    }

    let base = if raw_base.eq_ignore_ascii_case("esc") {
        Cow::Borrowed("escape")
    } else if raw_base.eq_ignore_ascii_case("return") {
        Cow::Borrowed("enter")
    } else if raw_base.eq_ignore_ascii_case("space") {
        Cow::Borrowed(" ")
    } else if raw_base.eq_ignore_ascii_case("plus") {
        Cow::Borrowed("+")
    } else if raw_base.eq_ignore_ascii_case("backtab") {
        mod_mask |= MOD_SHIFT;
        Cow::Borrowed("tab")
    } else if raw_base.eq_ignore_ascii_case("page_up") {
        Cow::Borrowed("pageup")
    } else if raw_base.eq_ignore_ascii_case("page_down") {
        Cow::Borrowed("pagedown")
    } else if mod_mask != 0 || extra_mods.is_some() {
        if raw_base.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(raw_base.to_ascii_lowercase())
        } else {
            Cow::Borrowed(raw_base)
        }
    } else {
        Cow::Borrowed(raw_base)
    };

    (mod_mask, extra_mods, base)
}

/// Check if a key string matches a binding pattern, accounting for aliases
/// and modifier ordering.
pub fn matches_key(key: &str, pattern: &str) -> bool {
    if key.eq_ignore_ascii_case(pattern) {
        return true;
    }

    let (k_mask, k_extra, k_base) = canonicalize_key_spec(key);
    let (p_mask, p_extra, p_base) = canonicalize_key_spec(pattern);

    k_mask == p_mask && k_extra == p_extra && k_base == p_base
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TuiAction {
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    Submit,
    Newline,
    Backspace,
    Delete,
    DeleteWordBackward,
    DeleteWordForward,
    KillToEnd,
    KillToStart,
    Yank,
    YankPop,
    Undo,
    HistoryPrev,
    HistoryNext,
    TabComplete,
    Interrupt,
    ClearOrExit,
    ScrollUp,
    ScrollDown,
    ScrollBottom,
}

const ALL_TUI_ACTIONS: [TuiAction; 27] = [
    TuiAction::CursorLeft,
    TuiAction::CursorRight,
    TuiAction::CursorUp,
    TuiAction::CursorDown,
    TuiAction::WordLeft,
    TuiAction::WordRight,
    TuiAction::LineStart,
    TuiAction::LineEnd,
    TuiAction::Submit,
    TuiAction::Newline,
    TuiAction::Backspace,
    TuiAction::Delete,
    TuiAction::DeleteWordBackward,
    TuiAction::DeleteWordForward,
    TuiAction::KillToEnd,
    TuiAction::KillToStart,
    TuiAction::Yank,
    TuiAction::YankPop,
    TuiAction::Undo,
    TuiAction::HistoryPrev,
    TuiAction::HistoryNext,
    TuiAction::TabComplete,
    TuiAction::Interrupt,
    TuiAction::ClearOrExit,
    TuiAction::ScrollUp,
    TuiAction::ScrollDown,
    TuiAction::ScrollBottom,
];

const _: () = assert!((TuiAction::ScrollBottom as u32) < 32);

impl TuiAction {
    #[inline]
    const fn bit(self) -> u32 {
        1u32 << (self as u32)
    }
}

#[derive(Debug, Clone)]
struct CompiledBinding {
    raw: String,
    mod_mask: u8,
    extra_mods: Option<BTreeSet<String>>,
    base: String,
}

impl CompiledBinding {
    fn from_raw(raw: String) -> Self {
        let (mod_mask, extra_mods, base) = canonicalize_key_spec(&raw);
        let base = base.into_owned();
        Self {
            raw,
            mod_mask,
            extra_mods,
            base,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Keybindings {
    bindings: HashMap<TuiAction, Vec<String>>,
    compiled: HashMap<TuiAction, Vec<CompiledBinding>>,
    ascii_fast: [u32; 128],
    exact_fast: HashMap<String, u32>,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::default_bindings()
    }
}

impl Keybindings {
    pub fn default_bindings() -> Self {
        let mut map: HashMap<TuiAction, Vec<String>> = HashMap::new();
        map.insert(
            TuiAction::CursorLeft,
            vec!["left".into(), "ctrl+b".into()],
        );
        map.insert(
            TuiAction::CursorRight,
            vec!["right".into(), "ctrl+f".into()],
        );
        map.insert(TuiAction::CursorUp, vec!["up".into()]);
        map.insert(TuiAction::CursorDown, vec!["down".into()]);
        map.insert(
            TuiAction::WordLeft,
            vec!["alt+left".into(), "ctrl+left".into(), "alt+b".into()],
        );
        map.insert(
            TuiAction::WordRight,
            vec!["alt+right".into(), "ctrl+right".into(), "alt+f".into()],
        );
        map.insert(
            TuiAction::LineStart,
            vec!["home".into(), "ctrl+a".into()],
        );
        map.insert(
            TuiAction::LineEnd,
            vec!["end".into(), "ctrl+e".into()],
        );
        map.insert(TuiAction::Submit, vec!["enter".into()]);
        map.insert(
            TuiAction::Newline,
            vec!["shift+enter".into(), "ctrl+j".into(), "alt+enter".into()],
        );
        map.insert(TuiAction::Backspace, vec!["backspace".into()]);
        map.insert(
            TuiAction::Delete,
            vec!["delete".into(), "ctrl+d".into()],
        );
        map.insert(
            TuiAction::DeleteWordBackward,
            vec!["ctrl+w".into(), "alt+backspace".into()],
        );
        map.insert(
            TuiAction::DeleteWordForward,
            vec!["alt+d".into(), "alt+delete".into()],
        );
        map.insert(TuiAction::KillToEnd, vec!["ctrl+k".into()]);
        map.insert(TuiAction::KillToStart, vec!["ctrl+u".into()]);
        map.insert(TuiAction::Yank, vec!["ctrl+y".into()]);
        map.insert(TuiAction::YankPop, vec!["alt+y".into()]);
        map.insert(
            TuiAction::Undo,
            vec!["ctrl+z".into(), "ctrl+-".into()],
        );
        map.insert(
            TuiAction::HistoryPrev,
            vec!["up".into(), "alt+p".into(), "ctrl+p".into()],
        );
        map.insert(
            TuiAction::HistoryNext,
            vec!["down".into(), "alt+n".into(), "ctrl+n".into()],
        );
        map.insert(TuiAction::TabComplete, vec!["tab".into()]);
        map.insert(
            TuiAction::Interrupt,
            vec!["escape".into(), "ctrl+c".into()],
        );
        map.insert(
            TuiAction::ClearOrExit,
            vec!["ctrl+c".into(), "ctrl+d".into()],
        );
        map.insert(
            TuiAction::ScrollUp,
            vec!["pageup".into(), "shift+up".into()],
        );
        map.insert(
            TuiAction::ScrollDown,
            vec!["pagedown".into(), "shift+down".into()],
        );
        map.insert(
            TuiAction::ScrollBottom,
            vec!["ctrl+end".into(), "shift+end".into()],
        );

        let compiled = map
            .iter()
            .map(|(&action, patterns)| {
                (
                    action,
                    patterns
                        .iter()
                        .map(|p| CompiledBinding::from_raw(p.clone()))
                        .collect(),
                )
            })
            .collect();

        let mut kb = Self {
            bindings: map,
            compiled,
            ascii_fast: [0u32; 128],
            exact_fast: HashMap::new(),
        };
        kb.rebuild_fast_cache();
        kb
    }

    fn matches_slow(&self, key: &str, action: TuiAction) -> bool {
        let Some(entries) = self.compiled.get(&action) else {
            return false;
        };
        if entries.iter().any(|b| key.eq_ignore_ascii_case(&b.raw)) {
            return true;
        }
        let (k_mask, k_extra, k_base) = canonicalize_key_spec(key);
        entries
            .iter()
            .any(|b| k_mask == b.mod_mask && k_extra == b.extra_mods && k_base == b.base)
    }

    fn compute_mask_slow(&self, key: &str) -> u32 {
        let (k_mask, k_extra, k_base) = canonicalize_key_spec(key);
        let mut mask = 0u32;
        for action in ALL_TUI_ACTIONS {
            let Some(entries) = self.compiled.get(&action) else {
                continue;
            };
            let hit = entries.iter().any(|b| {
                key.eq_ignore_ascii_case(&b.raw)
                    || (k_mask == b.mod_mask && k_extra == b.extra_mods && k_base == b.base)
            });
            if hit {
                mask |= action.bit();
            }
        }
        mask
    }

    fn rebuild_fast_cache(&mut self) {
        for b in 0u8..128 {
            let buf = [b];
            if let Ok(s) = std::str::from_utf8(&buf) {
                self.ascii_fast[b as usize] = self.compute_mask_slow(s);
            }
        }

        let mut keys_to_index: Vec<String> = Vec::with_capacity(128);
        for spec in [
            "enter", "tab", "backtab", "escape", "backspace", "delete", "left", "right", "up",
            "down", "home", "end", "pageup", "pagedown",
        ] {
            keys_to_index.push(spec.to_string());
        }
        for ch in 'a'..='z' {
            keys_to_index.push(format!("ctrl+{ch}"));
            keys_to_index.push(format!("alt+{ch}"));
        }
        for entries in self.compiled.values() {
            for b in entries {
                keys_to_index.push(b.raw.clone());
                keys_to_index.push(b.raw.to_ascii_lowercase());
                if b.extra_mods.is_none() {
                    let mut canon = String::new();
                    if (b.mod_mask & MOD_CTRL) != 0 {
                        canon.push_str("ctrl+");
                    }
                    if (b.mod_mask & MOD_ALT) != 0 {
                        canon.push_str("alt+");
                    }
                    if (b.mod_mask & MOD_SHIFT) != 0 {
                        canon.push_str("shift+");
                    }
                    if (b.mod_mask & MOD_SUPER) != 0 {
                        canon.push_str("super+");
                    }
                    if b.base == " " {
                        canon.push_str("space");
                    } else {
                        canon.push_str(&b.base);
                    }
                    keys_to_index.push(canon);
                }
            }
        }

        self.exact_fast.clear();
        for key in keys_to_index {
            if !self.exact_fast.contains_key(&key) {
                let mask = self.compute_mask_slow(&key);
                self.exact_fast.insert(key, mask);
            }
        }
    }

    #[inline]
    pub fn matches(&self, key: &str, action: TuiAction) -> bool {
        if key.len() == 1 {
            let b = key.as_bytes()[0];
            if b < 128 {
                return (self.ascii_fast[b as usize] & action.bit()) != 0;
            }
        }
        if let Some(&mask) = self.exact_fast.get(key) {
            return (mask & action.bit()) != 0;
        }
        self.matches_slow(key, action)
    }

    pub fn bind(&mut self, action: TuiAction, pattern: impl Into<String>) {
        let p = pattern.into();
        self.compiled
            .entry(action)
            .or_default()
            .push(CompiledBinding::from_raw(p.clone()));
        self.bindings.entry(action).or_default().push(p);
        self.rebuild_fast_cache();
    }

    pub fn set_bindings(&mut self, action: TuiAction, patterns: Vec<String>) {
        let compiled = patterns
            .iter()
            .map(|p| CompiledBinding::from_raw(p.clone()))
            .collect();
        self.compiled.insert(action, compiled);
        self.bindings.insert(action, patterns);
        self.rebuild_fast_cache();
    }

    pub fn get_bindings(&self, action: TuiAction) -> Option<&[String]> {
        self.bindings.get(&action).map(|v| v.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_crossterm_key_ctrl_shift_lowercase_char() {
        // When user presses Ctrl+Shift+P, crossterm may deliver KeyCode::Char('p') with CONTROL | SHIFT.
        let ev = KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            normalize_crossterm_key(ev),
            Some("ctrl+shift+p".to_string())
        );
    }

    #[test]
    fn test_canonicalize_plus_key_spec() {
        assert!(matches_key("+", "+"));
        assert!(matches_key("ctrl++", "ctrl++"));
        assert!(matches_key("ctrl+shift++", "ctrl+shift++"));
    }

    #[test]
    fn test_matches_key_space_alias() {
        assert!(matches_key(" ", "space"));
        assert!(matches_key("space", " "));
    }

    #[test]
    fn test_matches_key_case_insensitive_base_with_modifiers() {
        assert!(matches_key("shift+ctrl+P", "ctrl+shift+p"));
        assert!(matches_key("ctrl+shift+p", "shift+ctrl+P"));
    }

    #[test]
    fn test_normalize_crossterm_key_windows_altgr() {
        let ev_at = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        let ev_euro = KeyEvent::new(
            KeyCode::Char('\u{20AC}'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        let ev_alpha = KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        let ev_super = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
        );
        if cfg!(windows) {
            assert_eq!(normalize_crossterm_key(ev_at), Some("@".to_string()));
            assert_eq!(
                normalize_crossterm_key(ev_euro),
                Some("\u{20AC}".to_string())
            );
        } else {
            assert_eq!(
                normalize_crossterm_key(ev_at),
                Some("ctrl+alt+@".to_string())
            );
        }
        assert_eq!(
            normalize_crossterm_key(ev_alpha),
            Some("ctrl+alt+a".to_string())
        );
        assert_eq!(
            normalize_crossterm_key(ev_super),
            Some("ctrl+alt+super+@".to_string())
        );
    }

    #[test]
    fn test_normalize_crossterm_key_modified_space() {
        let ev_ctrl = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(
            normalize_crossterm_key(ev_ctrl),
            Some("ctrl+space".to_string())
        );
        assert!(matches_key(
            &normalize_crossterm_key(ev_ctrl).unwrap(),
            "ctrl+space"
        ));

        let ev_alt = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::ALT);
        assert_eq!(
            normalize_crossterm_key(ev_alt),
            Some("alt+space".to_string())
        );
        assert!(matches_key(
            &normalize_crossterm_key(ev_alt).unwrap(),
            "alt+space"
        ));

        let ev_ctrl_alt =
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(
            normalize_crossterm_key(ev_ctrl_alt),
            Some("ctrl+alt+space".to_string())
        );

        let ev_plain = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(normalize_crossterm_key(ev_plain), Some(" ".to_string()));
    }

    #[test]
    fn test_keybindings_fast_cache_matches_slow_across_ascii_aliases_and_mutations() {
        let mut kb = Keybindings::default_bindings();
        let assert_parity = |bindings: &Keybindings| {
            let mut probe_keys: Vec<String> = (0u8..128)
                .filter_map(|b| std::str::from_utf8(&[b]).ok().map(str::to_string))
                .collect();
            for extra in [
                "enter",
                "return",
                "esc",
                "escape",
                "space",
                "tab",
                "backtab",
                "shift+tab",
                "ctrl+c",
                "shift+ctrl+P",
                "ctrl++",
                "ctrl+x",
                "alt+left",
                "up",
                "down",
                "page_up",
                "page_down",
                "é",
            ] {
                probe_keys.push(extra.to_string());
            }
            for key in &probe_keys {
                for action in ALL_TUI_ACTIONS {
                    assert_eq!(
                        bindings.matches(key, action),
                        bindings.matches_slow(key, action),
                        "fast cache diverged for key={key:?} action={action:?}"
                    );
                }
            }
        };

        assert_parity(&kb);
        kb.bind(TuiAction::Submit, "ctrl+x");
        kb.set_bindings(TuiAction::Interrupt, vec!["k".into(), "ctrl+shift+q".into()]);
        assert_parity(&kb);
    }
}

