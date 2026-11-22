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
            if has_ctrl || has_alt || has_super {
                let lower = c.to_ascii_lowercase().to_string();
                Some(make_mod_str(&lower, has_shift || c.is_ascii_uppercase()))
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

fn canonicalize_key_spec(spec: &str) -> (BTreeSet<String>, String) {
    if spec == " " {
        return (BTreeSet::new(), " ".to_string());
    }
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return (BTreeSet::new(), String::new());
    }

    let (raw_mods, raw_base): (Vec<&str>, &str) = if trimmed == "+" {
        (Vec::new(), "+")
    } else if let Some(prefix) = trimmed.strip_suffix("++") {
        (prefix.split('+').filter(|s| !s.is_empty()).collect(), "+")
    } else if let Some(idx) = trimmed.rfind('+') {
        let prefix = &trimmed[..idx];
        let base = &trimmed[idx + 1..];
        (prefix.split('+').filter(|s| !s.is_empty()).collect(), base)
    } else {
        (Vec::new(), trimmed)
    };

    let mut modifiers = BTreeSet::new();
    for m in raw_mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                modifiers.insert("ctrl".to_string());
            }
            "alt" | "opt" | "option" => {
                modifiers.insert("alt".to_string());
            }
            "shift" => {
                modifiers.insert("shift".to_string());
            }
            "super" | "cmd" | "command" | "win" | "meta" => {
                modifiers.insert("super".to_string());
            }
            other => {
                modifiers.insert(other.to_string());
            }
        }
    }

    let lower_base = raw_base.to_ascii_lowercase();
    let base = match lower_base.as_str() {
        "esc" => "escape",
        "return" => "enter",
        "space" => " ",
        "plus" => "+",
        "backtab" => {
            modifiers.insert("shift".to_string());
            "tab"
        }
        "page_up" => "pageup",
        "page_down" => "pagedown",
        _ => raw_base,
    };

    (modifiers, base.to_string())
}

/// Check if a key string matches a binding pattern, accounting for aliases
/// and modifier ordering.
pub fn matches_key(key: &str, pattern: &str) -> bool {
    if key.eq_ignore_ascii_case(pattern) {
        return true;
    }

    let (k_mods, k_base) = canonicalize_key_spec(key);
    let (p_mods, p_base) = canonicalize_key_spec(pattern);

    k_mods == p_mods && k_base == p_base
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

#[derive(Debug, Clone)]
pub struct Keybindings {
    bindings: HashMap<TuiAction, Vec<String>>,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::default_bindings()
    }
}

impl Keybindings {
    pub fn default_bindings() -> Self {
        let mut map = HashMap::new();
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

        Self { bindings: map }
    }

    pub fn matches(&self, key: &str, action: TuiAction) -> bool {
        if let Some(keys) = self.bindings.get(&action) {
            keys.iter().any(|pattern| matches_key(key, pattern))
        } else {
            false
        }
    }

    pub fn bind(&mut self, action: TuiAction, pattern: impl Into<String>) {
        self.bindings.entry(action).or_default().push(pattern.into());
    }

    pub fn set_bindings(&mut self, action: TuiAction, patterns: Vec<String>) {
        self.bindings.insert(action, patterns);
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
}

