use crate::ansi::{truncate_to_width, wrap_text_with_ansi};
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
}

impl BorderedBox {
    pub fn new(child: Box<dyn Component>) -> Self {
        Self {
            title: None,
            style: BorderStyle::Rounded,
            child,
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
                format!("{tl}{h} {title} {}{tr}", h.to_string().repeat(right_pad))
            } else {
                format!("{tl}{}{tr}", h.to_string().repeat(inner_width))
            }
        } else {
            format!("{tl}{}{tr}", h.to_string().repeat(inner_width))
        };
        lines.push(top_line);

        // Child lines with vertical borders
        let child_lines = self.child.render(inner_width);
        for line in child_lines {
            let line_w = crate::ansi::visible_width(&line);
            let pad = inner_width.saturating_sub(line_w);
            lines.push(format!("{v}{line}{}{v}", " ".repeat(pad)));
        }

        // Bottom line
        lines.push(format!("{bl}{}{br}", h.to_string().repeat(inner_width)));

        lines
    }

    fn handle_key(&mut self, key: &str) -> bool {
        self.child.handle_key(key)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }
}

/// Animated Braille spinner frames.
pub const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated loader spinner with status message.
#[derive(Debug, Clone)]
pub struct Loader {
    pub label: String,
    pub frame: usize,
}

impl Loader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            frame: 0,
        }
    }

    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % BRAILLE_FRAMES.len();
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let spinner = BRAILLE_FRAMES[self.frame % BRAILLE_FRAMES.len()];
        let line = format!("\x1b[36m{spinner}\x1b[0m {}", self.label);
        wrap_text_with_ansi(&line, width)
    }
}

/// Selectable item for `SelectList`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

impl SelectItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
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
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self {
            items,
            filter: String::new(),
            selected: 0,
            max_visible: max_visible.max(1),
        }
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
        let filter_lower = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if filter_lower.is_empty() {
                    true
                } else {
                    item.label.to_lowercase().contains(&filter_lower)
                        || item.id.to_lowercase().contains(&filter_lower)
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn move_up(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            if self.selected == 0 {
                self.selected = filtered.len() - 1;
            } else {
                self.selected -= 1;
            }
        }
    }

    pub fn move_down(&mut self) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            self.selected = (self.selected + 1) % filtered.len();
        }
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        let filtered = self.filtered_indices();
        filtered.get(self.selected).map(|&idx| &self.items[idx])
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return vec![truncate_to_width("  No matching items", width, "...", false)];
        }

        let total = filtered.len();
        let max_vis = self.max_visible.min(total);
        let start = if self.selected >= max_vis {
            self.selected + 1 - max_vis
        } else {
            0
        };
        let end = (start + max_vis).min(total);

        let mut lines = Vec::new();
        for (vis_idx, &item_idx) in filtered[start..end].iter().enumerate() {
            let actual_idx = start + vis_idx;
            let item = &self.items[item_idx];
            let is_selected = actual_idx == self.selected;

            let desc_str = if let Some(ref d) = item.description {
                format!(" \x1b[2m{d}\x1b[0m")
            } else {
                String::new()
            };

            let line = if is_selected {
                format!("\x1b[36m> \x1b[1m{}\x1b[0m{desc_str}", item.label)
            } else {
                format!("  {}{desc_str}", item.label)
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
            _ => false,
        }
    }
}
