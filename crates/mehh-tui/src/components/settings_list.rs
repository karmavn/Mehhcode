//! Two-column settings list with description pane and value cycling,
//! ported from TypeScript `packages/tui/src/components/settings-list.ts`.

use crate::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::fuzzy::fuzzy_filter_indexed;
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
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize) -> Self {
        Self {
            items,
            selected: 0,
            max_visible: max_visible.max(1),
            filter: String::new(),
        }
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
            return vec![truncate_to_width("  No matching settings", width, "...", false)];
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

            let prefix = if is_selected {
                "\x1b[36m> \x1b[0m"
            } else {
                "  "
            };

            let label_len = visible_width(&item.label);
            let pad = if max_label_width > label_len {
                " ".repeat(max_label_width - label_len)
            } else {
                String::new()
            };

            let label_styled = if is_selected {
                format!("\x1b[1m{}{pad}\x1b[0m", item.label)
            } else {
                format!("{}{pad}", item.label)
            };

            let value_styled = if is_selected {
                format!("\x1b[36m{}\x1b[0m", item.current_value)
            } else {
                format!("\x1b[2m{}\x1b[0m", item.current_value)
            };

            let row = format!("{prefix}{label_styled}  {value_styled}");
            lines.push(truncate_to_width(&row, width, "...", false));
        }

        if total > max_vis {
            lines.push(truncate_to_width(
                &format!("  \x1b[2m({}/{})\x1b[0m", selected + 1, total),
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
                lines.push(truncate_to_width(&format!("  \x1b[2m{w}\x1b[0m"), width, "...", false));
            }
        }

        lines.push(String::new());
        lines.push(truncate_to_width(
            "  \x1b[2mEnter/Space to change · Esc to close\x1b[0m",
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
    fn test_settings_list_render_and_cycle() {
        let items = vec![
            SettingItem::new("theme", "Color theme", "dark")
                .with_values(vec!["dark".into(), "light".into()])
                .with_description("Active color theme for the TUI"),
            SettingItem::new("mode", "Queue mode", "none")
                .with_values(vec!["none".into(), "all".into(), "one_at_a_time".into()]),
        ];
        let mut list = SettingsList::new(items, 5);

        let lines = list.render(60);
        assert!(lines[0].contains("> "));
        assert!(lines[0].contains("Color theme"));
        assert!(lines[0].contains("dark"));
        assert!(lines.iter().any(|l| l.contains("Active color theme for the TUI")));

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
        let items = vec![
            SettingItem::new("theme", "Color theme", "dark")
                .with_values(vec!["dark".into(), "light".into()]),
        ];
        let mut list = SettingsList::new(items, 5);
        list.selected = 100;
        let lines = list.render(60);
        assert!(!lines.is_empty());
        assert!(lines[0].contains("> "));
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
        assert!(lines[2].contains("> "));
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
