use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Return the length of the ANSI, OSC, or APC escape sequence starting at `pos`,
/// or 0 if none is found.
pub fn ansi_code_len(s: &str, pos: usize) -> usize {
    let b = s.as_bytes();
    if pos >= b.len() || b[pos] != 0x1b {
        return 0;
    }
    if pos + 1 >= b.len() {
        return 0;
    }

    match b[pos + 1] {
        // CSI sequence: ESC [ ... FinalByte (0x40..=0x7E)
        b'[' => {
            for (offset, &c) in b[pos + 2..].iter().enumerate() {
                if (0x40..=0x7e).contains(&c) {
                    return offset + 3;
                }
            }
            0
        }
        // OSC sequence: ESC ] ... BEL (0x07) or ST (ESC \)
        // APC sequence: ESC _ ... BEL (0x07) or ST (ESC \)
        // DCS sequence: ESC P ... BEL (0x07) or ST (ESC \)
        b']' | b'_' | b'P' => {
            let mut j = pos + 2;
            while j < b.len() {
                if b[j] == 0x07 {
                    return j + 1 - pos;
                }
                if b[j] == 0x1b && j + 1 < b.len() && b[j + 1] == b'\\' {
                    return j + 2 - pos;
                }
                j += 1;
            }
            0
        }
        // SS3 sequence: ESC O ...
        b'O' if pos + 2 < b.len() => 3,
        _ => 0,
    }
}

/// Strip CSI, OSC, and APC ANSI sequences from `s`.
pub fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&0x1b) {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            let len = ansi_code_len(s, i);
            if len > 0 {
                i += len;
            } else {
                result.push('\x1b');
                i += 1;
            }
        } else if let Some(rel_esc) = b[i..].iter().position(|&c| c == 0x1b) {
            result.push_str(&s[i..i + rel_esc]);
            i += rel_esc;
        } else {
            result.push_str(&s[i..]);
            break;
        }
    }
    result
}

/// Compute visible display width in terminal columns.
/// Strips ANSI/APC, treats tabs as 3 spaces, zero-width as 0.
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }

    // Printable ASCII fast-path without ANSI
    let b = s.as_bytes();
    let mut all_ascii = true;
    for &byte in b {
        if byte == 0x1b || byte == b'\t' || !(0x20..=0x7e).contains(&byte) {
            all_ascii = false;
            break;
        }
    }
    if all_ascii {
        return s.len();
    }

    let clean: std::borrow::Cow<'_, str> = if b.contains(&0x1b) {
        std::borrow::Cow::Owned(strip_ansi(s))
    } else {
        std::borrow::Cow::Borrowed(s)
    };
    let mut width = 0;
    for g in clean.graphemes(true) {
        if g == "\t" {
            width += 3;
        } else if g == "\r" || g == "\n" {
            // Newlines have zero display width
        } else {
            width += g.width();
        }
    }
    width
}

/// Track active ANSI SGR and OSC 8 styling attributes across line breaks.
#[derive(Debug, Clone, Default)]
pub struct AnsiCodeTracker {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
    pub fg_color: Option<String>,
    pub bg_color: Option<String>,
    pub active_hyperlink: Option<(String, String, String)>, // (params, url, terminator)
}

impl AnsiCodeTracker {
    pub fn process(&mut self, code: &str) {
        // OSC 8 hyperlink: \x1b]8;params;url(\x07|\x1b\\)
        if code.starts_with("\x1b]8;") {
            let term = if code.ends_with("\x07") {
                "\x07".to_string()
            } else {
                "\x1b\\".to_string()
            };
            let body = if term == "\x07" {
                &code[4..code.len() - 1]
            } else {
                &code[4..code.len() - 2]
            };
            if let Some(idx) = body.find(';') {
                let params = &body[..idx];
                let url = &body[idx + 1..];
                if url.is_empty() {
                    self.active_hyperlink = None;
                } else {
                    self.active_hyperlink =
                        Some((params.to_string(), url.to_string(), term));
                }
            }
            return;
        }

        // CSI SGR code: \x1b[...m
        if code.starts_with("\x1b[") && code.ends_with('m') {
            let params = &code[2..code.len() - 1];
            if params.is_empty() || params == "0" {
                self.reset();
                return;
            }

            let parts: Vec<&str> = params.split(';').collect();
            let mut i = 0;
            while i < parts.len() {
                let num: u32 = parts[i].parse().unwrap_or(0);
                if (num == 38 || num == 48) && i + 2 < parts.len() && parts[i + 1] == "5" {
                    // 256 colors: 38;5;N or 48;5;N
                    let color = format!("{};5;{}", parts[i], parts[i + 2]);
                    if num == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 3;
                    continue;
                }
                if (num == 38 || num == 48) && i + 4 < parts.len() && parts[i + 1] == "2" {
                    // RGB colors: 38;2;R;G;B or 48;2;R;G;B
                    let color = format!(
                        "{};2;{};{};{}",
                        parts[i], parts[i + 2], parts[i + 3], parts[i + 4]
                    );
                    if num == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 5;
                    continue;
                }

                match num {
                    0 => self.reset(),
                    1 => self.bold = true,
                    2 => self.dim = true,
                    3 => self.italic = true,
                    4 => self.underline = true,
                    5 => self.blink = true,
                    7 => self.inverse = true,
                    8 => self.hidden = true,
                    9 => self.strikethrough = true,
                    21 => self.bold = false,
                    22 => {
                        self.bold = false;
                        self.dim = false;
                    }
                    23 => self.italic = false,
                    24 => self.underline = false,
                    25 => self.blink = false,
                    27 => self.inverse = false,
                    28 => self.hidden = false,
                    29 => self.strikethrough = false,
                    39 => self.fg_color = None,
                    49 => self.bg_color = None,
                    30..=37 | 90..=97 => self.fg_color = Some(num.to_string()),
                    40..=47 | 100..=107 => self.bg_color = Some(num.to_string()),
                    _ => {}
                }
                i += 1;
            }
        }
    }

    pub fn reset(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg_color = None;
        self.bg_color = None;
    }

    pub fn get_active_codes(&self) -> String {
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1");
        }
        if self.dim {
            codes.push("2");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.blink {
            codes.push("5");
        }
        if self.inverse {
            codes.push("7");
        }
        if self.hidden {
            codes.push("8");
        }
        if self.strikethrough {
            codes.push("9");
        }
        if let Some(ref fg) = self.fg_color {
            codes.push(fg.as_str());
        }
        if let Some(ref bg) = self.bg_color {
            codes.push(bg.as_str());
        }

        let mut res = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };

        if let Some((ref params, ref url, ref term)) = self.active_hyperlink {
            res.push_str(&format!("\x1b]8;{params};{url}{term}"));
        }
        res
    }

    pub fn get_line_end_reset(&self) -> String {
        let mut res = String::new();
        if self.underline {
            res.push_str("\x1b[24m");
        }
        if self.strikethrough {
            res.push_str("\x1b[29m");
        }
        if let Some((_, _, ref term)) = self.active_hyperlink {
            res.push_str(&format!("\x1b]8;;{term}"));
        }
        res
    }

    pub fn has_active_codes(&self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg_color.is_some()
            || self.bg_color.is_some()
            || self.active_hyperlink.is_some()
    }
}

/// Truncate text to fit within `max_width`, preserving active SGR styling,
/// appending `ellipsis` + `\x1b[0m`, and optionally padding with spaces to `max_width`.
pub fn truncate_to_width(s: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }

    let ell_width = visible_width(ellipsis);
    let s_width = visible_width(s);

    if s_width <= max_width {
        return if pad && s_width < max_width {
            format!("{s}{}", " ".repeat(max_width - s_width))
        } else {
            s.to_string()
        };
    }

    if ell_width >= max_width {
        let mut clipped = String::new();
        let mut cur = 0;
        for g in ellipsis.graphemes(true) {
            let w = g.width();
            if cur + w <= max_width {
                clipped.push_str(g);
                cur += w;
            } else {
                break;
            }
        }
        let pad_len = if pad { max_width.saturating_sub(cur) } else { 0 };
        let reset = if s.contains('\x1b') { "\x1b[0m" } else { "" };
        return format!("{clipped}{reset}{}", " ".repeat(pad_len));
    }

    let target_width = max_width - ell_width;
    let mut result = String::new();
    let mut kept_width = 0;
    let mut tracker = AnsiCodeTracker::default();
    let mut pending_ansi = String::new();
    let mut i = 0;

    while i < s.len() {
        let code_len = ansi_code_len(s, i);
        if code_len > 0 {
            let code = &s[i..i + code_len];
            tracker.process(code);
            pending_ansi.push_str(code);
            i += code_len;
            continue;
        }

        let ch = s[i..].chars().next().unwrap();
        let next_i = i + ch.len_utf8();
        let grapheme = &s[i..next_i];
        let w = if grapheme == "\t" { 3 } else { grapheme.width() };

        if kept_width + w <= target_width {
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push_str(grapheme);
            kept_width += w;
            i = next_i;
        } else {
            break;
        }
    }

    let mut out = result;
    if tracker.has_active_codes() {
        if let Some((_, _, ref term)) = tracker.active_hyperlink {
            out.push_str(&format!("\x1b]8;;{term}"));
        }
        out.push_str("\x1b[0m");
        out.push_str(ellipsis);
        out.push_str("\x1b[0m");
    } else {
        out.push_str(ellipsis);
    }

    let total_cur_width = kept_width + ell_width;
    if pad && total_cur_width < max_width {
        out.push_str(&" ".repeat(max_width - total_cur_width));
    }

    out
}

/// Helper to test if a character is CJK for wrapping purposes.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{2EFF}' | // CJK Radicals Supplement
        '\u{2F00}'..='\u{2FDF}' | // Kangxi Radicals
        '\u{3000}'..='\u{303F}' | // CJK Symbols and Punctuation
        '\u{3040}'..='\u{309F}' | // Hiragana
        '\u{30A0}'..='\u{30FF}' | // Katakana
        '\u{3100}'..='\u{312F}' | // Bopomofo
        '\u{31A0}'..='\u{31BF}' | // Bopomofo Extended
        '\u{3400}'..='\u{4DBF}' | // CJK Unified Ideographs Extension A
        '\u{4E00}'..='\u{9FFF}' | // CJK Unified Ideographs
        '\u{AC00}'..='\u{D7AF}' | // Hangul Syllables
        '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
    )
}

/// Split text into tokens with attached ANSI codes.
/// CJK characters are split into single-character tokens.
fn split_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_is_space: Option<bool> = None;
    let mut i = 0;

    let flush = |tokens: &mut Vec<String>, current: &mut String, current_is_space: &mut Option<bool>| {
        if !current.is_empty() {
            tokens.push(std::mem::take(current));
            *current_is_space = None;
        }
    };

    while i < text.len() {
        let len = ansi_code_len(text, i);
        if len > 0 {
            pending_ansi.push_str(&text[i..i + len]);
            i += len;
            continue;
        }

        let ch = text[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        let seg = &text[i..i + ch_len];
        let is_space = ch == ' ' || ch == '\t';

        if !is_space && is_cjk(ch) {
            flush(&mut tokens, &mut current, &mut current_is_space);
            let mut tok = std::mem::take(&mut pending_ansi);
            tok.push_str(seg);
            tokens.push(tok);
            i += ch_len;
            continue;
        }

        if let Some(curr_space) = current_is_space {
            if curr_space != is_space {
                flush(&mut tokens, &mut current, &mut current_is_space);
            }
        }

        if !pending_ansi.is_empty() {
            current.push_str(&pending_ansi);
            pending_ansi.clear();
        }

        current_is_space = Some(is_space);
        current.push_str(seg);
        i += ch_len;
    }

    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current.push_str(&pending_ansi);
        }
    }

    flush(&mut tokens, &mut current, &mut current_is_space);
    tokens
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_w = 0;
    let mut i = 0;

    while i < word.len() {
        let code_len = ansi_code_len(word, i);
        if code_len > 0 {
            let code = &word[i..i + code_len];
            tracker.process(code);
            current_line.push_str(code);
            i += code_len;
            continue;
        }

        let ch = word[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        let seg = &word[i..i + ch_len];
        let w = if seg == "\t" { 3 } else { seg.width() };

        if current_w + w > width && current_w > 0 {
            let reset = tracker.get_line_end_reset();
            if !reset.is_empty() {
                current_line.push_str(&reset);
            }
            lines.push(current_line);
            current_line = tracker.get_active_codes();
            current_w = 0;
        }

        current_line.push_str(seg);
        current_w += w;
        i += ch_len;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

fn wrap_single_line(line: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let vis_len = visible_width(line);
    if vis_len <= width {
        // Update tracker with line codes
        let mut i = 0;
        while i < line.len() {
            let len = ansi_code_len(line, i);
            if len > 0 {
                tracker.process(&line[i..i + len]);
                i += len;
            } else {
                let ch = line[i..].chars().next().unwrap();
                i += ch.len_utf8();
            }
        }
        return vec![line.to_string()];
    }

    let mut wrapped = Vec::new();
    let tokens = split_tokens_with_ansi(line);
    let mut current_line = tracker.get_active_codes();
    let mut current_len = 0;

    for token in tokens {
        let token_vis_len = visible_width(&token);
        let is_space = strip_ansi(&token).chars().all(|c| c == ' ' || c == '\t');

        if token_vis_len > width && !is_space {
            if current_len > 0 {
                let trimmed = current_line.trim_end().to_string();
                let reset = tracker.get_line_end_reset();
                let mut out = trimmed;
                if !reset.is_empty() {
                    out.push_str(&reset);
                }
                wrapped.push(out);
                current_line = tracker.get_active_codes();
                current_len = 0;
            }

            let broken = break_long_word(&token, width, tracker);
            for (idx, b_line) in broken.iter().enumerate() {
                if idx + 1 < broken.len() {
                    wrapped.push(b_line.clone());
                } else {
                    current_line = b_line.clone();
                    current_len = visible_width(&current_line);
                }
            }
            continue;
        }

        if current_len + token_vis_len > width && current_len > 0 {
            let trimmed = current_line.trim_end().to_string();
            let reset = tracker.get_line_end_reset();
            let mut out = trimmed;
            if !reset.is_empty() {
                out.push_str(&reset);
            }
            wrapped.push(out);

            if is_space {
                current_line = tracker.get_active_codes();
                current_len = 0;
            } else {
                current_line = format!("{}{token}", tracker.get_active_codes());
                current_len = token_vis_len;
            }
        } else {
            current_line.push_str(&token);
            current_len += token_vis_len;
        }

        // Update tracker with tokens
        let mut i = 0;
        while i < token.len() {
            let len = ansi_code_len(&token, i);
            if len > 0 {
                tracker.process(&token[i..i + len]);
                i += len;
            } else {
                let ch = token[i..].chars().next().unwrap();
                i += ch.len_utf8();
            }
        }
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }

    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped.into_iter().map(|l| l.trim_end().to_string()).collect()
    }
}

/// Wrap lines at word boundaries (breaking long words if wider than `width`),
/// tracking active SGR and OSC 8 across wrapped lines.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![text.to_string()];
    }

    let input_lines: Vec<&str> = text.split("\r\n").flat_map(|s| s.split('\n')).flat_map(|s| s.split('\r')).collect();
    let mut result = Vec::new();
    let mut tracker = AnsiCodeTracker::default();

    for line in input_lines {
        let wrapped = wrap_single_line(line, width, &mut tracker);
        for w in wrapped {
            result.push(w);
        }
    }

    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

/// Extract visible column slice `[start_col, end_col)` while preserving SGR styles.
pub fn slice_by_column(line: &str, start_col: usize, end_col: usize) -> String {
    if end_col <= start_col {
        return String::new();
    }

    let mut result = String::new();
    let mut current_col = 0;
    let mut pending_ansi = String::new();
    let mut tracker = AnsiCodeTracker::default();
    let mut i = 0;

    while i < line.len() {
        let code_len = ansi_code_len(line, i);
        if code_len > 0 {
            let code = &line[i..i + code_len];
            tracker.process(code);

            if current_col >= start_col && current_col < end_col {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(code);
            } else if current_col < start_col && !code.starts_with("\x1b_") {
                pending_ansi.push_str(code);
            }
            i += code_len;
            continue;
        }

        let ch = line[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        let seg = &line[i..i + ch_len];
        let w = if seg == "\t" { 3 } else { seg.width() };

        let in_range = current_col >= start_col && current_col < end_col;
        if in_range {
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push_str(seg);
        }

        current_col += w;
        i += ch_len;

        if current_col >= end_col {
            break;
        }
    }

    if tracker.has_active_codes() && !result.is_empty() {
        result.push_str("\x1b[0m");
    }

    result
}
