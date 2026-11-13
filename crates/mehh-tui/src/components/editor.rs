use std::collections::HashMap;

use crate::ansi::wrap_text_with_ansi;
use crate::components::basic::{SelectItem, SelectList};
use crate::keys::{Keybindings, TuiAction};
use crate::tui::{Component, CURSOR_MARKER};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorOutput {
    None,
    Submit(String),
    Changed(String),
}

fn split_char_at(s: &str, char_idx: usize) -> (String, String) {
    let chars: Vec<char> = s.chars().collect();
    let split = char_idx.min(chars.len());
    let before: String = chars[..split].iter().collect();
    let after: String = chars[split..].iter().collect();
    (before, after)
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
    pub kill_ring: String,
    pub autocomplete: Option<SelectList>,
    pub prompt_prefix: String,
    pub placeholder: String,
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
            kill_ring: String::new(),
            autocomplete: None,
            prompt_prefix: "> ".to_string(),
            placeholder: String::new(),
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
                let id_str: String = marker_body.chars().take_while(|c| c.is_ascii_digit()).collect();
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
    }

    pub fn push_undo(&mut self) {
        self.undo_stack.push((self.lines.clone(), self.cursor_row, self.cursor_col));
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
    }

    pub fn undo(&mut self) -> bool {
        if let Some((lines, row, col)) = self.undo_stack.pop() {
            self.lines = lines;
            self.cursor_row = row.min(self.lines.len().saturating_sub(1));
            self.cursor_col = col.min(self.lines[self.cursor_row].chars().count());
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
        self.autocomplete = Some(SelectList::new(items, 5));
    }

    pub fn clear_autocomplete(&mut self) {
        self.autocomplete = None;
    }

    fn move_word_left(&mut self) {
        let cur = &self.lines[self.cursor_row];
        let chars: Vec<char> = cur.chars().collect();
        let mut pos = self.cursor_col;

        while pos > 0 && chars.get(pos - 1).map(|c| c.is_whitespace()).unwrap_or(false) {
            pos -= 1;
        }
        while pos > 0 && !chars.get(pos - 1).map(|c| c.is_whitespace()).unwrap_or(true) {
            pos -= 1;
        }
        self.cursor_col = pos;
    }

    fn move_word_right(&mut self) {
        let cur = &self.lines[self.cursor_row];
        let chars: Vec<char> = cur.chars().collect();
        let len = chars.len();
        let mut pos = self.cursor_col;

        while pos < len && !chars[pos].is_whitespace() {
            pos += 1;
        }
        while pos < len && chars[pos].is_whitespace() {
            pos += 1;
        }
        self.cursor_col = pos;
    }

    fn delete_word_backward(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                let current_line = self.lines.remove(self.cursor_row);
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
                self.lines[self.cursor_row].push_str(&current_line);
            }
            return;
        }

        self.push_undo();
        let cur = &self.lines[self.cursor_row];
        let chars: Vec<char> = cur.chars().collect();
        let mut target = self.cursor_col;

        while target > 0 && chars.get(target - 1).map(|c| c.is_whitespace()).unwrap_or(false) {
            target -= 1;
        }
        while target > 0 && !chars.get(target - 1).map(|c| c.is_whitespace()).unwrap_or(true) {
            target -= 1;
        }

        let mut new_chars = Vec::new();
        new_chars.extend_from_slice(&chars[..target]);
        new_chars.extend_from_slice(&chars[self.cursor_col..]);
        self.lines[self.cursor_row] = new_chars.into_iter().collect();
        self.cursor_col = target;
    }

    fn kill_to_end(&mut self) {
        self.push_undo();
        let cur = &self.lines[self.cursor_row];
        let (before, after) = split_char_at(cur, self.cursor_col);
        self.kill_ring = after;
        self.lines[self.cursor_row] = before;
    }

    fn kill_to_start(&mut self) {
        self.push_undo();
        let cur = &self.lines[self.cursor_row];
        let (before, after) = split_char_at(cur, self.cursor_col);
        self.kill_ring = before;
        self.lines[self.cursor_row] = after;
        self.cursor_col = 0;
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
                "enter" | "tab" => {
                    if let Some(item) = self
                        .autocomplete
                        .as_ref()
                        .and_then(|ac| ac.selected_item().cloned())
                    {
                        self.clear_autocomplete();
                        self.insert_text(&item.label);
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
            if self.cursor_col > 0 {
                self.cursor_col -= 1;
            } else if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].chars().count();
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::CursorRight) {
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
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.cursor_col.min(self.lines[self.cursor_row].chars().count());
                return EditorOutput::None;
            } else {
                self.history_prev();
                return EditorOutput::Changed(self.text());
            }
        }

        if bindings.matches(key, TuiAction::CursorDown) {
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = self.cursor_col.min(self.lines[self.cursor_row].chars().count());
                return EditorOutput::None;
            } else {
                self.history_next();
                return EditorOutput::Changed(self.text());
            }
        }

        if bindings.matches(key, TuiAction::LineStart) {
            self.cursor_col = 0;
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::LineEnd) {
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
            if self.cursor_col > 0 {
                self.push_undo();
                let line = &self.lines[self.cursor_row];
                let chars: Vec<char> = line.chars().collect();
                let mut new_chars = Vec::with_capacity(chars.len());
                new_chars.extend_from_slice(&chars[..self.cursor_col - 1]);
                new_chars.extend_from_slice(&chars[self.cursor_col..]);
                self.lines[self.cursor_row] = new_chars.into_iter().collect();
                self.cursor_col -= 1;
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
            let len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col < len {
                self.push_undo();
                let line = &self.lines[self.cursor_row];
                let chars: Vec<char> = line.chars().collect();
                let mut new_chars = Vec::with_capacity(chars.len());
                new_chars.extend_from_slice(&chars[..self.cursor_col]);
                new_chars.extend_from_slice(&chars[self.cursor_col + 1..]);
                self.lines[self.cursor_row] = new_chars.into_iter().collect();
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

        if bindings.matches(key, TuiAction::KillToEnd) {
            self.kill_to_end();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::KillToStart) {
            self.kill_to_start();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::Yank) {
            if !self.kill_ring.is_empty() {
                let text = self.kill_ring.clone();
                self.insert_text(&text);
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::Undo) {
            if self.undo() {
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        if bindings.matches(key, TuiAction::HistoryPrev) {
            self.history_prev();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::HistoryNext) {
            self.history_next();
            return EditorOutput::Changed(self.text());
        }

        if bindings.matches(key, TuiAction::Interrupt) {
            if !self.is_empty() {
                self.clear();
                return EditorOutput::Changed(self.text());
            }
            return EditorOutput::None;
        }

        // Single character input
        if key.chars().count() == 1 && !key.contains('+') {
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
                "{}{CURSOR_MARKER}\x1b[2m{}\x1b[0m",
                self.prompt_prefix, self.placeholder
            );
            for w in wrap_text_with_ansi(&line, width) {
                result.push(w);
            }
        } else {
            for (row, line_str) in self.lines.iter().enumerate() {
                let pref = if row == 0 {
                    &self.prompt_prefix
                } else {
                    "  "
                };
                let formatted = if row == self.cursor_row {
                    let (b, a) = split_char_at(line_str, self.cursor_col);
                    format!("{pref}{b}{CURSOR_MARKER}{a}")
                } else {
                    format!("{pref}{line_str}")
                };

                for w in wrap_text_with_ansi(&formatted, width) {
                    result.push(w);
                }
            }
        }

        if let Some(ref ac) = self.autocomplete {
            let ac_lines = ac.render(width);
            for l in ac_lines {
                result.push(l);
            }
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
