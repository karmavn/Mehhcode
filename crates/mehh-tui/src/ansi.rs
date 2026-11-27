use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
            let mut j = pos + 2;
            while j < b.len() {
                if (0x40..=0x7e).contains(&b[j]) {
                    return j + 1 - pos;
                }
                j += 1;
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
        // SS3 sequence: ESC O <ASCII final byte>
        b'O' if pos + 2 < b.len() && (0x20..=0x7e).contains(&b[pos + 2]) => 3,
        _ => 0,
    }
}

/// Strip terminal escape sequences and dangerous control / Bidi override characters,
/// returning `Cow::Borrowed` when `s` already contains only safe ASCII bytes.
/// Preserves `\t`, and preserves `\n` when `allow_newlines` is `true`.
pub fn sanitize_terminal_text_cow(s: &str, allow_newlines: bool) -> Cow<'_, str> {
    let fast_clean = s.as_bytes().iter().all(|&b| {
        (0x20..=0x7e).contains(&b) || b == b'\t' || (allow_newlines && b == b'\n')
    });
    if fast_clean {
        return Cow::Borrowed(s);
    }
    Cow::Owned(sanitize_terminal_text_owned(s, allow_newlines))
}

/// Strip terminal escape sequences and dangerous control / Bidi override characters.
/// Preserves `\t`, and preserves `\n` when `allow_newlines` is `true`.
pub fn sanitize_terminal_text(s: &str, allow_newlines: bool) -> String {
    sanitize_terminal_text_cow(s, allow_newlines).into_owned()
}

fn sanitize_terminal_text_owned(s: &str, allow_newlines: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    while i < b.len() {
        let byte = b[i];
        if byte == 0x1b {
            let len = ansi_code_len(s, i);
            if len > 0 {
                i += len;
                continue;
            }
            if i + 1 < b.len() {
                let next = b[i + 1];
                // PM (ESC ^) or SOS (ESC X) terminated by BEL or ST
                if next == b'^' || next == b'X' {
                    let mut j = i + 2;
                    let mut seq_len = None;
                    while j < b.len() {
                        if b[j] == 0x07 {
                            seq_len = Some(j + 1 - i);
                            break;
                        }
                        if b[j] == 0x1b && j + 1 < b.len() && b[j + 1] == b'\\' {
                            seq_len = Some(j + 2 - i);
                            break;
                        }
                        j += 1;
                    }
                    if let Some(len) = seq_len {
                        i += len;
                        continue;
                    }
                } else if (0x20..=0x2f).contains(&next) {
                    // nF escape sequence: ESC <intermediate 0x20..=0x2F>+ <final 0x30..=0x7E>
                    let mut j = i + 2;
                    while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                        j += 1;
                    }
                    if j < b.len() && (0x30..=0x7e).contains(&b[j]) {
                        i = j + 1;
                        continue;
                    }
                } else if (0x30..=0x7e).contains(&next)
                    && !matches!(next, b'[' | b']' | b'_' | b'P')
                {
                    // 2-byte ESC sequence (e.g. \x1bc, \x1b7, \x1b8, \x1bM)
                    i += 2;
                    continue;
                }
            }
            // Bare ESC or unterminated introducer: strip ESC byte only
            i += 1;
            continue;
        }

        if (0x20..=0x7e).contains(&byte) {
            out.push(byte as char);
            i += 1;
            continue;
        }
        if byte == b'\t' || (allow_newlines && byte == b'\n') {
            out.push(byte as char);
            i += 1;
            continue;
        }
        if byte < 0x20 || byte == 0x7f {
            i += 1;
            continue;
        }

        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        let ch_len = ch.len_utf8();
        if !matches!(
            ch,
            '\u{0080}'..='\u{009F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        ) {
            out.push(ch);
        }
        i += ch_len;
    }

    out
}

fn grapheme_width(g: &str) -> usize {
    if g == "\t" {
        3
    } else if g.chars().all(|c| c.is_control()) {
        0
    } else {
        g.width()
    }
}

fn next_ansi_pos(s: &str, from: usize) -> usize {
    let bytes = s.as_bytes();
    let mut pos = from;
    while let Some(rel) = bytes[pos..].iter().position(|&b| b == 0x1b) {
        let abs = pos + rel;
        if ansi_code_len(s, abs) > 0 {
            return abs;
        }
        pos = abs + 1;
    }
    s.len()
}

fn safe_ascii_prefix_len(chunk: &str) -> usize {
    let cb = chunk.as_bytes();
    let mut ascii_len = 0;
    while ascii_len < cb.len() && (0x20..=0x7e).contains(&cb[ascii_len]) {
        ascii_len += 1;
    }
    if ascii_len > 0 && ascii_len < cb.len() && cb[ascii_len] >= 0x80 {
        ascii_len -= 1;
    }
    ascii_len
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

    let b = s.as_bytes();
    let mut i = 0;
    let mut width = 0;
    let mut fallback = false;
    while i < b.len() {
        let byte = b[i];
        if (0x20..=0x7e).contains(&byte) {
            width += 1;
            i += 1;
        } else if byte == b'\t' {
            width += 3;
            i += 1;
        } else if byte == b'\r' || byte == b'\n' {
            i += 1;
        } else if byte == 0x1b {
            let len = ansi_code_len(s, i);
            if len > 0 {
                i += len;
            } else {
                fallback = true;
                break;
            }
        } else if byte < 0x20 || byte == 0x7f {
            fallback = true;
            break;
        } else {
            let Some(ch) = s[i..].chars().next() else {
                break;
            };
            if ch >= '\u{10000}'
                || matches!(
                    ch,
                    '\u{0080}'..='\u{009F}'
                        | '\u{00AD}'
                        | '\u{0300}'..='\u{036F}'
                        | '\u{1100}'..='\u{11FF}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{206F}'
                        | '\u{20D0}'..='\u{20FF}'
                        | '\u{A960}'..='\u{A97F}'
                        | '\u{D7B0}'..='\u{D7FF}'
                        | '\u{FE00}'..='\u{FEFF}'
                )
            {
                fallback = true;
                break;
            }
            match ch.width() {
                Some(w) if w > 0 => {
                    width += w;
                    i += ch.len_utf8();
                }
                _ => {
                    fallback = true;
                    break;
                }
            }
        }
    }
    if !fallback {
        return width;
    }

    let clean: std::borrow::Cow<'_, str> = if b.contains(&0x1b) {
        std::borrow::Cow::Owned(strip_ansi(s))
    } else {
        std::borrow::Cow::Borrowed(s)
    };
    let mut width = 0;
    for g in clean.graphemes(true) {
        width += grapheme_width(g);
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
        if let Some(rest) = code.strip_prefix("\x1b]8;") {
            let (body, term) = if let Some(b) = rest.strip_suffix('\x07') {
                (b, "\x07")
            } else if let Some(b) = rest.strip_suffix("\x1b\\") {
                (b, "\x1b\\")
            } else {
                return;
            };
            if let Some((params, url)) = body.split_once(';') {
                if url.is_empty() {
                    self.active_hyperlink = None;
                } else {
                    self.active_hyperlink =
                        Some((params.to_string(), url.to_string(), term.to_string()));
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

            let mut parts = params.split(';');
            while let Some(part) = parts.next() {
                let num: u32 = part.parse().unwrap_or(0);
                if num == 38 || num == 48 {
                    let mut look = parts.clone();
                    match look.next() {
                        Some("5") => {
                            if let Some(c) = look.next() {
                                let color = format!("{part};5;{c}");
                                if num == 38 {
                                    self.fg_color = Some(color);
                                } else {
                                    self.bg_color = Some(color);
                                }
                                parts = look;
                                continue;
                            }
                        }
                        Some("2") => {
                            if let (Some(r), Some(g), Some(b)) =
                                (look.next(), look.next(), look.next())
                            {
                                let color = format!("{part};2;{r};{g};{b}");
                                if num == 38 {
                                    self.fg_color = Some(color);
                                } else {
                                    self.bg_color = Some(color);
                                }
                                parts = look;
                                continue;
                            }
                        }
                        _ => {}
                    }
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

    pub fn write_active_codes(&self, out: &mut String) {
        if !self.has_active_codes() {
            return;
        }
        let has_sgr = self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg_color.is_some()
            || self.bg_color.is_some();
        if has_sgr {
            out.push_str("\x1b[");
            let mut first = true;
            let mut push_code = |code: &str| {
                if !first {
                    out.push(';');
                }
                first = false;
                out.push_str(code);
            };
            if self.bold {
                push_code("1");
            }
            if self.dim {
                push_code("2");
            }
            if self.italic {
                push_code("3");
            }
            if self.underline {
                push_code("4");
            }
            if self.blink {
                push_code("5");
            }
            if self.inverse {
                push_code("7");
            }
            if self.hidden {
                push_code("8");
            }
            if self.strikethrough {
                push_code("9");
            }
            if let Some(ref fg) = self.fg_color {
                push_code(fg.as_str());
            }
            if let Some(ref bg) = self.bg_color {
                push_code(bg.as_str());
            }
            out.push('m');
        }

        if let Some((ref params, ref url, ref term)) = self.active_hyperlink {
            out.push_str("\x1b]8;");
            out.push_str(params);
            out.push(';');
            out.push_str(url);
            out.push_str(term);
        }
    }

    pub fn write_line_end_reset(&self, out: &mut String) {
        if self.underline {
            out.push_str("\x1b[24m");
        }
        if self.strikethrough {
            out.push_str("\x1b[29m");
        }
        if let Some((_, _, ref term)) = self.active_hyperlink {
            out.push_str("\x1b]8;;");
            out.push_str(term);
        }
    }

    pub fn get_active_codes(&self) -> String {
        let mut res = String::new();
        self.write_active_codes(&mut res);
        res
    }

    pub fn get_line_end_reset(&self) -> String {
        let mut res = String::new();
        self.write_line_end_reset(&mut res);
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

    let s_width = visible_width(s);

    if s_width <= max_width {
        if pad && s_width < max_width {
            let pad_len = max_width - s_width;
            let mut padded = String::with_capacity(s.len() + pad_len);
            padded.push_str(s);
            padded.extend(std::iter::repeat_n(' ', pad_len));
            return padded;
        }
        return s.to_string();
    }

    let ell_width = visible_width(ellipsis);
    if ell_width >= max_width {
        let mut clipped = String::new();
        let mut cur = 0;
        for g in ellipsis.graphemes(true) {
            let w = grapheme_width(g);
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
    let mut result = String::with_capacity(s.len().min(max_width.saturating_mul(2) + 16));
    let mut kept_width = 0;
    let mut tracker = AnsiCodeTracker::default();
    let mut pending_ansi = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let byte = bytes[i];
        if byte == 0x1b {
            let code_len = ansi_code_len(s, i);
            if code_len > 0 {
                let code = &s[i..i + code_len];
                pending_ansi.push_str(code);
                i += code_len;
                continue;
            }
        }

        let chunk_end = next_ansi_pos(s, i);
        let chunk = &s[i..chunk_end];
        let ascii_len = safe_ascii_prefix_len(chunk);
        if ascii_len > 0 {
            let remaining = target_width.saturating_sub(kept_width);
            if remaining == 0 {
                break;
            }
            if !pending_ansi.is_empty() {
                process_ansi_in_str(&pending_ansi, &mut tracker);
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            let take = ascii_len.min(remaining);
            result.push_str(&chunk[..take]);
            kept_width += take;
            i += take;
            if kept_width >= target_width {
                break;
            }
            continue;
        }

        let mut broke = false;
        for g in chunk.graphemes(true) {
            let w = grapheme_width(g);
            if kept_width + w <= target_width {
                if !pending_ansi.is_empty() {
                    process_ansi_in_str(&pending_ansi, &mut tracker);
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(g);
                kept_width += w;
                i += g.len();
            } else {
                broke = true;
                break;
            }
        }
        if broke {
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
        out.extend(std::iter::repeat_n(' ', max_width - total_cur_width));
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
fn split_tokens_with_ansi(text: &str) -> Vec<Cow<'_, str>> {
    let mut tokens: Vec<Cow<'_, str>> = Vec::new();
    let mut slice_start: Option<usize> = None;
    let mut owned = String::new();
    let mut pending_ansi = String::new();
    let mut current_is_space: Option<bool> = None;
    let mut i = 0;

    fn flush_token<'a>(
        tokens: &mut Vec<Cow<'a, str>>,
        text: &'a str,
        end: usize,
        slice_start: &mut Option<usize>,
        owned: &mut String,
        current_is_space: &mut Option<bool>,
    ) {
        if let Some(start) = slice_start.take() {
            if start < end {
                tokens.push(Cow::Borrowed(&text[start..end]));
            }
            *current_is_space = None;
        } else if !owned.is_empty() {
            tokens.push(Cow::Owned(std::mem::take(owned)));
            *current_is_space = None;
        }
    }

    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let len = ansi_code_len(text, i);
            if len > 0 {
                if let Some(start) = slice_start.take() {
                    owned.push_str(&text[start..i]);
                }
                pending_ansi.push_str(&text[i..i + len]);
                i += len;
                continue;
            }
        }

        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        let ch_len = ch.len_utf8();
        let seg = &text[i..i + ch_len];
        let is_space = ch == ' ' || ch == '\t';

        if !is_space && is_cjk(ch) {
            flush_token(
                &mut tokens,
                text,
                i,
                &mut slice_start,
                &mut owned,
                &mut current_is_space,
            );
            if pending_ansi.is_empty() {
                tokens.push(Cow::Borrowed(seg));
            } else {
                let mut tok = std::mem::take(&mut pending_ansi);
                tok.push_str(seg);
                tokens.push(Cow::Owned(tok));
            }
            i += ch_len;
            continue;
        }

        if let Some(curr_space) = current_is_space {
            if curr_space != is_space {
                flush_token(
                    &mut tokens,
                    text,
                    i,
                    &mut slice_start,
                    &mut owned,
                    &mut current_is_space,
                );
            }
        }

        if !pending_ansi.is_empty() {
            if let Some(start) = slice_start.take() {
                owned.push_str(&text[start..i]);
            }
            owned.push_str(&pending_ansi);
            pending_ansi.clear();
            owned.push_str(seg);
        } else if slice_start.is_none() && owned.is_empty() {
            slice_start = Some(i);
        } else if !owned.is_empty() {
            owned.push_str(seg);
        }

        current_is_space = Some(is_space);
        i += ch_len;
    }

    if !pending_ansi.is_empty() {
        if let Some(start) = slice_start.take() {
            owned.push_str(&text[start..]);
        }
        if !owned.is_empty() {
            owned.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.to_mut().push_str(&pending_ansi);
        } else {
            owned.push_str(&pending_ansi);
        }
    }

    flush_token(
        &mut tokens,
        text,
        text.len(),
        &mut slice_start,
        &mut owned,
        &mut current_is_space,
    );
    tokens
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    if width == 0 {
        return vec![word.to_string()];
    }
    let mut lines = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_w = 0;
    let bytes = word.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let code_len = ansi_code_len(word, i);
            if code_len > 0 {
                let code = &word[i..i + code_len];
                tracker.process(code);
                current_line.push_str(code);
                i += code_len;
                continue;
            }
        }

        let chunk_end = next_ansi_pos(word, i);
        let chunk = &word[i..chunk_end];
        let ascii_len = safe_ascii_prefix_len(chunk);
        if ascii_len > 0 {
            let mut offset = 0;
            while offset < ascii_len {
                let remaining = width.saturating_sub(current_w);
                if remaining == 0 {
                    tracker.write_line_end_reset(&mut current_line);
                    lines.push(current_line);
                    current_line = tracker.get_active_codes();
                    current_w = 0;
                    continue;
                }
                let take = (ascii_len - offset).min(remaining);
                current_line.push_str(&chunk[offset..offset + take]);
                current_w += take;
                offset += take;
            }
            i += ascii_len;
            continue;
        }

        for g in chunk.graphemes(true) {
            let w = grapheme_width(g);
            if current_w + w > width && current_w > 0 {
                tracker.write_line_end_reset(&mut current_line);
                lines.push(current_line);
                current_line = tracker.get_active_codes();
                current_w = 0;
            }

            current_line.push_str(g);
            current_w += w;
        }
        i = chunk_end;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

fn is_ansi_whitespace_only(s: &str) -> bool {
    let b = s.as_bytes();
    if !b.contains(&0x1b) {
        return s.chars().all(|c| c == ' ' || c == '\t');
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            let len = ansi_code_len(s, i);
            if len > 0 {
                i += len;
                continue;
            }
        }
        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        if ch != ' ' && ch != '\t' {
            return false;
        }
        i += ch.len_utf8();
    }
    true
}

fn has_non_ansi_content(s: &str) -> bool {
    let b = s.as_bytes();
    if !b.contains(&0x1b) {
        return !s.is_empty();
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            let len = ansi_code_len(s, i);
            if len > 0 {
                i += len;
                continue;
            }
        }
        return true;
    }
    false
}

fn process_ansi_in_str(s: &str, tracker: &mut AnsiCodeTracker) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while let Some(rel_esc) = bytes[i..].iter().position(|&b| b == 0x1b) {
        let pos = i + rel_esc;
        let len = ansi_code_len(s, pos);
        if len > 0 {
            tracker.process(&s[pos..pos + len]);
            i = pos + len;
        } else {
            i = pos + 1;
        }
    }
}

fn extract_apc_sequences(s: &str) -> String {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while let Some(rel_esc) = bytes[i..].iter().position(|&b| b == 0x1b) {
        let pos = i + rel_esc;
        let len = ansi_code_len(s, pos);
        if len > 0 {
            if bytes.get(pos + 1) == Some(&b'_') {
                out.push_str(&s[pos..pos + len]);
            }
            i = pos + len;
        } else {
            i = pos + 1;
        }
    }
    out
}

fn wrap_single_line_into(
    line: &str,
    width: usize,
    tracker: &mut AnsiCodeTracker,
    out_lines: &mut Vec<String>,
) {
    if line.is_empty() {
        out_lines.push(String::new());
        return;
    }

    let has_esc = line.as_bytes().contains(&0x1b);
    let vis_len = visible_width(line);
    if vis_len <= width {
        if !has_esc && !tracker.has_active_codes() {
            out_lines.push(line.to_string());
            return;
        }
        let mut s = String::with_capacity(line.len() + 16);
        tracker.write_active_codes(&mut s);
        s.push_str(line);
        if has_esc {
            process_ansi_in_str(line, tracker);
        }
        tracker.write_line_end_reset(&mut s);
        out_lines.push(s);
        return;
    }

    let start_idx = out_lines.len();
    let tokens = split_tokens_with_ansi(line);
    let mut current_line = tracker.get_active_codes();
    let mut current_len = 0;

    for token in tokens {
        let token_ref = token.as_ref();
        let token_vis_len = visible_width(token_ref);
        let is_space = is_ansi_whitespace_only(token_ref);

        if token_vis_len > width {
            if current_len > 0 {
                let mut out = current_line.trim_end().to_string();
                tracker.write_line_end_reset(&mut out);
                out_lines.push(out);
                current_line = tracker.get_active_codes();
                current_len = 0;
            }

            let broken = break_long_word(token_ref, width, tracker);
            let broken_len = broken.len();
            for (idx, b_line) in broken.into_iter().enumerate() {
                if idx + 1 < broken_len {
                    out_lines.push(b_line);
                } else {
                    current_len = visible_width(&b_line);
                    current_line = b_line;
                }
            }
            continue;
        }

        if current_len + token_vis_len > width && current_len > 0 {
            let mut out = current_line.trim_end().to_string();
            tracker.write_line_end_reset(&mut out);
            out_lines.push(out);

            let token_has_esc = token_ref.as_bytes().contains(&0x1b);
            if is_space {
                if token_has_esc {
                    process_ansi_in_str(token_ref, tracker);
                }
                current_line = tracker.get_active_codes();
                if token_has_esc {
                    let apc = extract_apc_sequences(token_ref);
                    current_line.push_str(&apc);
                }
                current_len = 0;
            } else {
                current_line = tracker.get_active_codes();
                current_line.push_str(token_ref);
                current_len = token_vis_len;
                if token_has_esc {
                    process_ansi_in_str(token_ref, tracker);
                }
            }
        } else {
            current_line.push_str(token_ref);
            current_len += token_vis_len;
            if token_ref.as_bytes().contains(&0x1b) {
                process_ansi_in_str(token_ref, tracker);
            }
        }
    }

    let trimmed = current_line.trim_end();
    if has_non_ansi_content(trimmed)
        || (trimmed.as_bytes().contains(&0x1b) && !extract_apc_sequences(trimmed).is_empty())
    {
        let mut out = trimmed.to_string();
        tracker.write_line_end_reset(&mut out);
        out_lines.push(out);
    }

    if out_lines.len() == start_idx {
        out_lines.push(String::new());
    } else {
        for l in &mut out_lines[start_idx..] {
            let trimmed_len = l.trim_end().len();
            l.truncate(trimmed_len);
        }
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

    if !text.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r') {
        if visible_width(text) <= width {
            return vec![text.to_string()];
        }
        let mut result = Vec::new();
        wrap_single_line_into(text, width, &mut AnsiCodeTracker::default(), &mut result);
        return result;
    }

    let input_lines = text
        .split("\r\n")
        .flat_map(|s| s.split('\n'))
        .flat_map(|s| s.split('\r'));
    let mut result = Vec::new();
    let mut tracker = AnsiCodeTracker::default();

    for line in input_lines {
        wrap_single_line_into(line, width, &mut tracker, &mut result);
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

    let bytes = line.as_bytes();
    while i < bytes.len() {
        if bytes[i] == 0x1b {
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
        }

        let chunk_end = next_ansi_pos(line, i);
        let chunk = &line[i..chunk_end];
        let ascii_len = safe_ascii_prefix_len(chunk);
        if ascii_len > 0 {
            let run_end_col = current_col + ascii_len;
            let vis_start = start_col.max(current_col).min(run_end_col);
            let vis_end = end_col.max(current_col).min(run_end_col);
            if vis_end > vis_start {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                let skip = vis_start - current_col;
                let take_end = vis_end - current_col;
                result.push_str(&chunk[skip..take_end]);
            }
            current_col = run_end_col;
            i += ascii_len;
            if current_col >= end_col {
                break;
            }
            continue;
        }

        for g in chunk.graphemes(true) {
            let w = grapheme_width(g);
            let in_range = if w == 0 {
                current_col >= start_col && current_col < end_col
            } else {
                current_col >= start_col && current_col + w <= end_col
            };
            if in_range {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(g);
            }

            current_col += w;
            i += g.len();

            if current_col >= end_col {
                break;
            }
        }
        if current_col >= end_col {
            break;
        }
    }

    if tracker.has_active_codes() && !result.is_empty() {
        if let Some((_, _, ref term)) = tracker.active_hyperlink {
            result.push_str(&format!("\x1b]8;;{term}"));
        }
        result.push_str("\x1b[0m");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_visible_width(s: &str) -> usize {
        let clean = strip_ansi(s);
        let mut width = 0;
        for g in clean.graphemes(true) {
            if g == "\t" {
                width += 3;
            } else if g.chars().all(|c| c.is_control()) {
                // zero width
            } else {
                width += g.width();
            }
        }
        width
    }

    #[test]
    fn visible_width_matches_grapheme_reference() {
        let samples = [
            "",
            "plain ascii status text",
            "\x1b[38;2;120;200;255m\x1b[1m┌───┬───┐\x1b[0m",
            "\x1b[36m日本語テスト\x1b[0m and tabs\t\tcol",
            "combining: e\u{0301} + a\u{0308}",
            "vs16: \u{2764}\u{FE0F} indicator",
            "zwj: \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\x1b]8;;https://docs.rs\x07Link\x1b]8;;\x07\n",
        ];
        for sample in samples {
            assert_eq!(
                visible_width(sample),
                reference_visible_width(sample),
                "mismatch for {sample:?}"
            );
        }
    }

    #[test]
    fn ss3_requires_ascii_final_byte_and_does_not_panic_on_multibyte() {
        let s = "\x1bO\u{00E9}";
        assert_eq!(ansi_code_len(s, 0), 0);
        assert_eq!(strip_ansi(s), s);
        assert_eq!(visible_width(s), 2);
        assert_eq!(truncate_to_width(s, 2, "", false), s);
        assert_eq!(slice_by_column(s, 0, 2), s);
        assert_eq!(sanitize_terminal_text(s, false), "\u{00E9}");
        assert_eq!(ansi_code_len("\x1bOPrest", 0), 3);
    }

    #[test]
    fn sanitize_terminal_text_strips_escapes_controls_and_bidi() {
        let payload = concat!(
            "\x1b[31mred\x1b[0m ",
            "\x1b]0;title\x07",
            "\x1b_mehh:c\x07",
            "\x1bP1$q\x1b\\",
            "\x1b^pm\x1b\\",
            "\x1bOP",
            "\x1bc",
            "ok\x1b",
            "\x00\x07\x08\r\n\t",
            "\u{0080}\u{009B}\u{009F}",
            "\u{202A}a\u{202E}b\u{2066}c\u{2069}",
            "\x7f"
        );
        assert_eq!(sanitize_terminal_text(payload, true), "red ok\n\tabc");
        assert_eq!(sanitize_terminal_text(payload, false), "red ok\tabc");
    }

    #[test]
    fn wrap_text_with_ansi_preserves_styles_and_resets_hyperlinks_across_multiline_fast_path() {
        let styled = "\x1b[31m\x1b[4mline1\nline2\nline3\x1b[0m";
        let lines = wrap_text_with_ansi(styled, 20);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "\x1b[31m\x1b[4mline1\x1b[24m");
        assert_eq!(lines[1], "\x1b[4;31mline2\x1b[24m");
        assert_eq!(lines[2], "\x1b[4;31mline3\x1b[0m");

        let linked = "\x1b]8;;https://example.com\x07first\nsecond\x1b]8;;\x07";
        let link_lines = wrap_text_with_ansi(linked, 20);
        assert_eq!(link_lines.len(), 2);
        assert_eq!(
            link_lines[0],
            "\x1b]8;;https://example.com\x07first\x1b]8;;\x07"
        );
        assert_eq!(
            link_lines[1],
            "\x1b]8;;https://example.com\x07second\x1b]8;;\x07"
        );
    }

    #[test]
    fn grapheme_truncation_wrapping_and_slicing_preserve_clusters_and_end_col() {
        // Combining mark after ASCII must not be detached
        let comb = "e\u{0301}bc";
        assert_eq!(truncate_to_width(comb, 2, "\u{2026}", false), "e\u{0301}\u{2026}");
        assert_eq!(slice_by_column(comb, 0, 2), "e\u{0301}b");

        // VS16 emoji (width 2) must not be split or overflow
        let vs16 = "a\u{2764}\u{FE0F}z";
        assert_eq!(truncate_to_width(vs16, 2, "", false), "a");
        assert_eq!(truncate_to_width(vs16, 3, "", false), "a\u{2764}\u{FE0F}");
        assert_eq!(slice_by_column(vs16, 0, 2), "a");
        assert_eq!(slice_by_column(vs16, 0, 3), "a\u{2764}\u{FE0F}");

        // ZWJ family emoji (width 2) stays intact across truncation, slicing, and break_long_word
        let zwj = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let two_zwj = format!("{zwj}{zwj}");
        assert_eq!(truncate_to_width(&two_zwj, 3, "\u{2026}", false), format!("{zwj}\u{2026}"));
        assert_eq!(slice_by_column(&format!("x{zwj}y"), 0, 2), "x");
        assert_eq!(slice_by_column(&format!("x{zwj}y"), 1, 3), zwj);
        let wrapped = wrap_text_with_ansi(&two_zwj, 2);
        assert_eq!(wrapped, vec![zwj.to_string(), zwj.to_string()]);
    }

    #[test]
    fn osc8_tracker_handles_short_and_unterminated_multibyte_sequences_without_panic() {
        let mut tracker = AnsiCodeTracker::default();
        for bad in ["\x1b]8;", "\x1b]8;a", "\x1b]8;;\u{00E9}", "\x1b]8;id=1;https://a\u{1F980}"] {
            tracker.process(bad);
            assert!(tracker.active_hyperlink.is_none());
        }
        tracker.process("\x1b]8;id=1;https://example.com\x07");
        assert_eq!(
            tracker.active_hyperlink,
            Some((
                "id=1".to_string(),
                "https://example.com".to_string(),
                "\x07".to_string()
            ))
        );
        tracker.process("\x1b]8;;\x1b\\");
        assert!(tracker.active_hyperlink.is_none());
    }

    #[test]
    fn wrap_single_line_fixes_apc_styles_trailing_blank_final_reset_and_wide_spaces() {
        // 1. Preserve APC sequence (CURSOR_MARKER) from dropped whitespace token
        let apc_line = "hello \x1b_mehh:c\x07 world";
        let wrapped_apc = wrap_text_with_ansi(apc_line, 5);
        assert_eq!(wrapped_apc.len(), 2, "{wrapped_apc:?}");
        assert_eq!(wrapped_apc[0], "hello");
        assert!(
            wrapped_apc[1].contains("\x1b_mehh:c\x07"),
            "APC cursor marker lost on wrap: {wrapped_apc:?}"
        );
        assert_eq!(strip_ansi(&wrapped_apc[1]), "world");

        // 2. Call process_ansi_in_str at the right time so dropped space resets/updates styles
        let reset_in_space = "\x1b[31mhello \x1b[0m world";
        let wrapped_reset = wrap_text_with_ansi(reset_in_space, 5);
        assert_eq!(wrapped_reset.len(), 2, "{wrapped_reset:?}");
        assert!(!wrapped_reset[1].contains("\x1b[31m"), "{wrapped_reset:?}");

        // 3. Do not emit phantom trailing style-only blank lines
        let trailing_style = "\x1b[31mhello world   ";
        let wrapped_trailing = wrap_text_with_ansi(trailing_style, 11);
        assert_eq!(
            wrapped_trailing,
            vec!["\x1b[31mhello world".to_string()],
            "phantom trailing style-only blank line emitted: {wrapped_trailing:?}"
        );

        // 4. Append tracker.get_line_end_reset() to the final wrapped line
        let linked = "\x1b]8;;https://example.com\x07hello world\nsecond\x1b]8;;\x07";
        let wrapped_link = wrap_text_with_ansi(linked, 8);
        assert_eq!(wrapped_link.len(), 3, "{wrapped_link:?}");
        assert!(
            wrapped_link[1].ends_with("\x1b]8;;\x07"),
            "final wrapped line missing line-end reset: {wrapped_link:?}"
        );

        // 5. Break whitespace tokens wider than width
        let wide_space = "      \x1b_mehh:c\x07";
        let wrapped_space = wrap_text_with_ansi(wide_space, 4);
        assert!(
            wrapped_space.iter().all(|l| visible_width(l) <= 4),
            "wide whitespace token overflowed width: {wrapped_space:?}"
        );
        assert!(
            wrapped_space.iter().any(|l| l.contains("\x1b_mehh:c\x07")),
            "APC lost in wide whitespace token: {wrapped_space:?}"
        );
    }

    #[test]
    fn truncate_to_width_does_not_process_unemitted_ansi_codes() {
        // Closing OSC 8 and SGR are buffered before wide char that causes break;
        // tracker must NOT process them since they were not emitted.
        let s = "\x1b[31m\x1b]8;;https://example.com\x07\u{4E16}\x1b]8;;\x07\x1b[0m\u{754C}";
        let out = truncate_to_width(s, 3, "\u{2026}", false);
        assert!(
            out.contains("\x1b]8;;\x07"),
            "unclosed OSC 8 hyperlink: {out:?}"
        );
        assert!(out.ends_with("\x1b[0m"), "unclosed SGR: {out:?}");

        // Opening SGR buffered before wide char that causes break must not emit spurious reset
        let s2 = "\u{4E16}\x1b[31m\u{754C}";
        let out2 = truncate_to_width(s2, 3, "\u{2026}", false);
        assert_eq!(out2, "\u{4E16}\u{2026}");
    }
}
