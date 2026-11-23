use crate::ansi::{ansi_code_len, slice_by_column, visible_width};
use unicode_segmentation::UnicodeSegmentation;

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
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
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
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen
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
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            previous_lines: Vec::new(),
            previous_width: 0,
        }
    }

    pub fn reset(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
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
        let mut cursor_byte: Option<(usize, usize)> = None;
        for (row_idx, line) in lines.iter_mut().enumerate() {
            if let Some(pos) = line.find(CURSOR_MARKER) {
                let col = visible_width(&line[..pos]);
                cursor_pos = Some((row_idx, col));
                cursor_byte = Some((row_idx, pos));
                *line = line.replacen(CURSOR_MARKER, "", 1);
                break;
            }
        }

        // Fake reverse-video cursor when hardware cursor is disabled
        if !show_hardware_cursor {
            if let Some((row, byte_pos)) = cursor_byte {
                if let Some(line) = lines.get_mut(row) {
                    let byte_pos = byte_pos.min(line.len());
                    // Keep ANSI codes at the cursor attached to the prefix.
                    let mut style_end = byte_pos;
                    while style_end < line.len() {
                        let code_len = ansi_code_len(line, style_end);
                        if code_len == 0 {
                            break;
                        }
                        style_end += code_len;
                    }
                    let style = &line[byte_pos..style_end];
                    let rest = &line[style_end..];
                    let grapheme = rest.graphemes(true).next().unwrap_or("");
                    let before = format!("{}{}", &line[..byte_pos], style);
                    let after = &rest[grapheme.len()..];
                    let cursor_char = if grapheme.is_empty() { " " } else { grapheme };
                    *line = format!("{before}\x1b[7m{cursor_char}\x1b[27m{after}");
                }
            }
        }

        let is_first = self.previous_lines.is_empty();
        let full_redraw = is_first || width_changed;

        let mut out = String::new();
        out.push_str("\x1b[?2026h"); // Synchronized output begin

        if full_redraw {
            out.push_str("\x1b[H");
            out.push('\r');
            for (i, line) in lines.iter().enumerate() {
                out.push_str("\x1b[2K");
                out.push_str(line);
                if i + 1 < lines.len() {
                    out.push_str("\r\n");
                }
            }
            if self.previous_lines.len() > lines.len() {
                out.push_str(&format!("\x1b[{};1H\x1b[J", lines.len() + 1));
            } else {
                out.push_str("\x1b[J");
            }
        } else {
            let max_len = lines.len().max(self.previous_lines.len());
            for i in 0..max_len {
                let old_l = self.previous_lines.get(i).map(|s| s.as_str()).unwrap_or("");
                let new_l = lines.get(i).map(|s| s.as_str()).unwrap_or("");
                if old_l != new_l {
                    if let Some(line) = lines.get(i) {
                        out.push_str(&format!("\x1b[{};1H\x1b[2K{line}", i + 1));
                    }
                }
            }
            if self.previous_lines.len() > lines.len() {
                out.push_str(&format!("\x1b[{};1H\x1b[J", lines.len() + 1));
            }
        }

        // Position hardware cursor if tracked.
        if show_hardware_cursor {
            if let Some((target_row, target_col)) = cursor_pos {
                let target_row = target_row.min(lines.len().saturating_sub(1));
                out.push_str(&format!("\x1b[{};{}H", target_row + 1, target_col + 1));
            }
        }

        out.push_str("\x1b[?2026l"); // Synchronized output end

        terminal.write(&out)?;
        terminal.flush()?;

        self.previous_lines = lines;
        self.previous_width = width;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_redraw_erases_stale_rows() {
        let mut term = MockTerminal::new(40, 6);
        let mut renderer = TuiRenderer::new();
        renderer
            .render_frame(
                &mut term,
                (0..6).map(|i| format!("old {i}")).collect(),
                false,
            )
            .unwrap();
        renderer.reset();
        term.clear_output();
        renderer
            .render_frame(&mut term, vec!["new".into()], false)
            .unwrap();
        assert!(
            term.full_output().contains("\x1b[J"),
            "full redraw must clear stale rows: {:?}",
            term.full_output()
        );
    }

    #[test]
    fn full_redraw_homes_cursor() {
        let mut term = MockTerminal::new(40, 3);
        let mut renderer = TuiRenderer::new();
        renderer
            .render_frame(&mut term, vec!["a".into()], false)
            .unwrap();
        let out = term.full_output();
        assert!(
            out.contains("\x1b[?2026h\x1b[H"),
            "full redraw must home before writing rows: {out:?}"
        );
    }

    #[test]
    fn differential_render_uses_absolute_rows() {
        let mut term = MockTerminal::new(40, 3);
        let mut renderer = TuiRenderer::new();
        renderer
            .render_frame(
                &mut term,
                vec!["a".into(), "b".into(), "c".into()],
                false,
            )
            .unwrap();
        term.clear_output();
        renderer
            .render_frame(
                &mut term,
                vec!["a".into(), "B".into(), "c".into()],
                false,
            )
            .unwrap();
        let out = term.full_output();
        assert!(
            out.contains("\x1b[2;1H\x1b[2KB"),
            "absolute row positioning: {out:?}"
        );
        assert!(!out.contains("\x1b[1A"), "no relative up move: {out:?}");
        assert!(!out.contains("\x1b[1B"), "no relative down move: {out:?}");
    }

    #[test]
    fn shorter_frame_erases_stale_rows() {
        let mut term = MockTerminal::new(40, 3);
        let mut renderer = TuiRenderer::new();
        renderer
            .render_frame(
                &mut term,
                vec!["a".into(), "b".into(), "c".into()],
                false,
            )
            .unwrap();
        term.clear_output();
        renderer
            .render_frame(&mut term, vec!["x".into()], false)
            .unwrap();
        let out = term.full_output();
        assert!(
            out.contains("\x1b[2;1H\x1b[J"),
            "erase stale rows: {out:?}"
        );
    }

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

        // With absolute row positioning the renderer writes the changed row directly
        // and then places the hardware cursor at the marker; no relative 3A overshoot.
        assert!(out.contains("line0_new"));
        assert!(out.contains("\x1b[1;1H\x1b[2Kline0_new"), "changed row must use absolute positioning: {out:?}");
        assert!(!out.contains("\x1b[3A"), "no relative overshoot: {out:?}");
    }

    #[test]
    fn fake_cursor_preserves_wide_character_after_marker() {
        let mut term = MockTerminal::new(40, 3);
        let mut renderer = TuiRenderer::new();
        let line = format!("> {CURSOR_MARKER}世界👨‍👩‍👧‍👦!");
        renderer.render_frame(&mut term, vec![line], false).unwrap();
        let out = term.full_output();
        assert!(out.contains('世'), "wide character dropped: {out:?}");
        assert!(out.contains('界'), "wide character dropped: {out:?}");
        assert!(
            out.contains('👨') && out.contains('👩') && out.contains('👧') && out.contains('👦'),
            "emoji sequence dropped: {out:?}"
        );
        assert!(out.contains('!'), "trailing character dropped: {out:?}");
    }
}

