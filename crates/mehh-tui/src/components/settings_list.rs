//! Two-column settings list with description pane and value cycling,
//! ported from TypeScript `packages/tui/src/components/settings-list.ts`.

use crate::ansi::{sanitize_terminal_text, truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::fuzzy::fuzzy_filter_indexed;
use crate::theme::Theme;
use crate::tui::Component;

/// Row-to-item mapping for clickable settings rows: `(row_index -> (item_index, setting_id))`.
pub type SettingsListRowMap = Vec<Option<(usize, String)>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
    pub value_descriptions: Vec<(String, String)>,
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
            category: None,
            description: None,
            current_value: current_value.into(),
            values: Vec::new(),
            value_descriptions: Vec::new(),
        }
    }

    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_values(mut self, values: Vec<String>) -> Self {
        self.values = values;
        self
    }

    pub fn with_value_descriptions(mut self, descriptions: Vec<(String, String)>) -> Self {
        self.value_descriptions = descriptions;
        self
    }

    /// Returns the per-value explanation for the active value, when defined.
    pub fn current_value_description(&self) -> Option<&str> {
        self.value_descriptions
            .iter()
            .find(|(val, _)| val == &self.current_value)
            .map(|(_, desc)| desc.as_str())
    }
}

/// Two-column interactive settings list.
#[derive(Debug, Clone)]
pub struct SettingsList {
    pub items: Vec<SettingItem>,
    pub selected: usize,
    pub max_visible: usize,
    pub max_height: Option<usize>,
    pub filter: String,
    pub theme: Theme,
    pub status_message: Option<(String, bool)>,
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize) -> Self {
        Self {
            items,
            selected: 0,
            max_visible: max_visible.max(1),
            max_height: None,
            filter: String::new(),
            theme: Theme::default(),
            status_message: None,
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Sets an inline feedback banner (`is_error = true` renders with error styling).
    pub fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status_message = Some((msg.into(), is_error));
    }

    /// Clears the inline feedback banner.
    pub fn clear_status(&mut self) {
        self.status_message = None;
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
            let values_joined = item.values.join(" ");
            let mut parts = vec![item.label.as_str(), item.id.as_str()];
            if let Some(cat) = &item.category {
                parts.push(cat.as_str());
            }
            if let Some(desc) = &item.description {
                parts.push(desc.as_str());
            }
            if !values_joined.is_empty() {
                parts.push(values_joined.as_str());
            }
            if !item.values.contains(&item.current_value) {
                parts.push(item.current_value.as_str());
            }
            parts.join(" ")
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

    fn cycle_with_delta(&mut self, forward: bool) -> Option<(String, String)> {
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
        let next_idx = if forward {
            (curr_idx + 1) % item.values.len()
        } else if curr_idx == 0 {
            item.values.len() - 1
        } else {
            curr_idx - 1
        };
        let new_val = item.values[next_idx].clone();
        item.current_value = new_val.clone();
        Some((item.id.clone(), new_val))
    }

    /// Cycle the currently selected item's value forward to the next in `values`.
    /// Returns `Some((id, new_value))` if a cycle occurred.
    pub fn cycle_current(&mut self) -> Option<(String, String)> {
        self.cycle_with_delta(true)
    }

    /// Cycle the currently selected item's value backward to the previous in `values`.
    /// Returns `Some((id, new_value))` if a cycle occurred.
    pub fn cycle_prev(&mut self) -> Option<(String, String)> {
        self.cycle_with_delta(false)
    }
    fn render_option_chips(&self, item: &SettingItem, width: usize, theme: &Theme) -> Vec<String> {
        let label_prefix = format!("  {} ", theme.fg(&theme.muted, "Options:"));
        let cont_prefix = "           ";
        let prefix_width = 11usize;
        let mut lines = Vec::new();
        let mut current = label_prefix;
        let mut cur_width = prefix_width;
        let mut has_chip_on_line = false;

        for v in &item.values {
            let is_active = v == &item.current_value;
            let clean_v = sanitize_terminal_text(v, false);
            let plain_len = visible_width(&clean_v) + if is_active { 2 } else { 0 };
            let styled = if is_active {
                format!("\x1b[1m{}[{}]\x1b[0m", theme.accent, clean_v)
            } else {
                theme.fg(&theme.dim, &clean_v)
            };
            let gap = if has_chip_on_line { 2 } else { 0 };
            if has_chip_on_line && cur_width + gap + plain_len > width {
                lines.push(truncate_to_width(&current, width, "...", false));
                current = format!("{cont_prefix}{styled}");
                cur_width = prefix_width + plain_len;
            } else {
                if has_chip_on_line {
                    current.push_str("  ");
                    cur_width += 2;
                }
                current.push_str(&styled);
                cur_width += plain_len;
                has_chip_on_line = true;
            }
        }
        if has_chip_on_line {
            lines.push(truncate_to_width(&current, width, "...", false));
        }
        lines
    }

    #[allow(clippy::too_many_arguments)]
    fn render_with_layout_and_map(
        &self,
        width: usize,
        max_visible: usize,
        include_categories: bool,
        include_description: bool,
        filtered: &[usize],
        max_label_width: usize,
        theme: &Theme,
    ) -> (Vec<String>, Vec<Option<(usize, String)>>) {
        if filtered.is_empty() {
            if self.filter.trim().is_empty() {
                return (
                    vec![truncate_to_width(
                        "  No matching settings",
                        width,
                        "...",
                        false,
                    )],
                    vec![None],
                );
            }
            let clean_filter = sanitize_terminal_text(self.filter.trim(), false);
            let msg = format!(
                "No match for '{}' · Backspace or Esc to clear",
                clean_filter
            );
            let mut lines: Vec<String> =
                wrap_text_with_ansi(&msg, width.saturating_sub(4).max(10))
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
            lines.push(String::new());
            lines.push(truncate_to_width(
                &format!(
                    "  {}",
                    theme.fg(
                        &theme.dim,
                        "↑↓ Move · ←→/Enter Change · Backspace · Esc Clear filter",
                    )
                ),
                width,
                "...",
                false,
            ));
            let row_map = vec![None; lines.len()];
            return (lines, row_map);
        }

        let total = filtered.len();
        let selected = self.selected.min(total.saturating_sub(1));
        let max_vis = max_visible.max(1).min(total);
        let start = (selected as isize - (max_vis as isize / 2))
            .clamp(0, (total as isize - max_vis as isize).max(0)) as usize;
        let end = (start + max_vis).min(total);

        let show_categories = include_categories && self.filter.trim().is_empty();
        let mut last_category: Option<&str> = None;
        let mut lines = Vec::new();
        let mut row_map: Vec<Option<(usize, String)>> = Vec::new();
        for (vis_idx, &item_idx) in filtered[start..end].iter().enumerate() {
            let actual_idx = start + vis_idx;
            let item = &self.items[item_idx];
            if show_categories {
                if let Some(cat) = item.category.as_deref() {
                    if last_category != Some(cat) {
                        let clean_cat = sanitize_terminal_text(cat, false);
                        lines.push(truncate_to_width(
                            &format!("  {}", theme.fg(&theme.muted, &clean_cat)),
                            width,
                            "...",
                            false,
                        ));
                        row_map.push(None);
                        last_category = Some(cat);
                    }
                }
            }
            let is_selected = actual_idx == selected;

            let clean_label = sanitize_terminal_text(&item.label, false);
            let clean_val = sanitize_terminal_text(&item.current_value, false);
            let val_display = if is_selected && item.values.len() > 1 {
                format!("‹ {clean_val} ›")
            } else {
                clean_val
            };
            let label_len = visible_width(&clean_label);
            let pad = " ".repeat(max_label_width.saturating_sub(label_len));
            let value_len = visible_width(&val_display);
            let used = 2 + label_len + pad.len() + 2 + value_len;
            let tail_pad = " ".repeat(width.saturating_sub(used));
            let reset = "\x1b[0m";
            let bg = theme.selected_bg.as_str();
            let rebg = bg;

            let row = if is_selected {
                format!(
                    "{bg}{accent}▎ {reset}{rebg}\x1b[1m{label}{pad}  {reset}{rebg}{value_color}{val_display}{tail_pad}{reset}",
                    accent = theme.accent,
                    label = clean_label,
                    value_color = theme.accent,
                )
            } else {
                let val_color = match item.current_value.as_str() {
                    "auto" => &theme.warning,
                    "off" | "hide" | "false" => &theme.dim,
                    _ => &theme.text,
                };
                format!(
                    "  {label}{pad}  {reset}{val_color}{val_display}{tail_pad}{reset}",
                    label = clean_label,
                )
            };
            lines.push(truncate_to_width(&row, width, "...", false));
            row_map.push(Some((actual_idx, item.id.clone())));
        }

        if total > max_vis {
            lines.push(truncate_to_width(
                &format!(
                    "  {}",
                    theme.fg(&theme.dim, &format!("({}/{}) · ↑↓ scroll", selected + 1, total))
                ),
                width,
                "...",
                false,
            ));
            row_map.push(None);
        }

        let selected_item = Some(&self.items[filtered[selected]]);
        let has_inspector = selected_item.is_some_and(|it| {
            !it.values.is_empty()
                || (include_description && it.description.is_some())
                || it.current_value_description().is_some()
        }) || self.status_message.is_some();

        if has_inspector {
            let sep_len = width.saturating_sub(4).clamp(1, 60);
            lines.push(truncate_to_width(
                &format!(
                    "  {}",
                    theme.fg(&theme.border_muted, &"─".repeat(sep_len))
                ),
                width,
                "...",
                false,
            ));
            row_map.push(None);
        }

        if let Some(item) = selected_item {
            if !item.values.is_empty() {
                let chip_lines = self.render_option_chips(item, width, theme);
                let count = chip_lines.len();
                lines.extend(chip_lines);
                row_map.extend(std::iter::repeat_n(None, count));
            }

            if include_description {
                if let Some(desc) = item.description.as_deref() {
                    let clean_desc = sanitize_terminal_text(desc, false);
                    let wrapped = wrap_text_with_ansi(&clean_desc, width.saturating_sub(4).max(10));
                    for w in wrapped {
                        lines.push(truncate_to_width(
                            &format!("  {}", theme.fg(&theme.muted, &w)),
                            width,
                            "...",
                            false,
                        ));
                        row_map.push(None);
                    }
                }
            }

            if let Some(val_desc) = item.current_value_description() {
                let wrapped = wrap_text_with_ansi(val_desc, width.saturating_sub(6).max(10));
                for (idx, w) in wrapped.into_iter().enumerate() {
                    let prefix = if idx == 0 { "  ▸ " } else { "    " };
                    lines.push(truncate_to_width(
                        &format!("{prefix}{}", theme.fg(&theme.dim, &w)),
                        width,
                        "...",
                        false,
                    ));
                    row_map.push(None);
                }
            }
        }

        if let Some((ref msg, is_err)) = self.status_message {
            let (icon, color) = if is_err {
                ("✗", &theme.error)
            } else {
                ("✓", &theme.success)
            };
            lines.push(truncate_to_width(
                &format!("  {}", theme.fg(color, &format!("{icon} {msg}"))),
                width,
                "...",
                false,
            ));
            row_map.push(None);
        }

        let controls = if !self.filter.is_empty() {
            "↑↓ Move · ←→/Enter Change · Backspace · Esc Clear filter"
        } else {
            "↑↓ Move · ←→ / Enter Change · Type to filter · Esc Close"
        };
        lines.push(truncate_to_width(
            &format!("  {}", theme.fg(&theme.dim, controls)),
            width,
            "...",
            false,
        ));
        row_map.push(None);

        (lines, row_map)
    }

    fn option_chip_line_count(&self, item: &SettingItem, width: usize) -> usize {
        if item.values.is_empty() {
            return 0;
        }
        let prefix_width = 11usize;
        let mut lines = 0usize;
        let mut cur_width = prefix_width;
        let mut has_chip_on_line = false;
        for v in &item.values {
            let clean_v = sanitize_terminal_text(v, false);
            let is_active = v == &item.current_value;
            let plain_len = visible_width(&clean_v) + if is_active { 2 } else { 0 };
            let gap = if has_chip_on_line { 2 } else { 0 };
            if has_chip_on_line && cur_width + gap + plain_len > width {
                lines += 1;
                cur_width = prefix_width + plain_len;
            } else {
                if has_chip_on_line {
                    cur_width += 2;
                }
                cur_width += plain_len;
                has_chip_on_line = true;
            }
        }
        if has_chip_on_line {
            lines += 1;
        }
        lines
    }

    fn layout_line_count(
        &self,
        _width: usize,
        max_visible: usize,
        include_categories: bool,
        filtered: &[usize],
        footer_base: usize,
        desc_rows: usize,
    ) -> usize {
        let total = filtered.len();
        let selected = self.selected.min(total.saturating_sub(1));
        let max_vis = max_visible.max(1).min(total);
        let start = (selected as isize - (max_vis as isize / 2))
            .clamp(0, (total as isize - max_vis as isize).max(0)) as usize;
        let end = (start + max_vis).min(total);

        let show_categories = include_categories && self.filter.trim().is_empty();
        let mut count = end - start;
        if show_categories {
            let mut last_category: Option<&str> = None;
            for &item_idx in &filtered[start..end] {
                if let Some(cat) = self.items[item_idx].category.as_deref() {
                    if last_category != Some(cat) {
                        count += 1;
                        last_category = Some(cat);
                    }
                }
            }
        }
        if total > max_vis {
            count += 1;
        }
        count + footer_base + desc_rows
    }

    /// Renders the settings list with an explicit `theme` and `max_height` override without cloning `SettingsList`,
    /// computing `filtered_indices()` once and returning `(lines, row_map, shown_count)`.
    pub fn render_with_row_map_themed(
        &self,
        width: usize,
        theme: &Theme,
        max_height: Option<usize>,
    ) -> (Vec<String>, SettingsListRowMap, usize) {
        let filtered = self.filtered_indices();
        let total = filtered.len();
        let max_label_width = self
            .items
            .iter()
            .map(|it| visible_width(&sanitize_terminal_text(&it.label, false)))
            .max()
            .unwrap_or(12)
            .clamp(10, 36);

        let Some(budget) = max_height else {
            let (lines, row_map) = self.render_with_layout_and_map(
                width,
                self.max_visible,
                true,
                true,
                &filtered,
                max_label_width,
                theme,
            );
            return (lines, row_map, total);
        };
        if total == 0 {
            let (mut lines, mut row_map) = self.render_with_layout_and_map(
                width,
                self.max_visible,
                true,
                true,
                &filtered,
                max_label_width,
                theme,
            );
            if lines.len() > budget {
                let last_line = lines.pop();
                let last_map = row_map.pop();
                let keep = budget.saturating_sub(usize::from(last_line.is_some()));
                lines.truncate(keep);
                row_map.truncate(keep);
                if let (Some(c), Some(m)) = (last_line, last_map) {
                    if budget > 0 {
                        lines.push(c);
                        row_map.push(m);
                    }
                }
            }
            return (lines, row_map, total);
        }

        let selected = self.selected.min(total.saturating_sub(1));
        let selected_item = &self.items[filtered[selected]];
        let chips_count = self.option_chip_line_count(selected_item, width);
        let desc_count = selected_item
            .description
            .as_deref()
            .map(|desc| {
                let clean_desc = sanitize_terminal_text(desc, false);
                wrap_text_with_ansi(&clean_desc, width.saturating_sub(4).max(10)).len()
            })
            .unwrap_or(0);
        let val_desc_count = selected_item
            .current_value_description()
            .map(|v| wrap_text_with_ansi(v, width.saturating_sub(6).max(10)).len())
            .unwrap_or(0);
        let status_count = usize::from(self.status_message.is_some());
        let sep_no_desc = usize::from(
            !selected_item.values.is_empty()
                || selected_item.current_value_description().is_some()
                || self.status_message.is_some(),
        );
        let sep_with_desc = usize::from(
            !selected_item.values.is_empty()
                || selected_item.description.is_some()
                || selected_item.current_value_description().is_some()
                || self.status_message.is_some(),
        );
        let footer_no_desc = sep_no_desc + chips_count + val_desc_count + status_count + 1;
        let footer_with_desc = sep_with_desc + chips_count + val_desc_count + status_count + 1;

        let start_vis = self.max_visible.min(total);
        for (cats, desc) in [(true, true), (true, false), (false, false)] {
            let (f_base, d_rows) = if desc {
                (footer_with_desc, desc_count)
            } else {
                (footer_no_desc, 0)
            };
            for vis in (1..=start_vis).rev() {
                if self.layout_line_count(width, vis, cats, &filtered, f_base, d_rows) <= budget {
                    let (lines, row_map) = self.render_with_layout_and_map(
                        width,
                        vis,
                        cats,
                        desc,
                        &filtered,
                        max_label_width,
                        theme,
                    );
                    return (lines, row_map, total);
                }
            }
        }
        let (mut lines, mut row_map) = self.render_with_layout_and_map(
            width,
            1,
            false,
            false,
            &filtered,
            max_label_width,
            theme,
        );
        if lines.len() > budget {
            let last_line = lines.pop();
            let last_map = row_map.pop();
            let keep = budget.saturating_sub(usize::from(last_line.is_some()));
            lines.truncate(keep);
            row_map.truncate(keep);
            if let (Some(c), Some(m)) = (last_line, last_map) {
                if budget > 0 {
                    lines.push(c);
                    row_map.push(m);
                }
            }
        }
        (lines, row_map, total)
    }

    /// Renders the settings list together with a 1-to-1 line map of `(filtered_pos, setting_id)`
    /// for clickable setting rows (headers, chips, descriptions, and status lines map to `None`).
    pub fn render_with_row_map(&self, width: usize) -> (Vec<String>, Vec<Option<(usize, String)>>) {
        let (lines, row_map, _) =
            self.render_with_row_map_themed(width, &self.theme, self.max_height);
        (lines, row_map)
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_with_row_map(width).0
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
            "left" => self.cycle_prev().is_some(),
            "right" | "enter" | " " | "return" => self.cycle_current().is_some(),
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

    #[test]
    fn test_settings_list_left_right_cycle_and_wrap() {
        let items = vec![SettingItem::new("mode", "Permission mode", "smart_approve")
            .with_values(vec![
                "smart_approve".into(),
                "approve".into(),
                "auto".into(),
                "chat".into(),
            ])];
        let mut list = SettingsList::new(items, 5);
        assert!(list.handle_key("left"));
        assert_eq!(list.items[0].current_value, "chat");
        assert!(list.handle_key("right"));
        assert_eq!(list.items[0].current_value, "smart_approve");
        assert_eq!(
            list.cycle_prev(),
            Some(("mode".to_string(), "chat".to_string()))
        );
    }

    #[test]
    fn test_settings_list_categories_options_value_desc_and_status() {
        let items = vec![
            SettingItem::new("permission_mode", "Permission mode", "smart_approve")
                .with_category("SAFETY & AGENT")
                .with_description("Controls tool approval prompts")
                .with_values(vec!["smart_approve".into(), "chat".into()])
                .with_value_descriptions(vec![
                    ("smart_approve".into(), "Asks before edits".into()),
                    ("chat".into(), "Read-only exploration".into()),
                ]),
            SettingItem::new("theme", "Color theme", "dark")
                .with_category("APPEARANCE")
                .with_values(vec!["dark".into(), "light".into()]),
        ];
        let mut list = SettingsList::new(items, 8);
        list.set_status("Saved: Permission mode -> smart_approve", false);
        let plain = crate::ansi::strip_ansi(&list.render(76).join("\n"));
        assert!(plain.contains("SAFETY & AGENT"), "{plain}");
        assert!(plain.contains("APPEARANCE"), "{plain}");
        assert!(plain.contains("‹ smart_approve ›"), "{plain}");
        assert!(plain.contains("Options: [smart_approve]  chat"), "{plain}");
        assert!(plain.contains("▸ Asks before edits"), "{plain}");
        assert!(plain.contains("✓ Saved: Permission mode -> smart_approve"), "{plain}");

        // Active filter hides category headers and shows clear-filter hint
        list.set_filter("chat");
        let filtered_plain = crate::ansi::strip_ansi(&list.render(76).join("\n"));
        assert!(!filtered_plain.contains("SAFETY & AGENT"), "{filtered_plain}");
        assert!(filtered_plain.contains("Permission mode"), "{filtered_plain}");
        assert!(filtered_plain.contains("Esc Clear filter"), "{filtered_plain}");

        // Empty filter result shows helpful recovery prompt without clipping at narrow widths
        list.set_filter("zzzzz");
        let empty_plain = crate::ansi::strip_ansi(&list.render(58).join("\n"));
        assert!(
            empty_plain.contains("No match for 'zzzzz'"),
            "{empty_plain}"
        );
        assert!(
            empty_plain.contains("Esc to clear"),
            "empty filter hint must not clip at 58 cols: {empty_plain}"
        );
        assert!(
            empty_plain.contains("Esc Clear filter"),
            "controls hint must stay visible on empty filter: {empty_plain}"
        );
    }

    #[test]
    fn test_settings_list_option_chips_wrap_cleanly_and_respect_max_height() {
        let items = vec![
            SettingItem::new("permission_mode", "Permission mode", "smart_approve")
                .with_category("SAFETY & AGENT")
                .with_description("Controls when the agent asks before running tools or shell commands")
                .with_values(vec![
                    "chat".into(),
                    "approve".into(),
                    "smart_approve".into(),
                    "auto".into(),
                ])
                .with_value_descriptions(vec![
                    ("smart_approve".into(), "Asks before edits or risky commands".into()),
                ]),
            SettingItem::new("theme", "Color theme", "dark")
                .with_category("APPEARANCE")
                .with_values(vec!["dark".into(), "light".into()]),
        ];
        let mut list = SettingsList::new(items, 8);
        list.set_status("Saved: Permission mode -> smart_approve", false);

        // Narrow width forces Options chips onto two lines without truncating any chip
        let chip_lines = list.render(38);
        let chip_plain = crate::ansi::strip_ansi(&chip_lines.join("\n"));
        assert!(chip_plain.contains("[smart_approve]"), "{chip_plain}");
        assert!(chip_plain.contains("auto"), "{chip_plain}");
        let chip_text_lines: Vec<&str> = chip_plain.lines().collect();
        let options_idx = chip_text_lines
            .iter()
            .position(|line| line.contains("Options:"))
            .expect("Options line should render");
        for line in chip_text_lines[options_idx..]
            .iter()
            .take_while(|line| line.contains("Options:") || line.starts_with("           "))
        {
            assert!(!line.contains("..."), "chips must wrap, not truncate: {line}");
        }
        for line in &chip_lines {
            assert!(
                visible_width(line) <= 38,
                "option line exceeded width 38: {:?}",
                crate::ansi::strip_ansi(line)
            );
        }

        // max_height tightly bounds output even with categories, descriptions, and status
        list.max_height = Some(10);
        let bounded = list.render(58);
        assert!(
            bounded.len() <= 10,
            "max_height=10 exceeded ({}): {:?}",
            bounded.len(),
            bounded
        );
        let bounded_plain = crate::ansi::strip_ansi(&bounded.join("\n"));
        assert!(bounded_plain.contains("Esc Close"), "{bounded_plain}");
    }

    #[test]
    fn test_settings_list_render_with_row_map_tracks_exact_clickable_rows() {
        let items = vec![
            SettingItem::new("mode", "Permission mode", "smart_approve")
                .with_category("SAFETY")
                .with_description("Control tool approvals"),
            SettingItem::new("theme", "Color theme", "dark")
                .with_category("APPEARANCE"),
            SettingItem::new("compact", "Auto compact", "on")
                .with_category("APPEARANCE"),
        ];
        let mut list = SettingsList::new(items, 8);
        let (lines, map) = list.render_with_row_map(60);
        assert_eq!(lines.len(), map.len());
        // Line 0: SAFETY header -> None
        assert_eq!(map[0], None);
        // Line 1: mode -> Some((0, "mode"))
        assert_eq!(map[1], Some((0, "mode".to_string())));
        // Line 2: APPEARANCE header -> None
        assert_eq!(map[2], None);
        // Line 3: theme -> Some((1, "theme"))
        assert_eq!(map[3], Some((1, "theme".to_string())));
        // Line 4: compact -> Some((2, "compact"))
        assert_eq!(map[4], Some((2, "compact".to_string())));

        // With filter active, category headers disappear and filtered_pos resets cleanly
        list.set_filter("APPEARANCE");
        let (flines, fmap) = list.render_with_row_map(60);
        assert_eq!(flines.len(), fmap.len());
        assert_eq!(fmap[0], Some((0, "theme".to_string())));
        assert_eq!(fmap[1], Some((1, "compact".to_string())));

        // With tight max_height, map length stays aligned with lines
        list.set_filter("");
        list.max_height = Some(6);
        let (blines, bmap) = list.render_with_row_map(60);
        assert_eq!(blines.len(), bmap.len());
        assert!(blines.len() <= 6);
        assert!(bmap.iter().flatten().any(|(idx, id)| *idx == 0 && id == "mode"));
    }

    #[test]
    fn settings_list_sanitizes_all_rendered_fields() {
        let items = vec![SettingItem::new(
            "s1",
            "\x1b]0;pwn\x07\x1b[31mLab\u{202E}el",
            "\x1b]52;c;AA\x07val\u{2066}1",
        )
        .with_category("\x1b[33mCAT\u{202E}1")
        .with_description("\x1b]0;bad\x07desc\u{202E}ription")
        .with_values(vec![
            "\x1b]52;c;AA\x07val\u{2066}1".into(),
            "\x1b[32mval\u{202E}2".into(),
        ])];
        let mut list = SettingsList::new(items, 5);
        let joined = list.render(60).join("\n");
        for forbidden in ["\x1b]0;", "\x1b]52;", "\x1b[31m", "\x1b[32m", "\x1b[33m", "\u{202E}", "\u{2066}"] {
            assert!(!joined.contains(forbidden), "unsanitized {forbidden:?} in {joined:?}");
        }
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(plain.contains("CAT1"), "{plain}");
        assert!(plain.contains("Label"), "{plain}");
        assert!(plain.contains("val1"), "{plain}");
        assert!(plain.contains("val2"), "{plain}");
        assert!(plain.contains("description"), "{plain}");

        list.set_filter("\x1b]0;pwn\x07\x1b[31mnomatch\u{202E}");
        let empty_joined = list.render(60).join("\n");
        for forbidden in ["\x1b]0;", "\x1b[31m", "\u{202E}"] {
            assert!(!empty_joined.contains(forbidden), "unsanitized {forbidden:?} in {empty_joined:?}");
        }
        let empty_plain = crate::ansi::strip_ansi(&empty_joined);
        assert!(empty_plain.contains("No match for 'nomatch'"), "{empty_plain}");
    }
}
