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
        if width == 0 {
            return Vec::new();
        }
        let pad_x = self.padding_x;
        let inner_width = width.saturating_sub(pad_x * 2);
        if inner_width == 0 {
            return Vec::new();
        }
        let wrapped = wrap_text_with_ansi(&self.content, inner_width);
        if pad_x == 0 {
            wrapped
        } else {
            wrapped
                .into_iter()
                .map(|line| {
                    let mut out = String::with_capacity(line.len() + pad_x * 2);
                    out.extend(std::iter::repeat_n(' ', pad_x));
                    out.push_str(&line);
                    out.extend(std::iter::repeat_n(' ', pad_x));
                    out
                })
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
        let child_lines = self.child.render(inner_width);
        let mut lines = Vec::with_capacity(child_lines.len() + 2);
        let mut h_fill = String::with_capacity(inner_width * h.len_utf8());
        h_fill.extend(std::iter::repeat_n(h, inner_width));
        let mut v_buf = [0u8; 4];
        let v_str = v.encode_utf8(&mut v_buf);
        let left = paint(&self.border_color, v_str);
        let right = paint(&self.border_color, v_str);

        // Top line with optional title
        let top_line = if let Some(ref title) = self.title {
            let title_w = crate::ansi::visible_width(title);
            let fitted = if title_w + 4 <= inner_width {
                Some((std::borrow::Cow::Borrowed(title.as_str()), title_w))
            } else if inner_width >= 6 {
                let truncated =
                    truncate_to_width(title, inner_width.saturating_sub(4), "…", false);
                let trunc_w = crate::ansi::visible_width(&truncated);
                Some((std::borrow::Cow::Owned(truncated), trunc_w))
            } else {
                None
            };
            if let Some((display_title, display_w)) = fitted {
                let right_pad = inner_width - display_w - 3;
                let mut top_inner = String::with_capacity(inner_width + 16);
                top_inner.push(tl);
                top_inner.push(h);
                top_inner.push(' ');
                if !self.title_color.is_empty() {
                    top_inner.push_str(&self.title_color);
                    top_inner.push_str(&display_title);
                    top_inner.push_str("\x1b[0m");
                    top_inner.push_str(&self.border_color);
                } else {
                    top_inner.push_str(&display_title);
                }
                top_inner.push(' ');
                top_inner.extend(std::iter::repeat_n(h, right_pad));
                top_inner.push(tr);
                paint(&self.border_color, &top_inner)
            } else {
                paint(&self.border_color, &format!("{tl}{h_fill}{tr}"))
            }
        } else {
            paint(&self.border_color, &format!("{tl}{h_fill}{tr}"))
        };
        lines.push(top_line);

        // Child lines with vertical borders. Paint only the border glyphs:
        // wrapping the whole line would tint plain child text (e.g. the prompt
        // input) with the border color, which is nearly invisible in dark themes.
        for line in child_lines {
            let line_w = crate::ansi::visible_width(&line);
            let (fitted, fitted_w) = if line_w > inner_width {
                let has_cursor = line.contains(crate::tui::CURSOR_MARKER);
                let mut truncated = truncate_to_width(&line, inner_width, "", false);
                if has_cursor && !truncated.contains(crate::tui::CURSOR_MARKER) {
                    truncated.push_str(crate::tui::CURSOR_MARKER);
                }
                let w = crate::ansi::visible_width(&truncated);
                (truncated, w)
            } else {
                (line, line_w)
            };
            let pad = inner_width.saturating_sub(fitted_w);
            let mut row = String::with_capacity(left.len() + fitted.len() + pad + right.len());
            row.push_str(&left);
            row.push_str(&fitted);
            row.extend(std::iter::repeat_n(' ', pad));
            row.push_str(&right);
            lines.push(row);
        }

        // Bottom line
        lines.push(paint(&self.border_color, &format!("{bl}{h_fill}{br}")));

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
        Self::new_with_theme(items, max_visible, Theme::default())
    }

    pub fn new_with_theme(items: Vec<SelectItem>, max_visible: usize, theme: Theme) -> Self {
        Self {
            items,
            filter: String::new(),
            selected: 0,
            max_visible: max_visible.max(1),
            theme,
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

    /// Returns `(start, end, filtered_indices)` for the currently visible viewport rows.
    pub fn visible_range(&self) -> (usize, usize, Vec<usize>) {
        let filtered = self.filtered_indices();
        let (start, end) = Self::visible_bounds(self.selected, self.max_visible, filtered.len());
        (start, end, filtered)
    }

    fn visible_bounds(selected: usize, max_visible: usize, total: usize) -> (usize, usize) {
        if total == 0 {
            return (0, 0);
        }
        let clamped_sel = selected.min(total.saturating_sub(1));
        let max_vis = max_visible.max(1).min(total);
        let start = (clamped_sel as isize - (max_vis as isize / 2))
            .clamp(0, (total as isize - max_vis as isize).max(0)) as usize;
        let end = (start + max_vis).min(total);
        (start, end)
    }

    /// Renders pre-filtered rows using the provided `theme` and `max_visible` cap without cloning the list.
    pub fn render_with_filtered(
        &self,
        width: usize,
        theme: &Theme,
        max_visible: usize,
        filtered: &[usize],
    ) -> (Vec<String>, usize, usize) {
        let (start, end) = Self::visible_bounds(self.selected, max_visible, filtered.len());
        if filtered.is_empty() {
            if self.filter.trim().is_empty() {
                return (
                    vec![truncate_to_width(
                        &format!("  {}", theme.fg(&theme.dim, "No matching items")),
                        width,
                        "...",
                        false,
                    )],
                    0,
                    0,
                );
            }
            let clean_filter = crate::ansi::sanitize_terminal_text(self.filter.trim(), false);
            let msg = format!(
                "No match for '{clean_filter}' · Backspace or Esc to clear"
            );
            let lines = wrap_text_with_ansi(&msg, width.saturating_sub(4).max(10))
                .into_iter()
                .map(|w| {
                    truncate_to_width(
                        &format!("  {}", theme.fg(&theme.dim, &w)),
                        width,
                        "...",
                        false,
                    )
                })
                .collect();
            return (lines, 0, 0);
        }

        let selected = self.selected.min(filtered.len().saturating_sub(1));

        let max_label_width = filtered
            .iter()
            .map(|&idx| {
                let clean = crate::ansi::sanitize_terminal_text_cow(&self.items[idx].label, false);
                crate::ansi::visible_width(&clean)
            })
            .max()
            .unwrap_or(12)
            .clamp(10, 36);

        let mut lines = Vec::with_capacity(end.saturating_sub(start));
        let reset = "\x1b[0m";
        let bg = theme.selected_bg.as_str();
        for (vis_idx, &item_idx) in filtered[start..end].iter().enumerate() {
            let actual_idx = start + vis_idx;
            let item = &self.items[item_idx];
            let is_selected = actual_idx == selected;

            let clean_label = crate::ansi::sanitize_terminal_text_cow(&item.label, false);
            let label_len = crate::ansi::visible_width(&clean_label);
            let label_pad_len = max_label_width.saturating_sub(label_len);
            let clean_desc = crate::ansi::sanitize_terminal_text_cow(
                item.description.as_deref().unwrap_or(""),
                false,
            );
            let desc_len = crate::ansi::visible_width(&clean_desc);
            let used = 2 + label_len + label_pad_len + 2 + desc_len;
            let tail_pad_len = width.saturating_sub(used);
            let label_color = item.label_color.as_deref().unwrap_or(&theme.text);

            let mut line = String::with_capacity(width + 64);
            if is_selected {
                line.push_str(bg);
                line.push_str(&theme.accent);
                line.push_str("▎ ");
                line.push_str(reset);
                line.push_str(bg);
                line.push_str("\x1b[1m");
                line.push_str(label_color);
                line.push_str(&clean_label);
                line.extend(std::iter::repeat_n(' ', label_pad_len));
                line.push_str("  ");
                line.push_str(reset);
                line.push_str(bg);
                line.push_str(&theme.dim);
                line.push_str(&clean_desc);
                line.extend(std::iter::repeat_n(' ', tail_pad_len));
                line.push_str(reset);
            } else {
                line.push_str("  ");
                line.push_str(label_color);
                line.push_str(&clean_label);
                line.extend(std::iter::repeat_n(' ', label_pad_len));
                line.push_str("  ");
                line.push_str(reset);
                line.push_str(&theme.dim);
                line.push_str(&clean_desc);
                line.extend(std::iter::repeat_n(' ', tail_pad_len));
                line.push_str(reset);
            }

            lines.push(truncate_to_width(&line, width, "...", false));
        }

        (lines, start, end)
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = self.filtered_indices();
        self.render_with_filtered(width, &self.theme, self.max_visible, &filtered)
            .0
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
    fn bordered_box_child_text_does_not_inherit_border_color() {
        let boxed = BorderedBox::new(Box::new(Text::new("hi")))
            .with_border_color("\x1b[38;2;1;2;3m")
            .with_title("T");
        let lines = boxed.render(20);
        let content = lines.iter().find(|l| l.contains("hi")).unwrap();
        assert!(
            content.contains("\x1b[0mhi"),
            "child text must be painted with its own/default color, not the border color: {content:?}"
        );
    }

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
    fn bordered_box_truncates_long_title_instead_of_dropping_it() {
        let boxed = BorderedBox::new(Box::new(Text::new("body")))
            .with_title("VeryLongSectionTitle");
        // width = 14 => inner_width = 12 (>= 6); max title width = 8 ("VeryLon…")
        let lines = boxed.render(14);
        assert_eq!(crate::ansi::visible_width(&lines[0]), 14, "{lines:?}");
        assert!(lines[0].contains("VeryLon\u{2026}"), "{lines:?}");

        // width = 7 => inner_width = 5 (< 6); drops title cleanly without panic or overflow
        let narrow = boxed.render(7);
        assert_eq!(crate::ansi::visible_width(&narrow[0]), 7, "{narrow:?}");
        assert!(!narrow[0].contains("Very"), "{narrow:?}");
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

    #[test]
    fn test_select_list_visible_range_tracks_scroll_and_filter() {
        let items: Vec<SelectItem> = (0..8)
            .map(|i| SelectItem::new(format!("id{i}"), format!("Option {i}")))
            .collect();
        let mut list = SelectList::new(items, 4);
        list.selected = 5;
        let (start, end, filtered) = list.visible_range();
        assert_eq!((start, end), (3, 7));
        let rendered = list.render(40);
        assert_eq!(rendered.len(), 4);
        for (row_idx, pos) in (start..end).enumerate() {
            let expected = &list.items[filtered[pos]].label;
            assert!(rendered[row_idx].contains(expected));
        }

        list.set_filter("Option 6");
        let (fstart, fend, ffiltered) = list.visible_range();
        assert_eq!((fstart, fend), (0, 1));
        assert_eq!(ffiltered, vec![6]);
    }

    #[test]
    fn select_list_sanitizes_ansi_osc_and_bidi_in_label_and_description() {
        let item = SelectItem::new("id1", "\x1b]0;pwn\x07\x1b[31mModel\u{202E}X\x1b[0m")
            .with_description("\x1b]52;c;AAAA\x07desc\u{2066}hidden\u{2069}");
        let list = SelectList::new(vec![item], 4);
        let lines = list.render(50);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("\x1b]0;"), "OSC title must be stripped: {:?}", lines[0]);
        assert!(!lines[0].contains("\x1b]52;"), "OSC 52 must be stripped: {:?}", lines[0]);
        assert!(!lines[0].contains('\u{202E}'), "Bidi override must be stripped: {:?}", lines[0]);
        assert!(!lines[0].contains('\u{2066}'), "Bidi isolate must be stripped: {:?}", lines[0]);
        let plain = crate::ansi::strip_ansi(&lines[0]);
        assert!(plain.contains("ModelX"), "{plain:?}");
        assert!(plain.contains("deschidden"), "{plain:?}");
    }

    #[test]
    fn select_list_empty_filter_sanitizes_escape_and_bidi_in_no_match_message() {
        let mut list = SelectList::new(vec![SelectItem::new("id1", "Alpha")], 4);
        list.set_filter("\x1b]0;pwn\x07\x1b[31mbad\u{202E}");
        let lines = list.render(60);
        let joined = lines.join("\n");
        assert!(!joined.contains("\x1b]0;"), "OSC leaked: {joined:?}");
        assert!(!joined.contains("\x1b[31m"), "CSI leaked: {joined:?}");
        assert!(!joined.contains('\u{202E}'), "Bidi leaked: {joined:?}");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(plain.contains("No match for 'bad'"), "{plain:?}");
    }

    #[test]
    fn text_render_handles_zero_width_and_zero_inner_width() {
        assert!(Text::new("hello").render(0).is_empty());
        let lines = Text::new("line1\nline2").with_padding(2).render(4);
        for l in &lines {
            assert!(!l.contains('\n'), "raw newline leaked: {l:?}");
            assert!(crate::ansi::visible_width(l) <= 4, "overflow: {l:?}");
        }
    }

    #[test]
    fn bordered_box_truncates_overflow_preserving_cursor_and_pads_before_border_color() {
        struct FixedLines(Vec<String>);
        impl Component for FixedLines {
            fn render(&self, _width: usize) -> Vec<String> {
                self.0.clone()
            }
        }

        let boxed = BorderedBox::new(Box::new(FixedLines(vec![format!(
            "abcdef{}",
            crate::tui::CURSOR_MARKER
        )])));
        let lines = boxed.render(7); // inner_width = 5
        assert_eq!(crate::ansi::visible_width(&lines[1]), 7, "{lines:?}");
        assert!(
            lines[1].contains(crate::tui::CURSOR_MARKER),
            "cursor marker dropped during truncation: {:?}",
            lines[1]
        );

        let boxed_cursor = BorderedBox::new(Box::new(FixedLines(vec![format!(
            "> hi{}",
            crate::tui::CURSOR_MARKER
        )])))
        .with_border_color("\x1b[38;2;1;2;3m");
        let rendered = boxed_cursor.render(20);
        let mut term = crate::tui::MockTerminal::new(20, 5);
        let mut tui = crate::tui::TuiRenderer::new();
        tui.render_frame(&mut term, rendered, false).unwrap();
        let out = term.full_output();
        assert!(
            out.contains("> hi\x1b[7m \x1b[27m"),
            "fake cursor must sit on uncolored pad space before border_color: {out:?}"
        );
        assert!(
            !out.contains("\x1b[7m│\x1b[27m"),
            "fake cursor must not invert right border: {out:?}"
        );
    }
}
