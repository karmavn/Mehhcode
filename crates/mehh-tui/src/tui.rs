use crate::ansi::{slice_by_column, visible_width};

pub const CURSOR_MARKER: &str = "\x1b_mehh:c\x07";

/// Trait implemented by interactive visual elements.
pub trait Component: Send + Sync {
    fn render(&self, width: usize) -> Vec<String>;
    fn handle_key(&mut self, _key: &str) -> bool {
        false
    }
    fn invalidate(&mut self) {}
}

/// Abstract terminal interface for rendering and testing.
pub trait Terminal: Send {
    fn size(&self) -> (u16, u16);
    fn write(&mut self, data: &str) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()>;
}

/// Real crossterm-based terminal that manages raw mode and writes to stdout.
pub struct CrosstermTerminal {
    raw_mode_enabled: bool,
}

impl CrosstermTerminal {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        );
        Ok(Self {
            raw_mode_enabled: true,
        })
    }
}

impl Drop for CrosstermTerminal {
    fn drop(&mut self) {
        if self.raw_mode_enabled {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste
            );
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

impl Terminal for CrosstermTerminal {
    fn size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    fn write(&mut self, data: &str) -> std::io::Result<()> {
        use std::io::Write;
        std::io::stdout().write_all(data.as_bytes())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        std::io::stdout().flush()
    }
}

/// Headless terminal implementation with configurable dimensions and an output buffer.
#[derive(Debug, Clone)]
pub struct MockTerminal {
    pub cols: u16,
    pub rows: u16,
    pub output: Vec<String>,
}

impl MockTerminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            output: Vec::new(),
        }
    }

    pub fn full_output(&self) -> String {
        self.output.join("")
    }

    pub fn clear_output(&mut self) {
        self.output.clear();
    }
}

impl Terminal for MockTerminal {
    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    fn write(&mut self, data: &str) -> std::io::Result<()> {
        self.output.push(data.to_string());
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Layout options for compositing overlays (such as autocomplete popups).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayOptions {
    pub col: usize,
    pub row: usize,
    pub width: usize,
    pub max_height: Option<usize>,
}

/// Composite an overlay line onto a base line at column `col`, preserving styling.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    col: usize,
    total_width: usize,
) -> String {
    let overlay_width = visible_width(overlay_line);
    let start_col = col.min(total_width);
    let after_start = (start_col + overlay_width).min(total_width);

    let before = slice_by_column(base_line, 0, start_col);
    let before_w = visible_width(&before);
    let before_pad = start_col.saturating_sub(before_w);

    let overlay_slice = if start_col + overlay_width > total_width {
        slice_by_column(overlay_line, 0, total_width.saturating_sub(start_col))
    } else {
        overlay_line.to_string()
    };
    let overlay_w = visible_width(&overlay_slice);
    let overlay_pad = total_width
        .saturating_sub(start_col)
        .min(overlay_width)
        .saturating_sub(overlay_w);

    let after = if after_start < total_width {
        slice_by_column(base_line, after_start, total_width)
    } else {
        String::new()
    };

    let mut res = String::new();
    res.push_str(&before);
    if before_pad > 0 {
        res.push_str(&" ".repeat(before_pad));
    }
    res.push_str("\x1b[0m");
    res.push_str(&overlay_slice);
    if overlay_pad > 0 {
        res.push_str(&" ".repeat(overlay_pad));
    }
    res.push_str("\x1b[0m");
    res.push_str(&after);

    let current_w = visible_width(&res);
    if current_w > total_width {
        slice_by_column(&res, 0, total_width)
    } else {
        res
    }
}

/// Differential terminal renderer.
#[derive(Debug, Default)]
pub struct TuiRenderer {
    pub previous_lines: Vec<String>,
    pub previous_width: usize,
    pub cursor_row: usize,
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            previous_lines: Vec::new(),
            previous_width: 0,
            cursor_row: 0,
        }
    }

    pub fn reset(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
        self.cursor_row = 0;
    }

    pub fn render_frame(
        &mut self,
        terminal: &mut dyn Terminal,
        mut lines: Vec<String>,
        show_hardware_cursor: bool,
    ) -> std::io::Result<()> {
        let (cols, _rows) = terminal.size();
        let width = cols as usize;
        let width_changed = self.previous_width != 0 && self.previous_width != width;

        // Extract CURSOR_MARKER position
        let mut cursor_pos: Option<(usize, usize)> = None;
        for (row_idx, line) in lines.iter_mut().enumerate() {
            if let Some(pos) = line.find(CURSOR_MARKER) {
                let col = visible_width(&line[..pos]);
                cursor_pos = Some((row_idx, col));
                *line = line.replace(CURSOR_MARKER, "");
                break;
            }
        }

        // Fake reverse-video cursor when hardware cursor is disabled
        if !show_hardware_cursor {
            if let Some((row, col)) = cursor_pos {
                if row < lines.len() {
                    let line = &lines[row];
                    let before = slice_by_column(line, 0, col);
                    let at = slice_by_column(line, col, col + 1);
                    let after = slice_by_column(line, col + 1, visible_width(line));
                    let cursor_char = if at.is_empty() { " " } else { &at };
                    lines[row] = format!("{before}\x1b[7m{cursor_char}\x1b[27m{after}");
                }
            }
        }

        let is_first = self.previous_lines.is_empty();
        let full_redraw = is_first || width_changed;

        let mut out = String::new();
        out.push_str("\x1b[?2026h"); // Synchronized output begin

        let mut final_cursor_row;

        if full_redraw {
            out.push('\r');
            for (i, line) in lines.iter().enumerate() {
                out.push_str("\x1b[2K");
                out.push_str(line);
                if i + 1 < lines.len() {
                    out.push_str("\r\n");
                }
            }
            final_cursor_row = lines.len().saturating_sub(1);
        } else {
            let max_len = lines.len().max(self.previous_lines.len());
            let mut first_changed = None;
            let mut last_changed = None;

            for i in 0..max_len {
                let old_l = self.previous_lines.get(i).map(|s| s.as_str()).unwrap_or("");
                let new_l = lines.get(i).map(|s| s.as_str()).unwrap_or("");
                if old_l != new_l {
                    if first_changed.is_none() {
                        first_changed = Some(i);
                    }
                    last_changed = Some(i);
                }
            }

            if let (Some(first), Some(last)) = (first_changed, last_changed) {
                let diff = first as isize - self.cursor_row as isize;
                if diff < 0 {
                    out.push_str(&format!("\x1b[{}A", -diff));
                } else if diff > 0 {
                    out.push_str(&format!("\x1b[{}B", diff));
                }
                out.push('\r');

                let render_end = last.min(lines.len().saturating_sub(1));
                for (i, line) in lines.iter().enumerate().take(render_end + 1).skip(first) {
                    if i > first {
                        out.push_str("\r\n");
                    }
                    out.push_str("\x1b[2K");
                    out.push_str(line);
                }
                final_cursor_row = render_end;

                if self.previous_lines.len() > lines.len() {
                    if render_end < lines.len().saturating_sub(1) {
                        let move_down = lines.len() - 1 - render_end;
                        out.push_str(&format!("\x1b[{move_down}B"));
                        final_cursor_row = lines.len() - 1;
                    }
                    let extra = self.previous_lines.len() - lines.len();
                    for _ in 0..extra {
                        out.push_str("\r\n\x1b[2K");
                    }
                    out.push_str(&format!("\x1b[{extra}A"));
                }
            } else {
                final_cursor_row = self.cursor_row;
            }
        }

        // Position hardware cursor if tracked
        if show_hardware_cursor {
            if let Some((target_row, target_col)) = cursor_pos {
                let target_row = target_row.min(lines.len().saturating_sub(1));
                let diff = target_row as isize - final_cursor_row as isize;
                if diff < 0 {
                    out.push_str(&format!("\x1b[{}A", -diff));
                } else if diff > 0 {
                    out.push_str(&format!("\x1b[{}B", diff));
                }
                out.push('\r');
                if target_col > 0 {
                    out.push_str(&format!("\x1b[{}C", target_col));
                }
                final_cursor_row = target_row;
            }
        }

        out.push_str("\x1b[?2026l"); // Synchronized output end

        terminal.write(&out)?;
        terminal.flush()?;

        self.previous_lines = lines;
        self.previous_width = width;
        self.cursor_row = final_cursor_row;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_differential_render_cursor_position_when_partial_lines_change() {
        let mut term = MockTerminal::new(40, 10);
        let mut renderer = TuiRenderer::new();

        // Initial 4 lines
        let frame1 = vec![
            "line0".to_string(),
            "line1".to_string(),
            "line2".to_string(),
            "line3".to_string(),
        ];
        renderer.render_frame(&mut term, frame1, false).unwrap();
        term.clear_output();

        // Frame 2: only line0 changes, with cursor at line0
        let frame2 = vec![
            format!("line0_new{CURSOR_MARKER}"),
            "line1".to_string(),
            "line2".to_string(),
            "line3".to_string(),
        ];
        renderer.render_frame(&mut term, frame2, true).unwrap();
        let out = term.full_output();

        // If cursor is at bottom (row 3) after re-rendering line0..line3, moving to line0 takes 3A.
        // But if renderer stops after line0, it should either advance to row 3 before moving 3A, or move 1A from row 1.
        // It must NOT emit an overshoot movement (e.g. moving up 3 from row 1).
        assert!(out.contains("line0_new"));
        assert!(!out.contains("\x1b[3A\r\x1b[2Kline0_new\r\n\x1b[3A"), "Out contained bad overshoot cursor move: {out:?}");
    }
}

