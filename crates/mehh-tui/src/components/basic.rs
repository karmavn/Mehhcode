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
