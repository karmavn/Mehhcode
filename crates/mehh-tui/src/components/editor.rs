use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const MAX_UNDO_SNAPSHOTS: usize = 100;
const TYPING_COALESCE_WINDOW: Duration = Duration::from_millis(500);

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

        if accumulate {
            if let Some(last) = self.ring.pop() {
                let merged = if prepend {
                    format!("{text}{last}")
                } else {
                    format!("{last}{text}")
                };
                self.ring.push(merged);
                return;
            }
        }
        self.ring.push(text.to_string());
        if self.ring.len() > self.max_entries {
            self.ring.remove(0);
        }
    }

    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(|s| s.as_str())
    }

    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            self.ring.rotate_right(1);
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
    let mut offset = 0;
    while let Some(start_byte) = line[offset..].find("[paste #") {
        let abs_start_byte = offset + start_byte;
        if let Some(end_bracket) = line[abs_start_byte..].find(']') {
            let abs_end_byte = abs_start_byte + end_bracket + 1;
            let after_prefix = &line[abs_start_byte + 8..abs_end_byte - 1];
            let id_len = after_prefix
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if id_len > 0 {
                let start_col = line[..abs_start_byte].chars().count();
                let marker_len = line[abs_start_byte..abs_end_byte].chars().count();
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

fn split_char_at(s: &str, char_idx: usize) -> (&str, &str) {
    match s.char_indices().nth(char_idx) {
        Some((byte_idx, _)) => (&s[..byte_idx], &s[byte_idx..]),
        None => (s, ""),
    }
}

fn normalize_line_endings(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
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
    history_draft: Option<(Vec<String>, usize, usize)>,
    pub undo_stack: VecDeque<(Vec<String>, usize, usize)>,
    last_typing_undo: Option<(Instant, usize, usize)>,
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
            history_draft: None,
            undo_stack: VecDeque::new(),
            last_typing_undo: None,
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
        self.render_autocomplete_with_theme(width, &self.theme).0
    }

    /// Render autocomplete rows with an explicit `theme` without cloning the `Editor` or `SelectList`,
    /// returning `(lines, vis_start, vis_end, total_filtered)`.
    pub fn render_autocomplete_with_theme(
        &self,
        width: usize,
        theme: &Theme,
    ) -> (Vec<String>, usize, usize, usize) {
        let Some(ac) = self.autocomplete.as_ref() else {
            return (Vec::new(), 0, 0, 0);
        };
        let filtered = ac.filtered_indices();
        let total = filtered.len();
        let (lines, vis_start, vis_end) = ac.render_with_filtered(width, theme, ac.max_visible, &filtered);
        (lines, vis_start, vis_end, total)
    }

    /// Colorize collapsed paste tokens so they read as one atomic chip.
    fn colorize_paste_markers_with_theme(&self, line: &str, theme: &Theme) -> String {
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
                    out.push_str(&theme.fg(&theme.accent, &tail[..=end]));
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

    /// Renders editor lines with an explicit `theme` and `max_lines` without cloning `Editor` (history, undo stack, or paste payloads).
    pub fn render_with_options(&self, width: usize, theme: &Theme, max_lines: usize) -> Vec<String> {
        let mut result = Vec::new();

        if self.is_empty() && !self.placeholder.is_empty() {
            let line = format!(
                "{}{CURSOR_MARKER}{}",
                self.prompt_prefix,
                theme.fg(&theme.dim, &self.placeholder)
            );
            for w in wrap_text_with_ansi(&line, width) {
                result.push(w);
            }
        } else {
            for (row, line_str) in self.lines.iter().enumerate() {
                let pref = if row == 0 { &self.prompt_prefix } else { "  " };
                let formatted = if row == self.cursor_row {
                    let (b, a) = split_char_at(line_str, self.cursor_col);
                    let b = crate::ansi::sanitize_terminal_text(b, false);
                    let a = crate::ansi::sanitize_terminal_text(a, false);
                    format!("{pref}{b}{CURSOR_MARKER}{a}")
                } else {
                    let clean_line = crate::ansi::sanitize_terminal_text(line_str, false);
                    format!("{pref}{clean_line}")
                };
                let formatted = self.colorize_paste_markers_with_theme(&formatted, theme);

                for w in wrap_text_with_ansi(&formatted, width) {
                    result.push(w);
                }
            }
        }

        let max_lines = max_lines.max(1);
        if max_lines != usize::MAX && result.len() > max_lines {
            let marker_row = result
                .iter()
                .position(|line| line.contains(CURSOR_MARKER))
                .unwrap_or(0);
            let start = marker_row.saturating_sub(max_lines / 2);
            let end = (start + max_lines).min(result.len());
            let start = end.saturating_sub(max_lines);
            result = result.into_iter().skip(start).take(end - start).collect();
        }

        result
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() <= 1 && self.lines.first().map(|l| l.is_empty()).unwrap_or(true)
    }

    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.last_action = None;
        let normalized = normalize_line_endings(text);
        self.lines = normalized.split('\n').map(str::to_string).collect();
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
                let id_end = marker_body
                    .as_bytes()
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if let Ok(id) = marker_body[..id_end].parse::<usize>() {
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
        self.history_idx = None;
        self.history_draft = None;
        self.undo_stack.clear();
        self.clear_autocomplete();
        self.last_action = None;
        self.last_typing_undo = None;
    }

    /// Pop the most recent history entry if `pred` returns `true` for it.
    pub fn scrub_last_history_if<F: FnOnce(&str) -> bool>(&mut self, pred: F) {
        if let Some(last) = self.history.last() {
            if pred(last) {
                self.history.pop();
                if self.history_idx.is_some_and(|idx| idx >= self.history.len()) {
                    self.history_idx = None;
                }
            }
        }
    }

    pub fn push_undo(&mut self) {
        self.last_typing_undo = None;
        self.undo_stack
            .push_back((self.lines.clone(), self.cursor_row, self.cursor_col));
        if self.undo_stack.len() > MAX_UNDO_SNAPSHOTS {
            self.undo_stack.pop_front();
        }
    }

    pub fn undo(&mut self) -> bool {
        if let Some((lines, row, col)) = self.undo_stack.pop_back() {
            self.lines = lines;
            self.cursor_row = row.min(self.lines.len().saturating_sub(1));
            self.cursor_col = col.min(self.lines[self.cursor_row].chars().count());
            self.last_action = None;
            self.last_typing_undo = None;
            true
        } else {
            false
        }
    }

    /// Inserts a single typed character, coalescing consecutive word characters
    /// within [`TYPING_COALESCE_WINDOW`] so typing does not deep-clone the buffer on every key.
    fn insert_typed_char(&mut self, key: &str) {
        let Some(ch) = key.chars().next() else {
            return;
        };
        let is_word_char = !ch.is_whitespace() && !is_punctuation_char(ch);
        let can_coalesce = is_word_char
            && self.last_typing_undo.is_some_and(|(ts, row, col)| {
                row == self.cursor_row
                    && col == self.cursor_col
                    && ts.elapsed() <= TYPING_COALESCE_WINDOW
            });
        if !can_coalesce {
            self.push_undo();
        }
        self.insert_text_internal(key);
        self.last_typing_undo = if is_word_char {
            Some((Instant::now(), self.cursor_row, self.cursor_col))
        } else {
            None
        };
    }

    /// Insert text at cursor, collapsing multi-line pastes > 10 lines into `[paste #N +L lines]`.
    pub fn insert_text(&mut self, text: &str) {
        self.push_undo();
        self.last_action = None;
        let normalized = normalize_line_endings(text);
        let pasted_line_count = normalized.split('\n').count();

        if pasted_line_count > 10 || normalized.len() > 1000 {
            self.paste_counter += 1;
            let paste_id = self.paste_counter;
            self.pastes.insert(paste_id, normalized.into_owned());
            let marker = format!("[paste #{paste_id} +{pasted_line_count} lines]");
            self.insert_text_internal(&marker);
        } else {
            self.insert_text_internal(&normalized);
        }
    }

    fn insert_text_internal(&mut self, text: &str) {
        let normalized = normalize_line_endings(text);
        let input_lines: Vec<&str> = normalized.split('\n').collect();

        if input_lines.len() <= 1 {
            let single = input_lines.first().copied().unwrap_or("");
            let updated = {
                let cur = &self.lines[self.cursor_row];
                let (before, after) = split_char_at(cur, self.cursor_col);
                format!("{before}{single}{after}")
            };
            self.lines[self.cursor_row] = updated;
            self.cursor_col += single.chars().count();
        } else {
            let last_idx = input_lines.len() - 1;
            let last_line = input_lines[last_idx];
            let (first, last) = {
                let cur = &self.lines[self.cursor_row];
                let (before, after) = split_char_at(cur, self.cursor_col);
                (
                    format!("{before}{}", input_lines[0]),
                    format!("{last_line}{after}"),
                )
            };
            self.lines[self.cursor_row] = first;

            for line in &input_lines[1..last_idx] {
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, line.to_string());
            }

            self.cursor_row += 1;
            self.lines.insert(self.cursor_row, last);
            self.cursor_col = last_line.chars().count();
        }
    }

    pub fn set_autocomplete(&mut self, items: Vec<SelectItem>) {
        self.autocomplete = Some(SelectList::new_with_theme(items, 5, self.theme.clone()));
        self.autocomplete_replace_start = None;
    }

    /// Sets autocomplete items that replace `[replace_start..cursor_col]` on the active line.
    pub fn set_autocomplete_replacing(&mut self, items: Vec<SelectItem>, replace_start: usize) {
        self.autocomplete = Some(SelectList::new_with_theme(items, 5, self.theme.clone()));
        self.autocomplete_replace_start = Some(replace_start);
    }

    pub fn clear_autocomplete(&mut self) {
        self.autocomplete = None;
        self.autocomplete_replace_start = None;
    }

    fn replace_current_line_range(&mut self, start_col: usize, replacement: &str) {
        self.push_undo();
        self.last_action = None;
        let line = &self.lines[self.cursor_row];
        let char_len = line.chars().count();
        let start = start_col.min(self.cursor_col).min(char_len);
        let end = self.cursor_col.min(char_len);
        let updated = {
            let line = &self.lines[self.cursor_row];
            let (before, _) = split_char_at(line, start);
            let (_, after) = split_char_at(line, end);
            format!("{before}{replacement}{after}")
        };
        self.lines[self.cursor_row] = updated;
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
        let target = {
            let cur = &self.lines[self.cursor_row];
            let target = find_word_backward(cur, self.cursor_col);
            let (before, rest) = split_char_at(cur, target);
            let (deleted, after) = split_char_at(rest, self.cursor_col - target);
            self.kill_ring.push(deleted, true, was_kill);
            let updated = format!("{before}{after}");
            self.lines[self.cursor_row] = updated;
            target
        };
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
        let updated = {
            let cur = &self.lines[self.cursor_row];
            let target = find_word_forward(cur, self.cursor_col);
            let (before, rest) = split_char_at(cur, self.cursor_col);
            let (deleted, after) = split_char_at(rest, target - self.cursor_col);
            self.kill_ring.push(deleted, false, was_kill);
            format!("{before}{after}")
        };
        self.lines[self.cursor_row] = updated;
        self.last_action = Some(EditorLastAction::Kill);
    }

    fn kill_to_end(&mut self) {
        self.push_undo();
        let was_kill = self.last_action == Some(EditorLastAction::Kill);
        let keep_len = {
            let cur = &self.lines[self.cursor_row];
            let (before, after) = split_char_at(cur, self.cursor_col);
            if !after.is_empty() {
                self.kill_ring.push(after, false, was_kill);
                Some(before.len())
            } else {
                None
            }
        };

        if let Some(len) = keep_len {
            self.lines[self.cursor_row].truncate(len);
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
        let drop_len = {
            let cur = &self.lines[self.cursor_row];
            let (before, _) = split_char_at(cur, self.cursor_col);
            if !before.is_empty() {
                self.kill_ring.push(before, true, was_kill);
                Some(before.len())
            } else {
                None
            }
        };

        if let Some(len) = drop_len {
            self.lines[self.cursor_row].replace_range(..len, "");
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
        let normalized = normalize_line_endings(text);
        let lines: Vec<&str> = normalized.split('\n').collect();
        if lines.is_empty() {
            return;
        }

        if lines.len() == 1 {
            let single = lines[0];
            let updated = {
                let cur = &self.lines[self.cursor_row];
                let (before, after) = split_char_at(cur, self.cursor_col);
                format!("{before}{single}{after}")
            };
            self.lines[self.cursor_row] = updated;
            self.cursor_col += single.chars().count();
        } else {
            let last_idx = lines.len() - 1;
            let last_line = lines[last_idx];
            let (first, last) = {
                let cur = &self.lines[self.cursor_row];
                let (before, after) = split_char_at(cur, self.cursor_col);
                (format!("{before}{}", lines[0]), format!("{last_line}{after}"))
            };
            self.lines[self.cursor_row] = first;

            for line in &lines[1..last_idx] {
                self.cursor_row += 1;
                self.lines.insert(self.cursor_row, line.to_string());
            }

            self.cursor_row += 1;
            self.lines.insert(self.cursor_row, last);
            self.cursor_col = last_line.chars().count();
        }
    }

    fn delete_yanked_text(&mut self) {
        let yanked_text = match self.kill_ring.peek() {
            Some(t) => t.to_string(),
            None => return,
        };

        let normalized = normalize_line_endings(&yanked_text);
        let yank_lines: Vec<&str> = normalized.split('\n').collect();
        if yank_lines.is_empty() {
            return;
        }

        if yank_lines.len() == 1 {
            let delete_len = yank_lines[0].chars().count();
            if self.cursor_col >= delete_len {
                let updated = {
                    let current_line = &self.lines[self.cursor_row];
                    let (before, rest) =
                        split_char_at(current_line, self.cursor_col - delete_len);
                    let (_, after) = split_char_at(rest, delete_len);
                    format!("{before}{after}")
                };
                self.lines[self.cursor_row] = updated;
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
            let splice_end = (start_line + drain_count).min(self.lines.len());
            let merged = format!("{before_yank}{after_cursor}");
            self.lines.splice(
                start_line..splice_end,
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
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        self.push_undo();
        if self.history_idx.is_none() {
            self.history_draft = Some((self.lines.clone(), self.cursor_row, self.cursor_col));
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
        self.last_action = None;
        if let Some(i) = self.history_idx {
            self.push_undo();
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
                if let Some((lines, row, col)) = self.history_draft.take() {
                    self.lines = lines;
                    self.cursor_row = row;
                    self.cursor_col = col;
                } else {
                    self.lines = vec![String::new()];
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                }
            }
        }
    }

    /// Process a key event and return `EditorOutput`.
    pub fn handle_editor_key(&mut self, key: &str, bindings: &Keybindings) -> EditorOutput {
        if let Some(ac) = self.autocomplete.as_mut() {
            match key {
                "up" => {
                    ac.move_up();
                    return EditorOutput::None;
                }
                "down" => {
                    ac.move_down();
                    return EditorOutput::None;
                }
                "pageup" => {
                    ac.page_up();
                    return EditorOutput::None;
                }
                "pagedown" => {
                    ac.page_down();
                    return EditorOutput::None;
                }
                "enter" | "tab" => {
                    if let Some(item) = ac.selected_item().cloned() {
                        let replace_start = self.autocomplete_replace_start;
                        self.clear_autocomplete();
                        self.last_action = None;
                        if let Some(start) = replace_start {
                            self.replace_current_line_range(start, &item.id);
                        } else {
                            self.insert_text(&item.label);
                        }
                        return EditorOutput::Changed(self.text());
                    } else {
                        self.clear_autocomplete();
                        return EditorOutput::None;
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
            self.last_action = None;
            let (before_len, after_owned) = {
                let cur = &self.lines[self.cursor_row];
                let (before, after) = split_char_at(cur, self.cursor_col);
                (before.len(), after.to_string())
            };
            self.lines[self.cursor_row].truncate(before_len);
            self.lines.insert(self.cursor_row + 1, after_owned);
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
                    let (del_start, del_end) = {
                        let line = &self.lines[self.cursor_row];
                        let (before, rest) = split_char_at(line, m.start_col);
                        let (_, after) = split_char_at(rest, m.end_col - m.start_col);
                        (before.len(), line.len() - after.len())
                    };
                    self.lines[self.cursor_row].replace_range(del_start..del_end, "");
                    self.cursor_col = m.start_col;
                } else {
                    if let Some((byte_start, ch)) = self.lines[self.cursor_row]
                        .char_indices()
                        .nth(self.cursor_col - 1)
                    {
                        let byte_end = byte_start + ch.len_utf8();
                        self.lines[self.cursor_row].replace_range(byte_start..byte_end, "");
                    }
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
                let markers = find_paste_markers(&self.lines[self.cursor_row]);
                if let Some(m) = markers.iter().find(|m| m.start_col == self.cursor_col) {
                    let (del_start, del_end) = {
                        let line = &self.lines[self.cursor_row];
                        let (before, rest) = split_char_at(line, m.start_col);
                        let (_, after) = split_char_at(rest, m.end_col - m.start_col);
                        (before.len(), line.len() - after.len())
                    };
                    self.lines[self.cursor_row].replace_range(del_start..del_end, "");
                } else if let Some((byte_start, ch)) = self.lines[self.cursor_row]
                    .char_indices()
                    .nth(self.cursor_col)
                {
                    let byte_end = byte_start + ch.len_utf8();
                    self.lines[self.cursor_row].replace_range(byte_start..byte_end, "");
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
        if key.chars().count() == 1 {
            self.last_action = Some(EditorLastAction::Other);
            self.insert_typed_char(key);
            return EditorOutput::Changed(self.text());
        }

        EditorOutput::None
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_with_options(width, &self.theme, self.max_lines)
    }

    fn handle_key(&mut self, key: &str) -> bool {
        static DEFAULT_BINDINGS: std::sync::LazyLock<Keybindings> =
            std::sync::LazyLock::new(Keybindings::default_bindings);
        matches!(
            self.handle_editor_key(key, &DEFAULT_BINDINGS),
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

    #[test]
    fn plus_key_inserts_plus_character() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();
        let out = editor.handle_editor_key("+", &kb);
        assert!(matches!(out, EditorOutput::Changed(_)), "output: {out:?}");
        assert_eq!(editor.text(), "+");
    }

    #[test]
    fn enter_with_empty_autocomplete_does_not_submit() {
        let mut editor = Editor::new();
        editor.set_text("draft");
        editor.set_autocomplete(Vec::new());
        let kb = Keybindings::default_bindings();

        let out = editor.handle_editor_key("enter", &kb);
        assert!(!matches!(out, EditorOutput::Submit(_)), "output: {out:?}");
        assert!(editor.autocomplete.is_none());
    }

    #[test]
    fn history_recall_restores_in_progress_draft() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();
        editor.history.push("old entry".to_string());
        for c in "draft".chars() {
            editor.handle_editor_key(&c.to_string(), &kb);
        }
        assert_eq!(editor.text(), "draft");

        let _ = editor.handle_editor_key("up", &kb);
        assert_eq!(editor.text(), "old entry");

        let _ = editor.handle_editor_key("down", &kb);
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn typing_coalesces_word_chars_into_single_undo_and_caps_stack() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();

        for ch in "hello".chars() {
            editor.handle_editor_key(&ch.to_string(), &kb);
        }
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.undo_stack.len(), 1);

        editor.handle_editor_key(" ", &kb);
        for ch in "world".chars() {
            editor.handle_editor_key(&ch.to_string(), &kb);
        }
        assert_eq!(editor.text(), "hello world");
        assert_eq!(editor.undo_stack.len(), 3);

        assert!(editor.undo());
        assert_eq!(editor.text(), "hello ");

        for i in 0..(MAX_UNDO_SNAPSHOTS + 25) {
            editor.set_text(&format!("v{i}"));
        }
        assert_eq!(editor.undo_stack.len(), MAX_UNDO_SNAPSHOTS);
    }

    #[test]
    fn cr_and_crlf_line_endings_normalize_to_newlines() {
        let mut editor = Editor::new();
        editor.set_text("a\rb\r\nc");
        assert_eq!(editor.lines, vec!["a", "b", "c"]);
        assert_eq!(editor.text(), "a\nb\nc");

        editor.clear();
        editor.insert_text("one\rtwo\r\nthree");
        assert_eq!(editor.lines, vec!["one", "two", "three"]);
        assert_eq!(editor.text(), "one\ntwo\nthree");

        // Large CR-delimited paste collapses properly (>10 lines)
        editor.clear();
        let big_cr = (1..=12).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\r");
        editor.insert_text(&big_cr);
        assert!(editor.lines[0].contains("[paste #1 +12 lines]"));
        assert!(!editor.text().contains('\r'));

        // Yank with CR normalizes across lines
        editor.clear();
        editor.kill_ring.push("x\ry\r\nz", false, false);
        assert!(editor.yank());
        assert_eq!(editor.lines, vec!["x", "y", "z"]);
    }

    #[test]
    fn clear_wipes_undo_stack_so_secrets_cannot_be_resurrected() {
        let mut editor = Editor::new();
        editor.insert_text("/login openai sk-secret-token");
        assert!(!editor.undo_stack.is_empty());

        editor.clear();
        assert!(editor.undo_stack.is_empty());
        assert!(!editor.undo());
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn scrub_last_history_if_pops_only_when_predicate_matches() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();

        editor.set_text("normal prompt");
        let _ = editor.handle_editor_key("enter", &kb);
        editor.set_text("/login openai sk-secret");
        let _ = editor.handle_editor_key("enter", &kb);
        assert_eq!(editor.history.len(), 2);

        editor.scrub_last_history_if(|entry| entry.starts_with("/login "));
        assert_eq!(editor.history, vec!["normal prompt".to_string()]);

        // Predicate false -> preserves remaining entry
        editor.scrub_last_history_if(|entry| entry.starts_with("/login "));
        assert_eq!(editor.history, vec!["normal prompt".to_string()]);
    }

    #[test]
    fn render_with_options_strips_escape_and_bidi_sequences_preserving_cursor_and_paste_chip() {
        let theme = Theme::dark();
        let mut editor = Editor::new().with_prefix("> ");
        let big_paste = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        editor.insert_text(&big_paste);
        editor.insert_text(" \x1b]52;c;ZHVtbXk=\x07\x1b[31mred\x1b_Gevil\x1b\\\u{202e}tail");
        // Insert a second line (non-cursor row when cursor moves back to row 0, or keep row 0 non-cursor)
        editor.insert_text("\n\x1b]0;title\x07\x1b[32mnext\u{202e}line");
        // Place cursor on row 1 between "next" and "line"
        editor.cursor_row = 1;
        editor.cursor_col = editor.lines[1].chars().count() - 4;

        let rendered = editor.render_with_options(120, &theme, 10);
        let joined = rendered.join("\n");

        assert!(!joined.contains("\x1b]52;"), "OSC 52 leaked: {joined:?}");
        assert!(!joined.contains("\x1b]0;"), "OSC 0 leaked: {joined:?}");
        assert!(!joined.contains("\x1b[31m"), "CSI color leaked: {joined:?}");
        assert!(!joined.contains("\x1b[32m"), "CSI color leaked: {joined:?}");
        assert!(!joined.contains("\x1b_Gevil"), "APC leaked: {joined:?}");
        assert!(!joined.contains('\u{202e}'), "Bidi override leaked: {joined:?}");

        assert!(joined.contains(CURSOR_MARKER), "CURSOR_MARKER missing: {joined:?}");
        let expected_chip = theme.fg(&theme.accent, "[paste #1 +12 lines]");
        assert!(joined.contains(&expected_chip),
            "colorized paste chip missing: {joined:?}"
        );
        assert!(joined.contains("redtail"), "sanitized text missing: {joined:?}");
    }

    #[test]
    fn c1_ctrl_y_up_alt_y_three_line_yank_one_line_history_does_not_panic() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();
        editor.kill_ring.push("older", false, false);
        editor.kill_ring.push("line1\nline2\nline3", false, false);
        editor.history.push("single line history".to_string());

        // Ctrl+Y -> yanks 3 lines
        let out = editor.handle_editor_key("ctrl+y", &kb);
        assert!(matches!(out, EditorOutput::Changed(_)));
        assert_eq!(editor.lines.len(), 3);

        // Ctrl+P (HistoryPrev) jumps straight to 1-line history; Alt+Y must not panic or yank_pop
        let _ = editor.handle_editor_key("ctrl+p", &kb);
        assert_eq!(editor.text(), "single line history");
        let pop_out = editor.handle_editor_key("alt+y", &kb);
        assert_eq!(pop_out, EditorOutput::None);
        assert_eq!(editor.text(), "single line history");

        // Also test Ctrl+Y -> Up -> Up -> Up -> Alt+Y
        editor.clear();
        let _ = editor.handle_editor_key("ctrl+y", &kb);
        assert_eq!(editor.cursor_row, 2);
        let _ = editor.handle_editor_key("up", &kb);
        let _ = editor.handle_editor_key("up", &kb);
        let _ = editor.handle_editor_key("up", &kb);
        assert_eq!(editor.text(), "single line history");
        let pop_out2 = editor.handle_editor_key("alt+y", &kb);
        assert_eq!(pop_out2, EditorOutput::None);
        assert_eq!(editor.text(), "single line history");

        // Direct delete_yanked_text clamp when buffer has fewer lines than yank
        editor.clear();
        assert!(editor.yank());
        editor.lines = vec!["only".to_string()];
        editor.cursor_row = 0;
        editor.cursor_col = 4;
        assert!(editor.yank_pop());
    }

    #[test]
    fn c1_clear_keeps_paste_counter_monotonic() {
        let mut editor = Editor::new();
        let big1 = (1..=12).map(|i| format!("a{i}")).collect::<Vec<_>>().join("\n");
        let big2 = (1..=12).map(|i| format!("b{i}")).collect::<Vec<_>>().join("\n");
        editor.insert_text(&big1);
        assert!(editor.lines[0].contains("[paste #1 +12 lines]"));

        editor.clear();
        editor.insert_text(&big2);
        assert!(
            editor.lines[0].contains("[paste #2 +12 lines]"),
            "paste_counter must remain monotonic across clear(): {:?}",
            editor.lines
        );
    }

    #[test]
    fn c1_history_next_pushes_undo_and_preserves_undo_and_pastes() {
        let mut editor = Editor::new();
        let big = (1..=12).map(|i| format!("p{i}")).collect::<Vec<_>>().join("\n");
        editor.insert_text(&big);
        editor.history.push("hist1".to_string());

        editor.history_prev();
        assert_eq!(editor.text(), "hist1");
        let undo_before_next = editor.undo_stack.len();

        // Clear history_draft to exercise the fallback branch of history_next()
        editor.history_draft = None;
        editor.history_next();
        assert_eq!(editor.text(), "");
        assert_eq!(
            editor.undo_stack.len(),
            undo_before_next + 1,
            "history_next must push undo instead of clearing undo_stack"
        );
        assert!(editor.undo());
        assert_eq!(editor.text(), "hist1");
        assert!(editor.undo());
        assert_eq!(editor.text(), big, "pastes map must survive history_next");
    }

    #[test]
    fn c1_last_action_reset_on_edits_and_navigation() {
        let mut editor = Editor::new();
        let kb = Keybindings::default_bindings();
        editor.kill_ring.push("a", false, false);
        editor.kill_ring.push("b", false, false);
        editor.history.push("h".to_string());

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.set_text("x");
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.insert_text("y");
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.replace_current_line_range(0, "z");
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.history_prev();
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.history_next();
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        let _ = editor.handle_editor_key("shift+enter", &kb);
        assert_eq!(editor.last_action, None);

        editor.yank();
        assert_eq!(editor.last_action, Some(EditorLastAction::Yank));
        editor.set_autocomplete(vec![SelectItem::new("id", "label")]);
        let _ = editor.handle_editor_key("enter", &kb);
        assert_eq!(editor.last_action, None);
    }
}
