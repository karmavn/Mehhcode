# Dump 03 - TUI components va keybindings

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| crates/mehh-tui/src/keys.rs | 10.2 | 327 | d455165ed35693734f640568a318b6d87e434a050f5b045cb6b1055c2ad2a9d2 |
| crates/mehh-tui/src/components/basic.rs | 17.2 | 575 | bda347b9a829a071c109d7dd4ed27cc6f6f49d73c467947498e9f5b491908c5a |
| crates/mehh-tui/src/components/settings_list.rs | 14.1 | 430 | 71a0290f988b4ec6f4c3cf3a72a3eb438c4e3476dbbe087cdce958af21538f11 |
| crates/mehh-tui/src/components/editor.rs | 47.9 | 1389 | 0f90062f25c447ed082544025f0fcca9b1cd4836b0853be4fa5d2920e9be29c2 |

## crates/mehh-tui/src/keys.rs

`````rust
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

`````

## crates/mehh-tui/src/components/basic.rs

`````rust
use crate::ansi::{truncate_to_width, wrap_text_with_ansi};
use crate::theme::Theme;
use crate::tui::Component;

/// Plain or padded text component with ANSI-safe word wrapping.
#[derive(Debug, Clone)]
pub struct Text {
    pub content: String,
    pub padding_x: usize,
}

impl Text {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            padding_x: 0,
        }
    }

    pub fn with_padding(mut self, padding_x: usize) -> Self {
        self.padding_x = padding_x;
        self
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        let pad_x = self.padding_x;
        let inner_width = width.saturating_sub(pad_x * 2);
        let wrapped = wrap_text_with_ansi(&self.content, inner_width);
        if pad_x == 0 {
            wrapped
        } else {
            let pad_str = " ".repeat(pad_x);
            wrapped
                .into_iter()
                .map(|line| format!("{pad_str}{line}{pad_str}"))
                .collect()
        }
    }
}

/// Spacer component emitting empty lines for vertical layout.
#[derive(Debug, Clone)]
pub struct Spacer {
    pub lines: usize,
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }
}

impl Component for Spacer {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}

/// Border styling options for `BorderedBox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
    Single,
    Double,
    Rounded,
    Heavy,
}

impl BorderStyle {
    pub fn chars(&self) -> (char, char, char, char, char, char) {
        match self {
            BorderStyle::Single => ('┌', '─', '┐', '│', '└', '┘'),
            BorderStyle::Double => ('╔', '═', '╗', '║', '╚', '╝'),
            BorderStyle::Rounded => ('╭', '─', '╮', '│', '╰', '╯'),
            BorderStyle::Heavy => ('┏', '━', '┓', '┃', '┗', '┛'),
        }
    }
}

/// Box container rendering border outlines around a child component.
pub struct BorderedBox {
    pub title: Option<String>,
    pub style: BorderStyle,
    pub child: Box<dyn Component>,
    pub border_color: String,
    pub title_color: String,
}

fn paint(code: &str, text: &str) -> String {
    if code.is_empty() {
        text.to_string()
    } else {
        format!("{code}{text}\x1b[0m")
    }
}

impl BorderedBox {
    pub fn new(child: Box<dyn Component>) -> Self {
        Self {
            title: None,
            style: BorderStyle::Rounded,
            child,
            border_color: String::new(),
            title_color: String::new(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_style(mut self, style: BorderStyle) -> Self {
        self.style = style;
        self
    }

    pub fn with_border_color(mut self, color: impl Into<String>) -> Self {
        self.border_color = color.into();
        self
    }

    pub fn with_title_color(mut self, color: impl Into<String>) -> Self {
        self.title_color = color.into();
        self
    }
}

impl Component for BorderedBox {
    fn render(&self, width: usize) -> Vec<String> {
        if width < 3 {
            return self.child.render(width);
        }

        let (tl, h, tr, v, bl, br) = self.style.chars();
        let inner_width = width - 2;
        let mut lines = Vec::new();

        // Top line with optional title
        let top_line = if let Some(ref title) = self.title {
            let title_clean = crate::ansi::strip_ansi(title);
            let title_w = crate::ansi::visible_width(&title_clean);
            if title_w + 4 <= inner_width {
                let right_pad = inner_width - title_w - 3;
                let painted_title = if self.title_color.is_empty() {
                    title.clone()
                } else {
                    format!("{}{}\x1b[0m{}", self.title_color, title, self.border_color)
                };
                paint(
                    &self.border_color,
                    &format!("{tl}{h} {painted_title} {}{tr}", h.to_string().repeat(right_pad)),
                )
            } else {
                paint(
                    &self.border_color,
                    &format!("{tl}{}{tr}", h.to_string().repeat(inner_width)),
                )
            }
        } else {
            paint(
                &self.border_color,
                &format!("{tl}{}{tr}", h.to_string().repeat(inner_width)),
            )
        };
        lines.push(top_line);

        // Child lines with vertical borders
        let child_lines = self.child.render(inner_width);
        for line in child_lines {
            let line_w = crate::ansi::visible_width(&line);
            let pad = inner_width.saturating_sub(line_w);
            let right_v = if self.border_color.is_empty() {
                v.to_string()
            } else {
                format!("{}{v}", self.border_color)
            };
            lines.push(paint(
                &self.border_color,
                &format!("{v}{line}{}{right_v}", " ".repeat(pad)),
            ));
        }

        // Bottom line
        lines.push(paint(
            &self.border_color,
            &format!("{bl}{}{br}", h.to_string().repeat(inner_width)),
        ));

        lines
    }

    fn handle_key(&mut self, key: &str) -> bool {
        self.child.handle_key(key)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }
}

/// A pre-rendered set of lines, used for panels whose content is already width-fitted.
#[derive(Debug, Clone)]
pub struct RawLines {
    pub lines: Vec<String>,
}

impl Component for RawLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// Animated Braille spinner frames.
pub const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated loader spinner with status message.
#[derive(Debug, Clone)]
pub struct Loader {
    pub label: String,
    pub frame: usize,
    pub theme: Theme,
}

impl Loader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            frame: 0,
            theme: Theme::default(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % BRAILLE_FRAMES.len();
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let spinner = BRAILLE_FRAMES[self.frame % BRAILLE_FRAMES.len()];
        let line = format!(
            "{} {}",
            self.theme.fg(&self.theme.accent, spinner),
            self.label
        );
        wrap_text_with_ansi(&line, width)
    }
}

/// Selectable item for `SelectList`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    #[serde(default)]
    pub label_color: Option<String>,
}

impl SelectItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            label_color: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_label_color(mut self, color: impl Into<String>) -> Self {
        self.label_color = Some(color.into());
        self
    }
}

/// Interactive scrollable selection list with filtering.
#[derive(Debug, Clone)]
pub struct SelectList {
    pub items: Vec<SelectItem>,
    pub filter: String,
    pub selected: usize,
    pub max_visible: usize,
    pub theme: Theme,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self {
            items,
            filter: String::new(),
            selected: 0,
            max_visible: max_visible.max(1),
            theme: Theme::default(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.selected = 0;
    }

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.selected = 0;
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.trim().is_empty() {
            return (0..self.items.len()).collect();
        }
        crate::fuzzy::fuzzy_filter_indexed(&self.items, &self.filter, |item| {
            if let Some(desc) = &item.description {
                format!("{} {} {}", item.label, item.id, desc)
            } else {
                format!("{} {}", item.label, item.id)
            }
        })
        .into_iter()
        .map(|(idx, _)| idx)
        .collect()
    }

    pub fn move_up(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            let cur = self.selected.min(filtered.len() - 1);
            if cur == 0 {
                self.selected = filtered.len() - 1;
            } else {
                self.selected = cur - 1;
            }
        }
    }

    pub fn move_down(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            let cur = self.selected.min(filtered.len() - 1);
            self.selected = (cur + 1) % filtered.len();
        }
    }

    pub fn page_up(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let cur = self.selected.min(filtered.len() - 1);
        self.selected = cur.saturating_sub(self.max_visible);
    }

    pub fn page_down(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let cur = self.selected.min(filtered.len() - 1);
        self.selected = (cur + self.max_visible).min(filtered.len() - 1);
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return None;
        }
        let sel = self.selected.min(filtered.len() - 1);
        Some(&self.items[filtered[sel]])
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return vec![truncate_to_width(
                &format!("  {}", self.theme.fg(&self.theme.dim, "No matching items")),
                width,
                "...",
                false,
            )];
        }

        let total = filtered.len();
        let selected = self.selected.min(total.saturating_sub(1));
        let max_vis = self.max_visible.min(total);
        let start = (selected as isize - (max_vis as isize / 2))
            .clamp(0, (total as isize - max_vis as isize).max(0)) as usize;
        let end = (start + max_vis).min(total);

        let max_label_width = filtered
            .iter()
            .map(|&idx| crate::ansi::visible_width(&self.items[idx].label))
            .max()
            .unwrap_or(12)
            .clamp(10, 36);

        let mut lines = Vec::new();
        for (vis_idx, &item_idx) in filtered[start..end].iter().enumerate() {
            let actual_idx = start + vis_idx;
            let item = &self.items[item_idx];
            let is_selected = actual_idx == selected;

            let label_len = crate::ansi::visible_width(&item.label);
            let label_pad = " ".repeat(max_label_width.saturating_sub(label_len));
            let desc = item.description.as_deref().unwrap_or("");
            let desc_len = crate::ansi::visible_width(desc);
            let used = 2 + label_len + label_pad.len() + 2 + desc_len;
            let tail_pad = " ".repeat(width.saturating_sub(used));
            let reset = "\x1b[0m";
            let bg = self.theme.selected_bg.as_str();
            let rebg = bg;
            let label_color = item.label_color.as_deref().unwrap_or(&self.theme.text);

            let line = if is_selected {
                format!(
                    "{bg}{accent}▎ {reset}{rebg}\x1b[1m{label_color}{label}{label_pad}  {reset}{rebg}{dim}{desc}{tail_pad}{reset}",
                    accent = self.theme.accent,
                    label_color = label_color,
                    label = item.label,
                    dim = self.theme.dim,
                )
            } else {
                format!(
                    "  {label_color}{label}{label_pad}  {reset}{dim}{desc}{tail_pad}{reset}",
                    label_color = label_color,
                    label = item.label,
                    dim = self.theme.dim,
                )
            };

            lines.push(truncate_to_width(&line, width, "...", false));
        }

        lines
    }

    fn handle_key(&mut self, key: &str) -> bool {
        match key {
            "up" => {
                self.move_up();
                true
            }
            "down" => {
                self.move_down();
                true
            }
            "pageup" => {
                self.page_up();
                true
            }
            "pagedown" => {
                self.page_down();
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bordered_box_uses_theme_colors_without_changing_width() {
        let boxed = BorderedBox::new(Box::new(Text::new("hi")))
            .with_border_color("\x1b[38;2;1;2;3m")
            .with_title_color("\x1b[38;2;4;5;6m")
            .with_title("T");
        let lines = boxed.render(20);
        let joined = lines.join("\n");
        assert!(joined.contains("\x1b[38;2;1;2;3m"));
        assert!(joined.contains("\x1b[38;2;4;5;6m"));
        assert!(lines.iter().all(|l| crate::ansi::visible_width(l) == 20), "{lines:?}");
    }

    #[test]
    fn select_list_selected_row_uses_selected_bg_and_full_width() {
        let mut list = SelectList::new(
            vec![SelectItem::new("a", "Alpha").with_description("first")],
            5,
        );
        list.theme.selected_bg = "\x1b[48;2;9;9;9m".to_string();
        let lines = list.render(30);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\x1b[48;2;9;9;9m"), "{:?}", lines[0]);
        assert!(lines[0].contains('▎'), "{:?}", lines[0]);
        assert_eq!(crate::ansi::visible_width(&lines[0]), 30);
    }

    #[test]
    fn raw_lines_returns_input_verbatim() {
        let raw = RawLines {
            lines: vec!["a".into(), "b".into()],
        };
        assert_eq!(raw.render(10), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_select_list_out_of_bounds_highlights_clamped_item() {
        let items = vec![
            SelectItem::new("1", "Apple"),
            SelectItem::new("2", "Banana"),
        ];
        let mut list = SelectList::new(items, 5);
        list.selected = 10;
        let lines = list.render(30);
        assert!(lines[1].contains('▎'));
        assert!(lines[1].contains("Banana"));
        assert_eq!(
            list.selected_item().map(|it| it.label.as_str()),
            Some("Banana")
        );
    }

    #[test]
    fn test_select_list_viewport_centering() {
        let items: Vec<SelectItem> = (0..10)
            .map(|i| SelectItem::new(i.to_string(), format!("Item {i}")))
            .collect();
        let mut list = SelectList::new(items, 5);
        list.selected = 3;
        let lines = list.render(30);
        assert_eq!(lines.len(), 5);
        // Centered window for selected=3 with max_visible=5 should start at 1 (3 - 5/2 = 1) -> Items 1, 2, 3, 4, 5
        assert!(lines[0].contains("Item 1"));
        assert!(lines[2].contains("Item 3"));
        assert!(lines[2].contains('▎'));
        assert!(lines[4].contains("Item 5"));
    }

    #[test]
    fn test_select_list_page_up_page_down() {
        let items: Vec<SelectItem> = (0..12)
            .map(|i| SelectItem::new(i.to_string(), format!("Item {i}")))
            .collect();
        let mut list = SelectList::new(items, 5);
        assert_eq!(list.selected, 0);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 5);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 10);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 11); // clamped to total - 1

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 6);

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 1);

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 0); // clamped to 0
    }
}
`````

## crates/mehh-tui/src/components/settings_list.rs

`````rust
//! Two-column settings list with description pane and value cycling,
//! ported from TypeScript `packages/tui/src/components/settings-list.ts`.

use crate::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::fuzzy::fuzzy_filter_indexed;
use crate::theme::Theme;
use crate::tui::Component;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
}

impl SettingItem {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: Vec::new(),
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_values(mut self, values: Vec<String>) -> Self {
        self.values = values;
        self
    }
}

/// Two-column interactive settings list.
#[derive(Debug, Clone)]
pub struct SettingsList {
    pub items: Vec<SettingItem>,
    pub selected: usize,
    pub max_visible: usize,
    pub filter: String,
    pub theme: Theme,
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize) -> Self {
        Self {
            items,
            selected: 0,
            max_visible: max_visible.max(1),
            filter: String::new(),
            theme: Theme::default(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn set_items(&mut self, items: Vec<SettingItem>) {
        self.items = items;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
    }

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.selected = 0;
    }

    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|it| it.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.trim().is_empty() {
            return (0..self.items.len()).collect();
        }
        fuzzy_filter_indexed(&self.items, &self.filter, |item| {
            if let Some(desc) = &item.description {
                format!("{} {} {} {}", item.label, item.id, item.current_value, desc)
            } else {
                format!("{} {} {}", item.label, item.id, item.current_value)
            }
        })
        .into_iter()
        .map(|(idx, _)| idx)
        .collect()
    }

    pub fn move_up(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            let cur = self.selected.min(filtered.len() - 1);
            if cur == 0 {
                self.selected = filtered.len() - 1;
            } else {
                self.selected = cur - 1;
            }
        }
    }

    pub fn move_down(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            let cur = self.selected.min(filtered.len() - 1);
            self.selected = (cur + 1) % filtered.len();
        }
    }

    pub fn page_up(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let cur = self.selected.min(filtered.len() - 1);
        self.selected = cur.saturating_sub(self.max_visible);
    }

    pub fn page_down(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let cur = self.selected.min(filtered.len() - 1);
        self.selected = (cur + self.max_visible).min(filtered.len() - 1);
    }

    pub fn selected_item(&self) -> Option<&SettingItem> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return None;
        }
        let sel = self.selected.min(filtered.len() - 1);
        Some(&self.items[filtered[sel]])
    }

    pub fn selected_item_mut(&mut self) -> Option<&mut SettingItem> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return None;
        }
        let sel = self.selected.min(filtered.len() - 1);
        let idx = filtered[sel];
        self.items.get_mut(idx)
    }

    /// Cycle the currently selected item's value to the next in `values`.
    /// Returns `Some((id, new_value))` if a cycle occurred.
    pub fn cycle_current(&mut self) -> Option<(String, String)> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return None;
        }
        let sel = self.selected.min(filtered.len() - 1);
        let idx = filtered[sel];
        let item = self.items.get_mut(idx)?;

        if item.values.is_empty() {
            return None;
        }

        let curr_idx = item
            .values
            .iter()
            .position(|v| v == &item.current_value)
            .unwrap_or(0);
        let next_idx = (curr_idx + 1) % item.values.len();
        let new_val = item.values[next_idx].clone();
        item.current_value = new_val.clone();
        Some((item.id.clone(), new_val))
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return vec![truncate_to_width(
                "  No matching settings",
                width,
                "...",
                false,
            )];
        }

        let total = filtered.len();
        let selected = self.selected.min(total.saturating_sub(1));
        let max_vis = self.max_visible.min(total);
        let start = (selected as isize - (max_vis as isize / 2))
            .clamp(0, (total as isize - max_vis as isize).max(0)) as usize;
        let end = (start + max_vis).min(total);

        let max_label_width = self
            .items
            .iter()
            .map(|it| visible_width(&it.label))
            .max()
            .unwrap_or(12)
            .clamp(10, 36);

        let mut lines = Vec::new();
        for (vis_idx, &item_idx) in filtered[start..end].iter().enumerate() {
            let actual_idx = start + vis_idx;
            let item = &self.items[item_idx];
            let is_selected = actual_idx == selected;

            let label_len = visible_width(&item.label);
            let pad = " ".repeat(max_label_width.saturating_sub(label_len));
            let value_len = visible_width(&item.current_value);
            let used = 2 + label_len + pad.len() + 2 + value_len;
            let tail_pad = " ".repeat(width.saturating_sub(used));
            let reset = "\x1b[0m";
            let bg = self.theme.selected_bg.as_str();
            let rebg = bg;

            let row = if is_selected {
                format!(
                    "{bg}{accent}▎ {reset}{rebg}\x1b[1m{label}{pad}  {reset}{rebg}{value_color}{value}{tail_pad}{reset}",
                    accent = self.theme.accent,
                    label = item.label,
                    value_color = self.theme.accent,
                    value = item.current_value,
                )
            } else {
                format!(
                    "  {label}{pad}  {reset}{dim}{value}{tail_pad}{reset}",
                    label = item.label,
                    dim = self.theme.dim,
                    value = item.current_value,
                )
            };
            lines.push(truncate_to_width(&row, width, "...", false));
        }

        if total > max_vis {
            lines.push(truncate_to_width(
                &format!(
                    "  {}",
                    self.theme
                        .fg(&self.theme.dim, &format!("({}/{})", selected + 1, total))
                ),
                width,
                "...",
                false,
            ));
        }

        let selected_item = self.selected_item();
        if let Some(desc) = selected_item.and_then(|it| it.description.as_deref()) {
            lines.push(String::new());
            let wrapped = wrap_text_with_ansi(desc, width.saturating_sub(4).max(10));
            for w in wrapped {
                lines.push(truncate_to_width(
                    &format!("  {}", self.theme.fg(&self.theme.dim, &w)),
                    width,
                    "...",
                    false,
                ));
            }
        }

        lines.push(String::new());
        lines.push(truncate_to_width(
            &format!(
                "  {}",
                self.theme
                    .fg(&self.theme.dim, "Enter/Space to change · Esc to close")
            ),
            width,
            "...",
            false,
        ));

        lines
    }

    fn handle_key(&mut self, key: &str) -> bool {
        match key {
            "up" => {
                self.move_up();
                true
            }
            "down" => {
                self.move_down();
                true
            }
            "pageup" => {
                self.page_up();
                true
            }
            "pagedown" => {
                self.page_down();
                true
            }
            "enter" | " " | "return" => self.cycle_current().is_some(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_list_selected_row_uses_selected_bg_and_full_width() {
        let items = vec![SettingItem::new("x", "Label", "value").with_description("desc")];
        let mut list = SettingsList::new(items, 5);
        list.theme.selected_bg = "\x1b[48;2;9;9;9m".to_string();
        let lines = list.render(30);
        assert!(lines[0].contains("\x1b[48;2;9;9;9m"), "{:?}", lines[0]);
        assert!(lines[0].contains('▎'), "{:?}", lines[0]);
        assert_eq!(crate::ansi::visible_width(&lines[0]), 30);
    }

    #[test]
    fn test_settings_list_render_and_cycle() {
        let items = vec![
            SettingItem::new("theme", "Color theme", "dark")
                .with_values(vec!["dark".into(), "light".into()])
                .with_description("Active color theme for the TUI"),
            SettingItem::new("mode", "Queue mode", "none").with_values(vec![
                "none".into(),
                "all".into(),
                "one_at_a_time".into(),
            ]),
        ];
        let mut list = SettingsList::new(items, 5);

        let lines = list.render(60);
        assert!(lines[0].contains('▎'));
        assert!(lines[0].contains("Color theme"));
        assert!(lines[0].contains("dark"));
        assert!(lines
            .iter()
            .any(|l| l.contains("Active color theme for the TUI")));

        // Cycle theme
        let cycled = list.cycle_current();
        assert_eq!(cycled, Some(("theme".to_string(), "light".to_string())));
        assert_eq!(list.items[0].current_value, "light");

        // Move down and cycle queue mode
        list.move_down();
        assert_eq!(list.selected, 1);
        let cycled2 = list.cycle_current();
        assert_eq!(cycled2, Some(("mode".to_string(), "all".to_string())));
        assert_eq!(list.items[1].current_value, "all");
    }

    #[test]
    fn test_settings_list_fuzzy_filter() {
        let items = vec![
            SettingItem::new("theme", "Color theme", "dark"),
            SettingItem::new("steering", "Steering mode", "none"),
        ];
        let mut list = SettingsList::new(items, 5);
        list.set_filter("steer");
        assert_eq!(list.filtered_indices(), vec![1]);
        assert_eq!(list.selected_item().unwrap().id, "steering");
    }

    #[test]
    fn test_settings_list_out_of_bounds_selected_render_does_not_panic() {
        let items = vec![SettingItem::new("theme", "Color theme", "dark")
            .with_values(vec!["dark".into(), "light".into()])];
        let mut list = SettingsList::new(items, 5);
        list.selected = 100;
        let lines = list.render(60);
        assert!(!lines.is_empty());
        assert!(lines[0].contains('▎'));
        assert!(lines[0].contains("Color theme"));
        assert_eq!(list.selected_item().map(|it| it.id.as_str()), Some("theme"));
        let cycled = list.cycle_current();
        assert_eq!(cycled, Some(("theme".to_string(), "light".to_string())));
    }

    #[test]
    fn test_settings_list_viewport_centering() {
        let items: Vec<SettingItem> = (0..10)
            .map(|i| SettingItem::new(format!("id_{i}"), format!("Setting {i}"), "val"))
            .collect();
        let mut list = SettingsList::new(items, 5);
        list.selected = 3;
        let lines = list.render(60);
        // Centered window for selected=3 with max_visible=5 should start at Setting 1
        assert!(lines[0].contains("Setting 1"));
        assert!(lines[2].contains("Setting 3"));
        assert!(lines[2].contains('▎'));
    }

    #[test]
    fn test_settings_list_page_up_page_down() {
        let items: Vec<SettingItem> = (0..12)
            .map(|i| SettingItem::new(format!("id_{i}"), format!("Setting {i}"), "val"))
            .collect();
        let mut list = SettingsList::new(items, 5);
        assert_eq!(list.selected, 0);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 5);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 10);

        assert!(list.handle_key("pagedown"));
        assert_eq!(list.selected, 11);

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 6);

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 1);

        assert!(list.handle_key("pageup"));
        assert_eq!(list.selected, 0);
    }
}
`````

## crates/mehh-tui/src/components/editor.rs

`````rust
use std::collections::HashMap;

use crate::ansi::wrap_text_with_ansi;
use crate::components::basic::{SelectItem, SelectList};
use crate::keys::{Keybindings, TuiAction};
use crate::theme::Theme;
use crate::tui::{Component, CURSOR_MARKER};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorOutput {
    None,
    Submit(String),
    Changed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorLastAction {
    Kill,
    Yank,
    Other,
}

/// Ring buffer for Emacs-style kill/yank operations (max 60 entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillRing {
    ring: Vec<String>,
    max_entries: usize,
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new(60)
    }
}

impl KillRing {
    pub fn new(max_entries: usize) -> Self {
        Self {
            ring: Vec::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }

        if accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap();
            let merged = if prepend {
                format!("{text}{last}")
            } else {
                format!("{last}{text}")
            };
            self.ring.push(merged);
        } else {
            self.ring.push(text.to_string());
            if self.ring.len() > self.max_entries {
                self.ring.remove(0);
            }
        }
    }

    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(|s| s.as_str())
    }

    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().unwrap();
            self.ring.insert(0, last);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

pub fn is_punctuation_char(c: char) -> bool {
    matches!(
        c,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteMarkerSpan {
    pub start_col: usize,
    pub end_col: usize,
}

pub fn find_paste_markers(line: &str) -> Vec<PasteMarkerSpan> {
    let mut markers = Vec::new();
    let s = line.to_string();
    let mut offset = 0;
    while let Some(start_byte) = s[offset..].find("[paste #") {
        let abs_start_byte = offset + start_byte;
        if let Some(end_bracket) = s[abs_start_byte..].find(']') {
            let abs_end_byte = abs_start_byte + end_bracket + 1;
            let after_prefix = &s[abs_start_byte + 8..abs_end_byte - 1];
            let id_len = after_prefix
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if id_len > 0 {
                let start_col = s[..abs_start_byte].chars().count();
                let marker_len = s[abs_start_byte..abs_end_byte].chars().count();
                markers.push(PasteMarkerSpan {
                    start_col,
                    end_col: start_col + marker_len,
                });
            }
            offset = abs_end_byte;
        } else {
            break;
        }
    }
    markers
}

pub fn find_word_backward(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let chars: Vec<char> = text.chars().collect();
    if cursor > chars.len() {
        return chars.len();
    }
    let markers = find_paste_markers(text);

    // If cursor is inside or at end of an atomic paste marker, jump to start
    for m in &markers {
        if cursor > m.start_col && cursor <= m.end_col {
            return m.start_col;
        }
    }

    let mut pos = cursor;

    // Step 1: Skip trailing whitespace
    while pos > 0 {
        if markers.iter().any(|m| m.end_col == pos) {
            break;
        }
        if chars[pos - 1].is_whitespace() {
            pos -= 1;
        } else {
            break;
        }
    }

    if pos == 0 {
        return 0;
    }

    // Check if we hit an atomic marker boundary
    for m in &markers {
        if pos > m.start_col && pos <= m.end_col {
            return m.start_col;
        }
    }

    // Step 2: Skip punctuation run or word-like run
    if is_punctuation_char(chars[pos - 1]) {
        while pos > 0 {
            if markers.iter().any(|m| m.end_col == pos) {
                break;
            }
            if is_punctuation_char(chars[pos - 1]) {
                pos -= 1;
            } else {
                break;
            }
        }
    } else {
        while pos > 0 {
            if markers.iter().any(|m| m.end_col == pos) {
                break;
            }
            if !chars[pos - 1].is_whitespace() && !is_punctuation_char(chars[pos - 1]) {
                pos -= 1;
            } else {
                break;
            }
        }
    }

    pos
}

pub fn find_word_forward(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if cursor >= len {
        return len;
    }
    let markers = find_paste_markers(text);

    // If cursor is inside or at start of an atomic paste marker, jump to end
    for m in &markers {
        if cursor >= m.start_col && cursor < m.end_col {
            return m.end_col;
        }
    }

    let mut pos = cursor;

    // Step 1: Skip leading whitespace
    while pos < len {
        if markers.iter().any(|m| m.start_col == pos) {
            break;
        }
        if chars[pos].is_whitespace() {
            pos += 1;
        } else {
            break;
        }
    }

    if pos >= len {
        return len;
    }

    // Check if we hit an atomic marker boundary
    for m in &markers {
        if pos >= m.start_col && pos < m.end_col {
            return m.end_col;
        }
    }

    // Step 2: Skip punctuation run or word-like run
    if is_punctuation_char(chars[pos]) {
        while pos < len {
            if markers.iter().any(|m| m.start_col == pos) {
                break;
            }
            if is_punctuation_char(chars[pos]) {
                pos += 1;
            } else {
                break;
            }
        }
    } else {
        while pos < len {
            if markers.iter().any(|m| m.start_col == pos) {
                break;
            }
            if !chars[pos].is_whitespace() && !is_punctuation_char(chars[pos]) {
                pos += 1;
            } else {
                break;
            }
        }
    }

    pos
}

fn split_char_at(s: &str, char_idx: usize) -> (String, String) {
    match s.char_indices().nth(char_idx) {
        Some((byte_idx, _)) => (s[..byte_idx].to_string(), s[byte_idx..].to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// Multiline interactive text editor supporting undo, kill ring, history,
/// paste collapsing, and autocomplete.
#[derive(Debug, Clone)]
pub struct Editor {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub undo_stack: Vec<(Vec<String>, usize, usize)>,
    pub kill_ring: KillRing,
    pub last_action: Option<EditorLastAction>,
    pub autocomplete: Option<SelectList>,
    pub autocomplete_replace_start: Option<usize>,
    pub prompt_prefix: String,
    pub placeholder: String,
    pub theme: Theme,
    pub max_lines: usize,
    pastes: HashMap<usize, String>,
    paste_counter: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            history: Vec::new(),
            history_idx: None,
            undo_stack: Vec::new(),
            kill_ring: KillRing::default(),
            last_action: None,
            autocomplete: None,
            autocomplete_replace_start: None,
            prompt_prefix: "> ".to_string(),
            placeholder: String::new(),
            theme: Theme::default(),
            max_lines: usize::MAX,
            pastes: HashMap::new(),
            paste_counter: 0,
        }
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prompt_prefix = prefix.into();
        self
    }

    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn with_max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines.max(1);
        self
    }

    /// Render only autocomplete rows; the host places them above the prompt dock.
    pub fn render_autocomplete(&self, width: usize) -> Vec<String> {
        self.autocomplete
            .as_ref()
            .map(|ac| ac.render(width))
            .unwrap_or_default()
    }

    /// Colorize collapsed paste tokens so they read as one atomic chip.
    fn colorize_paste_markers(&self, line: &str) -> String {
        if !line.contains("[paste #") {
            return line.to_string();
        }
        let mut out = String::new();
        let mut rest = line;
        while let Some(pos) = rest.find("[paste #") {
            out.push_str(&rest[..pos]);
            let tail = &rest[pos..];
            match tail.find(']') {
                Some(end) => {
                    out.push_str(&self.theme.fg(&self.theme.accent, &tail[..=end]));
                    rest = &tail[end + 1..];
                }
                None => {
                    out.push_str(tail);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() <= 1 && self.lines.first().map(|l| l.is_empty()).unwrap_or(true)
    }

    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.lines = text
            .split('\n')
            .map(|s| s.trim_end_matches('\r').to_string())
            .collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    /// Return buffer content with collapsed paste tokens expanded to their full payloads.
    pub fn text(&self) -> String {
        let raw = self.lines.join("\n");
        self.expand_pastes(&raw)
    }

    fn expand_pastes(&self, input: &str) -> String {
        if !input.contains("[paste #") || self.pastes.is_empty() {
            return input.to_string();
        }

        let mut output = String::new();
        let mut rest = input;

        while let Some(start_pos) = rest.find("[paste #") {
            output.push_str(&rest[..start_pos]);
            let after = &rest[start_pos + 8..];
            if let Some(end_bracket) = after.find(']') {
                let marker_body = &after[..end_bracket];
                // parse ID before space or end
                let id_str: String = marker_body
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(id) = id_str.parse::<usize>() {
                    if let Some(payload) = self.pastes.get(&id) {
                        output.push_str(payload);
                    } else {
                        output.push_str(&rest[start_pos..start_pos + 8 + end_bracket + 1]);
                    }
                } else {
                    output.push_str(&rest[start_pos..start_pos + 8 + end_bracket + 1]);
                }
                rest = &after[end_bracket + 1..];
            } else {
                output.push_str(&rest[start_pos..]);
                rest = "";
                break;
            }
        }
        output.push_str(rest);
        output
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pastes.clear();
        self.paste_counter = 0;
        self.history_idx = None;
        self.clear_autocomplete();
        self.last_action = None;
    }

    pub fn push_undo(&mut self) {
        self.undo_stack
            .push((self.lines.clone(), self.cursor_row, self.cursor_col));
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
    }

    pub fn undo(&mut self) -> bool {
        if let Some((lines, row, col)) = self.undo_stack.pop() {
            self.lines = lines;
            self.cursor_row = row.min(self.lines.len().saturating_sub(1));
            self.cursor_col = col.min(self.lines[self.cursor_row].chars().count());
            self.last_action = None;
            true
        } else {
            false
        }
    }

    /// Insert text at cursor, collapsing multi-line pastes > 10 lines into `[paste #N +L lines]`.
    pub fn insert_text(&mut self, text: &str) {
        self.push_undo();
        let pasted_lines: Vec<&str> = text.split('\n').map(|s| s.trim_end_matches('\r')).collect();

        if pasted_lines.len() > 10 || text.len() > 1000 {
            self.paste_counter += 1;
            let paste_id = self.paste_counter;
            self.pastes.insert(paste_id, text.to_string());
            let marker = format!("[paste #{paste_id} +{} lines]", pasted_lines.len());
            self.insert_text_internal(&marker);
        } else {
            self.insert_text_internal(text);
        }
    }

    fn insert_text_internal(&mut self, text: &str) {
        let input_lines: Vec<&str> = text.split('\n').map(|s| s.trim_end_matches('\r')).collect();

        if input_lines.len() <= 1 {
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            self.lines[self.cursor_row] = format!("{before}{text}{after}");
            self.cursor_col += text.chars().count();
        } else {
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            self.lines[self.cursor_row] = format!("{before}{}", input_lines[0]);

            let last_idx = input_lines.len() - 1;
            for line in &input_lines[1..last_idx] {
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, line.to_string());
            }

            self.cursor_row += 1;
            let last_line = input_lines[last_idx];
            self.lines
                .insert(self.cursor_row, format!("{last_line}{after}"));
            self.cursor_col = last_line.chars().count();
        }
    }

    pub fn set_autocomplete(&mut self, items: Vec<SelectItem>) {
        self.autocomplete = Some(SelectList::new(items, 5).with_theme(self.theme.clone()));
        self.autocomplete_replace_start = None;
    }

    /// Sets autocomplete items that replace `[replace_start..cursor_col]` on the active line.
    pub fn set_autocomplete_replacing(&mut self, items: Vec<SelectItem>, replace_start: usize) {
        self.autocomplete = Some(SelectList::new(items, 5).with_theme(self.theme.clone()));
        self.autocomplete_replace_start = Some(replace_start);
    }

    pub fn clear_autocomplete(&mut self) {
        self.autocomplete = None;
        self.autocomplete_replace_start = None;
    }

    fn replace_current_line_range(&mut self, start_col: usize, replacement: &str) {
        self.push_undo();
        let line = &self.lines[self.cursor_row];
        let char_len = line.chars().count();
        let start = start_col.min(self.cursor_col).min(char_len);
        let end = self.cursor_col.min(char_len);
        let (before, _) = split_char_at(line, start);
        let (_, after) = split_char_at(line, end);
        self.lines[self.cursor_row] = format!("{before}{replacement}{after}");
        self.cursor_col = start + replacement.chars().count();
    }

    fn move_word_left(&mut self) {
        self.last_action = Some(EditorLastAction::Other);
        let cur = &self.lines[self.cursor_row];
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
            }
        } else {
            self.cursor_col = find_word_backward(cur, self.cursor_col);
        }
    }

    fn move_word_right(&mut self) {
        self.last_action = Some(EditorLastAction::Other);
        let cur = &self.lines[self.cursor_row];
        let len = cur.chars().count();
        if self.cursor_col >= len {
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
        } else {
            self.cursor_col = find_word_forward(cur, self.cursor_col);
        }
    }

    fn delete_word_backward(&mut self) {
        let was_kill = self.last_action == Some(EditorLastAction::Kill);
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.push_undo();
                self.kill_ring.push("\n", true, was_kill);
                let current_line = self.lines.remove(self.cursor_row);
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
                self.lines[self.cursor_row].push_str(&current_line);
                self.last_action = Some(EditorLastAction::Kill);
            }
            return;
        }

        self.push_undo();
        let cur = &self.lines[self.cursor_row];
        let target = find_word_backward(cur, self.cursor_col);
        let (before, rest) = split_char_at(cur, target);
        let (deleted, after) = split_char_at(&rest, self.cursor_col - target);
        self.kill_ring.push(&deleted, true, was_kill);
        self.lines[self.cursor_row] = format!("{before}{after}");
        self.cursor_col = target;
        self.last_action = Some(EditorLastAction::Kill);
    }

    fn delete_word_forward(&mut self) {
        let len = self.lines[self.cursor_row].chars().count();
        let was_kill = self.last_action == Some(EditorLastAction::Kill);
        if self.cursor_col >= len {
            if self.cursor_row + 1 < self.lines.len() {
                self.push_undo();
                self.kill_ring.push("\n", false, was_kill);
                let next_line = self.lines.remove(self.cursor_row + 1);
                self.lines[self.cursor_row].push_str(&next_line);
                self.last_action = Some(EditorLastAction::Kill);
            }
            return;
        }

        self.push_undo();
        let cur = &self.lines[self.cursor_row];
        let target = find_word_forward(cur, self.cursor_col);
        let (before, rest) = split_char_at(cur, self.cursor_col);
        let (deleted, after) = split_char_at(&rest, target - self.cursor_col);
        self.kill_ring.push(&deleted, false, was_kill);
        self.lines[self.cursor_row] = format!("{before}{after}");
        self.last_action = Some(EditorLastAction::Kill);
    }

    fn kill_to_end(&mut self) {
        self.push_undo();
        let was_kill = self.last_action == Some(EditorLastAction::Kill);
        let cur = &self.lines[self.cursor_row];
        let (before, after) = split_char_at(cur, self.cursor_col);

        if !after.is_empty() {
            self.kill_ring.push(&after, false, was_kill);
            self.lines[self.cursor_row] = before;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.kill_ring.push("\n", false, was_kill);
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
        self.last_action = Some(EditorLastAction::Kill);
    }

    fn kill_to_start(&mut self) {
        self.push_undo();
        let was_kill = self.last_action == Some(EditorLastAction::Kill);
        let cur = &self.lines[self.cursor_row];
        let (before, after) = split_char_at(cur, self.cursor_col);

        if !before.is_empty() {
            self.kill_ring.push(&before, true, was_kill);
            self.lines[self.cursor_row] = after;
            self.cursor_col = 0;
        } else if self.cursor_row > 0 {
            self.kill_ring.push("\n", true, was_kill);
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current_line);
        }
        self.last_action = Some(EditorLastAction::Kill);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        let lines: Vec<&str> = text.split('\n').map(|s| s.trim_end_matches('\r')).collect();
        if lines.is_empty() {
            return;
        }

        if lines.len() == 1 {
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            self.lines[self.cursor_row] = format!("{before}{text}{after}");
            self.cursor_col += text.chars().count();
        } else {
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            self.lines[self.cursor_row] = format!("{before}{}", lines[0]);

            let last_idx = lines.len() - 1;
            for line in &lines[1..last_idx] {
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, line.to_string());
            }

            self.cursor_row += 1;
            let last_line = lines[last_idx];
            self.lines
                .insert(self.cursor_row, format!("{last_line}{after}"));
            self.cursor_col = last_line.chars().count();
        }
    }

    fn delete_yanked_text(&mut self) {
        let yanked_text = match self.kill_ring.peek() {
            Some(t) => t.to_string(),
            None => return,
        };

        let yank_lines: Vec<&str> = yanked_text
            .split('\n')
            .map(|s| s.trim_end_matches('\r'))
            .collect();
        if yank_lines.is_empty() {
            return;
        }

        if yank_lines.len() == 1 {
            let current_line = &self.lines[self.cursor_row];
            let delete_len = yanked_text.chars().count();
            if self.cursor_col >= delete_len {
                let (before, rest) = split_char_at(current_line, self.cursor_col - delete_len);
                let (_, after) = split_char_at(&rest, delete_len);
                self.lines[self.cursor_row] = format!("{before}{after}");
                self.cursor_col -= delete_len;
            }
        } else {
            let start_line = self.cursor_row.saturating_sub(yank_lines.len() - 1);
            let first_line_chars = yank_lines[0].chars().count();
            let start_line_str = &self.lines[start_line];
            let start_col = start_line_str
                .chars()
                .count()
                .saturating_sub(first_line_chars);

            let current_line_str = &self.lines[self.cursor_row];
            let (_, after_cursor) = split_char_at(current_line_str, self.cursor_col);
            let (before_yank, _) = split_char_at(&self.lines[start_line], start_col);

            let drain_count = yank_lines.len();
            let merged = format!("{before_yank}{after_cursor}");
            self.lines.splice(
                start_line..start_line + drain_count,
                std::iter::once(merged),
            );
            self.cursor_row = start_line;
            self.cursor_col = start_col;
        }
    }

    pub fn yank(&mut self) -> bool {
        if let Some(text) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.push_undo();
            self.insert_yanked_text(&text);
            self.last_action = Some(EditorLastAction::Yank);
            true
        } else {
            false
        }
    }

    pub fn yank_pop(&mut self) -> bool {
        if self.last_action != Some(EditorLastAction::Yank) || self.kill_ring.len() <= 1 {
            return false;
        }

        self.push_undo();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        if let Some(text) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.insert_yanked_text(&text);
            self.last_action = Some(EditorLastAction::Yank);
            true
        } else {
            false
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(new_idx);
        let hist_text = self.history[new_idx].clone();
        self.lines = hist_text
            .split('\n')
            .map(|s| s.trim_end_matches('\r').to_string())
            .collect();
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    fn history_next(&mut self) {
        if let Some(i) = self.history_idx {
            if i + 1 < self.history.len() {
                let new_idx = i + 1;
                self.history_idx = Some(new_idx);
                let hist_text = self.history[new_idx].clone();
                self.lines = hist_text
                    .split('\n')
                    .map(|s| s.trim_end_matches('\r').to_string())
                    .collect();
                self.cursor_row = self.lines.len() - 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
            } else {
                self.history_idx = None;
                self.clear();
            }
        }
    }

    /// Process a key event and return `EditorOutput`.
    pub fn handle_editor_key(&mut self, key: &str, bindings: &Keybindings) -> EditorOutput {
        if self.autocomplete.is_some() {
            match key {
                "up" => {
                    self.autocomplete.as_mut().unwrap().move_up();
                    return EditorOutput::None;
                }
                "down" => {
                    self.autocomplete.as_mut().unwrap().move_down();
                    return EditorOutput::None;
                }
                "pageup" => {
                    self.autocomplete.as_mut().unwrap().page_up();
                    return EditorOutput::None;
                }
                "pagedown" => {
                    self.autocomplete.as_mut().unwrap().page_down();
                    return EditorOutput::None;
                }
                "enter" | "tab" => {
                    if let Some(item) = self
                        .autocomplete
                        .as_ref()
                        .and_then(|ac| ac.selected_item().cloned())
                    {
                        let replace_start = self.autocomplete_replace_start;
                        self.clear_autocomplete();
                        if let Some(start) = replace_start {
                            self.replace_current_line_range(start, &item.id);
                        } else {
                            self.insert_text(&item.label);
                        }
                        return EditorOutput::Changed(self.text());
                    } else {
                        self.clear_autocomplete();
                    }
                }
                "escape" => {
                    self.clear_autocomplete();
                    return EditorOutput::None;
                }
                _ => {}
            }
        }

        if bindings.matches(key, TuiAction::Submit) {
            let submitted = self.text();
            if !submitted.trim().is_empty() {
                self.history.push(submitted.clone());
            }
            self.clear();
            return EditorOutput::Submit(submitted);
        }

        if bindings.matches(key, TuiAction::Newline) {
            self.push_undo();
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            self.lines[self.cursor_row] = before;
            self.lines.insert(self.cursor_row + 1, after);
            self.cursor_row += 1;
            self.cursor_col = 0;
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::CursorLeft) {
            self.last_action = Some(EditorLastAction::Other);
            if self.cursor_col > 0 {
                self.cursor_col -= 1;
            } else if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::CursorRight) {
            self.last_action = Some(EditorLastAction::Other);
            let len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col < len {
                self.cursor_col += 1;
            } else if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::CursorUp) {
            self.last_action = Some(EditorLastAction::Other);
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self
                    .cursor_col
                    .min(self.lines[self.cursor_row].chars().count());
                return EditorOutput::None;
            } else {
                self.history_prev();
                return EditorOutput::Changed(self.text());
            }
        }

        if bindings.matches(key, TuiAction::CursorDown) {
            self.last_action = Some(EditorLastAction::Other);
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = self
                    .cursor_col
                    .min(self.lines[self.cursor_row].chars().count());
                return EditorOutput::None;
            } else {
                self.history_next();
                return EditorOutput::Changed(self.text());
            }
        }

        if bindings.matches(key, TuiAction::LineStart) {
            self.last_action = Some(EditorLastAction::Other);
            self.cursor_col = 0;
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::LineEnd) {
            self.last_action = Some(EditorLastAction::Other);
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::WordLeft) {
            self.move_word_left();
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::WordRight) {
            self.move_word_right();
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::Backspace) {
            self.last_action = Some(EditorLastAction::Other);
            if self.cursor_col > 0 {
                self.push_undo();
                let line = &self.lines[self.cursor_row];
                let markers = find_paste_markers(line);
                if let Some(m) = markers.iter().find(|m| m.end_col == self.cursor_col) {
                    let (before, rest) = split_char_at(line, m.start_col);
                    let (_, after) = split_char_at(&rest, m.end_col - m.start_col);
                    self.lines[self.cursor_row] = format!("{before}{after}");
                    self.cursor_col = m.start_col;
                } else {
                    let chars: Vec<char> = line.chars().collect();
                    let mut new_chars = Vec::with_capacity(chars.len());
                    new_chars.extend_from_slice(&chars[..self.cursor_col - 1]);
                    new_chars.extend_from_slice(&chars[self.cursor_col..]);
                    self.lines[self.cursor_row] = new_chars.into_iter().collect();
                    self.cursor_col -= 1;
                }
                return EditorOutput::Changed(self.text());
            } else if self.cursor_row > 0 {
                self.push_undo();
                let current_line = self.lines.remove(self.cursor_row);
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
                self.lines[self.cursor_row].push_str(&current_line);
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::Delete) {
            self.last_action = Some(EditorLastAction::Other);
            let len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col < len {
                self.push_undo();
                let line = &self.lines[self.cursor_row];
                let markers = find_paste_markers(line);
                if let Some(m) = markers.iter().find(|m| m.start_col == self.cursor_col) {
                    let (before, rest) = split_char_at(line, m.start_col);
                    let (_, after) = split_char_at(&rest, m.end_col - m.start_col);
                    self.lines[self.cursor_row] = format!("{before}{after}");
                } else {
                    let chars: Vec<char> = line.chars().collect();
                    let mut new_chars = Vec::with_capacity(chars.len());
                    new_chars.extend_from_slice(&chars[..self.cursor_col]);
                    new_chars.extend_from_slice(&chars[self.cursor_col + 1..]);
                    self.lines[self.cursor_row] = new_chars.into_iter().collect();
                }
                return EditorOutput::Changed(self.text());
            } else if self.cursor_row + 1 < self.lines.len() {
                self.push_undo();
                let next_line = self.lines.remove(self.cursor_row + 1);
                self.lines[self.cursor_row].push_str(&next_line);
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::DeleteWordBackward) {
            self.delete_word_backward();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::DeleteWordForward) {
            self.delete_word_forward();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::KillToEnd) {
            self.kill_to_end();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::KillToStart) {
            self.kill_to_start();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::Yank) {
            if self.yank() {
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::YankPop) {
            if self.yank_pop() {
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::Undo) {
            self.last_action = Some(EditorLastAction::Other);
            if self.undo() {
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::HistoryPrev) {
            self.last_action = Some(EditorLastAction::Other);
            self.history_prev();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::HistoryNext) {
            self.last_action = Some(EditorLastAction::Other);
            self.history_next();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::Interrupt) {
            self.last_action = Some(EditorLastAction::Other);
            if !self.is_empty() {
                self.clear();
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        // Single character input
        if key.chars().count() == 1 && !key.contains('+') {
            self.last_action = Some(EditorLastAction::Other);
            self.insert_text(key);
            return EditorOutput::Changed(self.text());
        }

        EditorOutput::None
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        let mut result = Vec::new();

        if self.is_empty() && !self.placeholder.is_empty() {
            let line = format!(
                "{}{CURSOR_MARKER}{}",
                self.prompt_prefix,
                self.theme.fg(&self.theme.dim, &self.placeholder)
            );
            for w in wrap_text_with_ansi(&line, width) {
                result.push(w);
            }
        } else {
            for (row, line_str) in self.lines.iter().enumerate() {
                let pref = if row == 0 { &self.prompt_prefix } else { "  " };
                let formatted = if row == self.cursor_row {
                    let (b, a) = split_char_at(line_str, self.cursor_col);
                    format!("{pref}{b}{CURSOR_MARKER}{a}")
                } else {
                    format!("{pref}{line_str}")
                };
                let formatted = self.colorize_paste_markers(&formatted);

                for w in wrap_text_with_ansi(&formatted, width) {
                    result.push(w);
                }
            }
        }

        if self.max_lines != usize::MAX && result.len() > self.max_lines {
            let marker_row = result
                .iter()
                .position(|line| line.contains(CURSOR_MARKER))
                .unwrap_or(0);
            let start = marker_row.saturating_sub(self.max_lines / 2);
            let end = (start + self.max_lines).min(result.len());
            let start = end.saturating_sub(self.max_lines);
            result = result.into_iter().skip(start).take(end - start).collect();
        }

        result
    }

    fn handle_key(&mut self, key: &str) -> bool {
        let default_bindings = Keybindings::default_bindings();
        matches!(
            self.handle_editor_key(key, &default_bindings),
            EditorOutput::Changed(_) | EditorOutput::Submit(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_render_excludes_autocomplete() {
        let mut e = Editor::new();
        e.set_autocomplete(vec![SelectItem::new("x", "extra")]);
        let lines = e.render(40);
        assert!(lines.iter().all(|l| !l.contains("extra")), "{lines:?}");
        assert!(e.render_autocomplete(40).iter().any(|l| l.contains("extra")));
    }

    #[test]
    fn visible_window_keeps_cursor_marker_on_long_wrapped_line() {
        let mut e = Editor::new();
        e.lines = vec!["x".repeat(116)];
        e.cursor_row = 0;
        e.cursor_col = 116;
        e.max_lines = 2;
        let lines = e.render(58);
        assert!(
            lines.iter().any(|l| l.contains(CURSOR_MARKER)),
            "cursor marker clipped: {lines:?}"
        );
    }

    #[test]
    fn test_set_autocomplete_replacing_replaces_range_on_accept() {
        let mut editor = Editor::new();
        editor.set_text("check @src/ma");
        assert_eq!(editor.cursor_col, 13);

        editor.set_autocomplete_replacing(vec![SelectItem::new("@src/main.rs ", "src/main.rs")], 6);
        let kb = Keybindings::default_bindings();
        let out = editor.handle_editor_key("tab", &kb);
        assert!(matches!(out, EditorOutput::Changed(_)));
        assert_eq!(editor.text(), "check @src/main.rs ");
        assert_eq!(editor.cursor_col, 19);
        assert!(editor.autocomplete.is_none());
        assert!(editor.autocomplete_replace_start.is_none());
    }

    #[test]
    fn test_set_autocomplete_replacing_handles_multibyte_chars() {
        let mut editor = Editor::new();
        editor.set_text("xem @tệp");
        // Chars: 'x','e','m',' ','@','t','ệ','p' -> '@' at char index 4, cursor at 8
        assert_eq!(editor.cursor_col, 8);
        editor.set_autocomplete_replacing(vec![SelectItem::new("@test.rs", "test.rs")], 4);
        let kb = Keybindings::default_bindings();
        let _ = editor.handle_editor_key("enter", &kb);
        assert_eq!(editor.text(), "xem @test.rs");
    }

    #[test]
    fn test_kill_ring_push_peek_rotate() {
        let mut kr = KillRing::new(3);
        assert_eq!(kr.peek(), None);
        assert_eq!(kr.len(), 0);

        kr.push("entry1", false, false);
        kr.push("entry2", false, false);
        assert_eq!(kr.peek(), Some("entry2"));
        assert_eq!(kr.len(), 2);

        kr.rotate();
        assert_eq!(kr.peek(), Some("entry1"));

        kr.rotate();
        assert_eq!(kr.peek(), Some("entry2"));

        kr.push("entry3", false, false);
        kr.push("entry4", false, false);
        // Exceeds max_entries (3) -> entry1 was oldest and dropped
        assert_eq!(kr.len(), 3);
        assert_eq!(kr.peek(), Some("entry4"));
    }

    #[test]
    fn test_kill_ring_accumulation() {
        let mut kr = KillRing::new(10);
        kr.push("world", true, false);
        // Prepend accumulation (e.g. backward delete)
        kr.push("hello ", true, true);
        assert_eq!(kr.peek(), Some("hello world"));
        assert_eq!(kr.len(), 1);

        // Append accumulation (e.g. forward kill)
        kr.push("!", false, true);
        assert_eq!(kr.peek(), Some("hello world!"));
        assert_eq!(kr.len(), 1);
    }

    #[test]
    fn test_yank_and_yank_pop_cycle() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();

        editor.set_text("line1\nline2\nline3");
        editor.cursor_row = 0;
        editor.cursor_col = 0;

        // Kill line1 to end
        let _ = editor.handle_editor_key("ctrl+k", &kb);
        assert_eq!(editor.kill_ring.peek(), Some("line1"));

        // Move to line2 (resets last_action)
        let _ = editor.handle_editor_key("down", &kb);
        editor.cursor_col = 0;

        // Kill line2 to end
        let _ = editor.handle_editor_key("ctrl+k", &kb);
        assert_eq!(editor.kill_ring.peek(), Some("line2"));
        assert_eq!(editor.kill_ring.len(), 2);

        // Clear editor and test yank
        editor.clear();
        assert_eq!(editor.text(), "");

        // Yank pastes most recent: "line2"
        let _ = editor.handle_editor_key("ctrl+y", &kb);
        assert_eq!(editor.text(), "line2");

        // YankPop cycles to "line1"
        let _ = editor.handle_editor_key("alt+y", &kb);
        assert_eq!(editor.text(), "line1");

        // YankPop cycles back to "line2"
        let _ = editor.handle_editor_key("alt+y", &kb);
        assert_eq!(editor.text(), "line2");
    }

    #[test]
    fn test_word_navigation_punctuation() {
        let text = "foo.bar(baz)";
        // Forward navigation
        let p1 = find_word_forward(text, 0); // "foo"
        assert_eq!(p1, 3);
        let p2 = find_word_forward(text, p1); // "."
        assert_eq!(p2, 4);
        let p3 = find_word_forward(text, p2); // "bar"
        assert_eq!(p3, 7);
        let p4 = find_word_forward(text, p3); // "("
        assert_eq!(p4, 8);
        let p5 = find_word_forward(text, p4); // "baz"
        assert_eq!(p5, 11);
        let p6 = find_word_forward(text, p5); // ")"
        assert_eq!(p6, 12);

        // Backward navigation
        let b1 = find_word_backward(text, 12); // skips ")"
        assert_eq!(b1, 11);
        let b2 = find_word_backward(text, b1); // skips "baz"
        assert_eq!(b2, 8);
        let b3 = find_word_backward(text, b2); // skips "("
        assert_eq!(b3, 7);
        let b4 = find_word_backward(text, b3); // skips "bar"
        assert_eq!(b4, 4);
        let b5 = find_word_backward(text, b4); // skips "."
        assert_eq!(b5, 3);
        let b6 = find_word_backward(text, b5); // skips "foo"
        assert_eq!(b6, 0);
    }

    #[test]
    fn test_word_navigation_atomic_paste() {
        let text = "alpha [paste #1 +12 lines] omega";
        // From end of text (32), word backward lands on start of "omega" (27)
        let b1 = find_word_backward(text, 32);
        assert_eq!(b1, 27);
        // From start of "omega" (27), skips space (26), hits paste marker -> jumps to start (6)
        let b2 = find_word_backward(text, 27);
        assert_eq!(b2, 6);

        // Forward from 6 jumps past entire paste marker to 26
        let f1 = find_word_forward(text, 6);
        assert_eq!(f1, 26);

        // Test atomic backspace in editor
        let mut editor = Editor::new();
        editor.set_text(text);
        editor.cursor_col = 26; // right after paste marker
        let kb = Keybindings::default_bindings();
        let _ = editor.handle_editor_key("backspace", &kb);
        assert_eq!(editor.text(), "alpha  omega");
        assert_eq!(editor.cursor_col, 6);
    }

    #[test]
    fn test_consecutive_backward_word_kills_accumulate_in_reading_order() {
        let mut editor = Editor::new();
        editor.set_text("one two");
        editor.cursor_col = 7;
        let kb = Keybindings::default_bindings();

        // Delete "two"
        let _ = editor.handle_editor_key("ctrl+w", &kb);
        assert_eq!(editor.kill_ring.peek(), Some("two"));

        // Delete " " then "one" without moving cursor in between
        let _ = editor.handle_editor_key("ctrl+w", &kb);
        // Accumulated in reading order: "one " + "two" = "one two"
        assert_eq!(editor.kill_ring.peek(), Some("one two"));

        editor.clear();
        let _ = editor.handle_editor_key("ctrl+y", &kb);
        assert_eq!(editor.text(), "one two");
    }

    #[test]
    fn test_delete_word_forward_keybinding() {
        let mut editor = Editor::new();
        editor.set_text("first second");
        editor.cursor_col = 0;
        let kb = Keybindings::default_bindings();

        // alt+d deletes "first" forward
        let _ = editor.handle_editor_key("alt+d", &kb);
        assert_eq!(editor.text(), " second");
        assert_eq!(editor.kill_ring.peek(), Some("first"));
    }

    #[test]
    fn test_autocomplete_page_up_page_down() {
        let mut editor = Editor::new();
        let items: Vec<SelectItem> = (0..10)
            .map(|i| SelectItem::new(format!("item_{i}"), format!("Item {i}")))
            .collect();
        editor.set_autocomplete(items);
        let kb = Keybindings::default_bindings();

        assert_eq!(editor.autocomplete.as_ref().unwrap().selected, 0);

        let _ = editor.handle_editor_key("pagedown", &kb);
        assert_eq!(editor.autocomplete.as_ref().unwrap().selected, 5);

        let _ = editor.handle_editor_key("pageup", &kb);
        assert_eq!(editor.autocomplete.as_ref().unwrap().selected, 0);
    }
}
`````

