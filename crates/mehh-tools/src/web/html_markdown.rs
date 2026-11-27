//! Zero-dependency, bounded streaming HTML-to-Markdown and text extractor.

use std::borrow::Cow;

use reqwest::Url;

pub const MAX_HTML_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_NESTING_DEPTH: usize = 256;
const MIN_FAILED_TAG_SCAN_BUDGET: usize = 16 * 1024;
const MAX_TABLE_COLUMNS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered(usize),
}

#[derive(Default)]
struct TableBuffer {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_cell: bool,
    buffered_bytes: usize,
}

#[derive(Debug)]
struct LinkCapture {
    href: Option<String>,
    buf: String,
}

/// Converts an HTML document into bounded GitHub-Flavored Markdown.
pub fn html_to_markdown(html: &str, base_url: Option<&Url>) -> String {
    let input_limit = crate::core::output::floor_char_boundary(html, MAX_HTML_BYTES);
    let src = &html[..input_limit];
    let mut conv = MdConverter::new(base_url, src.len());
    scan_html(src, &mut conv);
    conv.finish()
}

/// Extracts plain readable text from an HTML document, dropping scripts, styles, and embeds.
pub fn html_to_text(html: &str) -> String {
    let input_limit = crate::core::output::floor_char_boundary(html, MAX_HTML_BYTES);
    let src = &html[..input_limit];
    let mut extractor = TextExtractor::with_capacity(src.len());
    scan_html(src, &mut extractor);
    collapse_blank_lines(extractor.out, MAX_HTML_BYTES)
}

/// Zero-allocation ASCII case-insensitive substring check.
#[inline]
pub(crate) fn contains_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> bool {
    let nlen = needle_lower.len();
    if nlen == 0 {
        return true;
    }
    if haystack.len() < nlen {
        return false;
    }
    let first = needle_lower[0];
    let limit = haystack.len() - nlen;
    let mut i = 0;
    while i <= limit {
        if haystack[i].to_ascii_lowercase() == first
            && haystack[i + 1..i + nlen].eq_ignore_ascii_case(&needle_lower[1..])
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Strips common Markdown formatting syntax into plain readable text.
pub fn strip_markdown_to_text(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    let mut first = true;
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
        if in_fence {
            out.push_str(line);
            continue;
        }
        let s = trimmed.trim_start_matches('#').trim_start();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + 1 < bytes.len()
                && ((bytes[i] == b'*' && bytes[i + 1] == b'*')
                    || (bytes[i] == b'_' && bytes[i + 1] == b'_')
                    || (bytes[i] == b'~' && bytes[i + 1] == b'~'))
            {
                i += 2;
                continue;
            }
            if bytes[i] == b'`' {
                i += 1;
                continue;
            }
            let ch = s[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    collapse_blank_lines(out, MAX_HTML_BYTES)
}

/// Returns true when `raw_html` produced almost no visible content yet carries SPA shell markers.
pub fn looks_like_empty_spa_shell(raw_html: &str, rendered: &str) -> bool {
    if rendered.chars().filter(|c| !c.is_whitespace()).nth(199).is_some() {
        return false;
    }
    let scan_limit = crate::core::output::floor_char_boundary(raw_html, 256 * 1024);
    let bytes = &raw_html.as_bytes()[..scan_limit];
    contains_ascii_ci(bytes, b"<script")
        || contains_ascii_ci(bytes, b"enable javascript")
        || contains_ascii_ci(bytes, b"__next_data__")
        || contains_ascii_ci(bytes, b"id=\"root\"")
        || contains_ascii_ci(bytes, b"id='root'")
        || contains_ascii_ci(bytes, b"id=\"app\"")
        || contains_ascii_ci(bytes, b"id='app'")
        || contains_ascii_ci(bytes, b"id=\"__next\"")
}

pub(crate) trait HtmlSink {
    fn on_start_tag(&mut self, name: &str, raw_attrs: &str, self_closing: bool);
    fn on_end_tag(&mut self, name: &str);
    fn on_text(&mut self, text: &str);
}

fn has_ascii_uppercase(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut k = 0;
    while k < bytes.len() {
        if bytes[k].is_ascii_uppercase() {
            return true;
        }
        k += 1;
    }
    false
}

fn lower_ascii_into(s: &str, buf: &mut [u8; 32], heap: &mut Option<String>) -> usize {
    if s.len() <= buf.len() {
        for (idx, b) in s.bytes().enumerate() {
            buf[idx] = b.to_ascii_lowercase();
        }
        *heap = None;
        s.len()
    } else {
        *heap = Some(s.to_ascii_lowercase());
        0
    }
}

pub(crate) fn scan_html(src: &str, sink: &mut impl HtmlSink) {
    let bytes = src.as_bytes();
    let mut pos = 0;
    let mut failed_tag_scan_budget = src.len().saturating_mul(2).max(MIN_FAILED_TAG_SCAN_BUDGET);
    let mut name_buf = [0u8; 32];
    let mut name_heap: Option<String> = None;

    while pos < bytes.len() {
        if let Some(rel_lt) = src[pos..].find('<') {
            if rel_lt > 0 {
                sink.on_text(&src[pos..pos + rel_lt]);
                pos += rel_lt;
            }
            if src[pos..].starts_with("<!--") {
                if let Some(end_comment) = src[pos + 4..].find("-->") {
                    pos += 4 + end_comment + 3;
                } else {
                    break;
                }
                continue;
            }
            if bytes.get(pos + 1).is_some_and(|b| *b == b'!' || *b == b'?') {
                if let Some(end_decl) = src[pos..].find('>') {
                    pos += end_decl + 1;
                } else {
                    break;
                }
                continue;
            }
            let starts_tag = bytes
                .get(pos + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'/');
            if starts_tag {
                if let Some((tag_end, raw_tag)) =
                    read_tag_slice(src, pos, &mut failed_tag_scan_budget)
                {
                    pos = tag_end;
                    let inner = raw_tag.trim();
                    if let Some(closing) = inner.strip_prefix('/') {
                        let raw_name = closing
                            .split(|c: char| c.is_ascii_whitespace())
                            .next()
                            .unwrap_or("");
                        if !raw_name.is_empty() {
                            let name: &str = if !has_ascii_uppercase(raw_name) {
                                raw_name
                            } else {
                                let buf_len =
                                    lower_ascii_into(raw_name, &mut name_buf, &mut name_heap);
                                match name_heap {
                                    Some(ref h) => h.as_str(),
                                    None => std::str::from_utf8(&name_buf[..buf_len]).unwrap_or(""),
                                }
                            };
                            if !name.is_empty() {
                                sink.on_end_tag(name);
                            }
                        }
                    } else if !inner.is_empty() {
                        let self_closing = inner.ends_with('/');
                        let body = if self_closing {
                            inner[..inner.len() - 1].trim_end()
                        } else {
                            inner
                        };
                        let (raw_name, raw_attrs) = split_tag_name_and_attrs(body);
                        if !raw_name.is_empty() {
                            let name: &str = if !has_ascii_uppercase(raw_name) {
                                raw_name
                            } else {
                                let buf_len =
                                    lower_ascii_into(raw_name, &mut name_buf, &mut name_heap);
                                match name_heap {
                                    Some(ref h) => h.as_str(),
                                    None => std::str::from_utf8(&name_buf[..buf_len]).unwrap_or(""),
                                }
                            };
                            if !name.is_empty() {
                                let is_raw_text =
                                    !self_closing && matches!(name, "script" | "style");
                                sink.on_start_tag(name, raw_attrs, self_closing);
                                if is_raw_text {
                                    if let Some(close_end) = skip_raw_text_element(src, pos, name) {
                                        pos = close_end;
                                        sink.on_end_tag(name);
                                    } else {
                                        // Unclosed <script>/<style>: consume to EOF cleanly.
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
            }
            sink.on_text(&src[pos..pos + 1]);
            pos += 1;
        } else {
            sink.on_text(&src[pos..]);
            break;
        }
    }
}

fn read_tag_slice<'a>(
    src: &'a str,
    lt_pos: usize,
    failed_budget: &mut usize,
) -> Option<(usize, &'a str)> {
    let bytes = src.as_bytes();
    let mut i = lt_pos + 1;
    if *failed_budget == 0 {
        // Fallback O(N) unquoted scan so exhausted budgets still isolate subsequent <script>/<style> tags.
        while i < bytes.len() {
            match bytes[i] {
                b'>' => return Some((i + 1, &src[lt_pos + 1..i])),
                b'<' => return None,
                _ => i += 1,
            }
        }
        return None;
    }

    let mut scanned = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        scanned += 1;
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some((i + 1, &src[lt_pos + 1..i])),
            _ => {}
        }
        if scanned >= *failed_budget {
            *failed_budget = 0;
            return None;
        }
        i += 1;
    }
    *failed_budget = failed_budget.saturating_sub(scanned);
    None
}

fn skip_raw_text_element(src: &str, start: usize, tag_name: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let needle: &[u8] = match tag_name {
        "script" => b"</script",
        "style" => b"</style",
        _ => return None,
    };
    let mut i = start;
    while i < src.len() {
        let rel = src[i..].find("</")?;
        let abs = i + rel;
        let rest = &src[abs..];
        if rest
            .as_bytes()
            .get(..needle.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(needle))
        {
            let after = abs + needle.len();
            if let Some(b) = bytes.get(after) {
                if *b == b'>' || b.is_ascii_whitespace() {
                    if let Some(rel_gt) = src[after..].find('>') {
                        return Some(after + rel_gt + 1);
                    }
                }
            }
        }
        i = abs + 2;
    }
    None
}

fn split_tag_name_and_attrs(tag_body: &str) -> (&str, &str) {
    let bytes = tag_body.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            return (&tag_body[..idx], &tag_body[idx..]);
        }
        idx += 1;
    }
    (tag_body, "")
}

fn find_attr_raw<'a>(raw_attrs: &'a str, target_key: &str) -> Option<&'a str> {
    let bytes = raw_attrs.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let key = &raw_attrs[key_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut raw_val = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                i += 1;
                let v_start = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                raw_val = &raw_attrs[v_start..i];
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let v_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                raw_val = &raw_attrs[v_start..i];
            }
        }
        if !key.is_empty() && key.eq_ignore_ascii_case(target_key) {
            return Some(raw_val);
        }
    }

    None
}

pub(crate) fn find_attr_decoded<'a>(raw_attrs: &'a str, key: &str) -> Option<Cow<'a, str>> {
    find_attr_raw(raw_attrs, key).map(decode_entities)
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn match_skip_subtree_tag(name: &str) -> Option<&'static str> {
    match name {
        "script" => Some("script"),
        "style" => Some("style"),
        "noscript" => Some("noscript"),
        "template" => Some("template"),
        "iframe" => Some("iframe"),
        "object" => Some("object"),
        "embed" => Some("embed"),
        "meta" => Some("meta"),
        "link" => Some("link"),
        _ => None,
    }
}

#[derive(Default)]
struct SkipTracker {
    root_tag: Option<&'static str>,
    depth: usize,
}

impl SkipTracker {
    fn is_skipping(&self) -> bool {
        self.root_tag.is_some()
    }

    fn on_start_tag(&mut self, name: &str, self_closing: bool) -> bool {
        if let Some(root) = self.root_tag {
            if !self_closing && !is_void_tag(name) && root == name {
                self.depth += 1;
            }
            return true;
        }
        if let Some(skip_tag) = match_skip_subtree_tag(name) {
            if !self_closing && !is_void_tag(name) {
                self.root_tag = Some(skip_tag);
                self.depth = 1;
            }
            return true;
        }
        false
    }

    fn on_end_tag(&mut self, name: &str) -> bool {
        if let Some(root) = self.root_tag {
            if root == name {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.root_tag = None;
                }
            }
            return true;
        }
        false
    }
}

struct MdConverter<'a> {
    base_url: Option<&'a Url>,
    out: String,
    scratch: String,
    skip: SkipTracker,
    pre_depth: usize,
    blockquote_depth: usize,
    list_stack: Vec<ListKind>,
    list_overflow: usize,
    link_stack: Vec<LinkCapture>,
    link_overflow: usize,
    table: Option<TableBuffer>,
    pending_code_lang: Option<String>,
    pre_has_inner_code: bool,
}

impl<'a> MdConverter<'a> {
    fn new(base_url: Option<&'a Url>, src_len: usize) -> Self {
        Self {
            base_url,
            out: String::with_capacity((src_len / 2).min(64 * 1024)),
            scratch: String::with_capacity(256),
            skip: SkipTracker::default(),
            pre_depth: 0,
            blockquote_depth: 0,
            list_stack: Vec::new(),
            list_overflow: 0,
            link_stack: Vec::new(),
            link_overflow: 0,
            table: None,
            pending_code_lang: None,
            pre_has_inner_code: false,
        }
    }

    fn emit(&mut self, s: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            if link.buf.len() < 4096 {
                link.buf.push_str(s);
            }
            return;
        }
        if let Some(table) = self.table.as_mut() {
            if table.in_cell {
                if table.current_cell.len() < 8192 {
                    table.current_cell.push_str(s);
                }
                return;
            }
        }
        if self.out.len() < MAX_HTML_BYTES {
            self.out.push_str(s);
        }
    }

    fn ensure_newlines(&mut self, count: usize) {
        if self.link_stack.last().is_some()
            || self.table.as_ref().is_some_and(|t| t.in_cell)
        {
            return;
        }
        if self.out.is_empty() {
            return;
        }
        let bytes = self.out.as_bytes();
        let mut idx = bytes.len();
        let mut trailing = 0usize;
        while idx > 0 && bytes[idx - 1] == b'\n' {
            trailing += 1;
            idx -= 1;
        }
        for _ in trailing..count {
            self.emit("\n");
            let qd = self.blockquote_depth.min(MAX_NESTING_DEPTH);
            for _ in 0..qd {
                self.emit("> ");
            }
        }
    }

    fn flush_link(&mut self, link: LinkCapture) {
        let label = link.buf.trim();
        match (label.is_empty(), link.href) {
            (false, Some(href)) => {
                self.emit("[");
                self.emit(label);
                self.emit("](");
                self.emit(&href);
                self.emit(")");
            }
            (false, None) => self.emit(label),
            (true, Some(href)) => self.emit(&href),
            (true, None) => {}
        }
    }

    fn finalize_cell(table: &mut TableBuffer) {
        table.in_cell = false;
        let raw = std::mem::take(&mut table.current_cell);
        if table.current_row.len() >= MAX_TABLE_COLUMNS || table.buffered_bytes >= MAX_HTML_BYTES {
            return;
        }
        let mut cell = String::with_capacity(raw.len());
        let mut prev_ws = true;
        for ch in raw.chars() {
            if ch.is_whitespace() {
                if !prev_ws {
                    cell.push(' ');
                    prev_ws = true;
                }
            } else {
                if ch == '|' {
                    cell.push_str("\\|");
                } else {
                    cell.push(ch);
                }
                prev_ws = false;
            }
        }
        if cell.ends_with(' ') {
            cell.pop();
        }
        let remaining = MAX_HTML_BYTES.saturating_sub(table.buffered_bytes);
        if cell.len() > remaining {
            let cut = crate::core::output::floor_char_boundary(&cell, remaining);
            cell.truncate(cut);
        }
        table.buffered_bytes = table.buffered_bytes.saturating_add(cell.len());
        table.current_row.push(cell);
    }

    fn finalize_row(table: &mut TableBuffer) {
        if table.in_cell {
            Self::finalize_cell(table);
        }
        if !table.current_row.is_empty() {
            let row = std::mem::take(&mut table.current_row);
            table.rows.push(row);
        }
    }

    fn flush_table(&mut self, mut table: TableBuffer) {
        Self::finalize_row(&mut table);
        if table.rows.is_empty() {
            return;
        }
        let cols = table
            .rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_TABLE_COLUMNS);
        for (idx, row) in table.rows.iter().enumerate() {
            if self.out.len() >= MAX_HTML_BYTES {
                break;
            }
            self.emit("| ");
            for c in 0..cols {
                if self.out.len() >= MAX_HTML_BYTES {
                    break;
                }
                if c > 0 {
                    self.emit(" | ");
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                self.emit(cell);
            }
            self.emit(" |\n");
            if idx == 0 {
                self.emit("|");
                for _ in 0..cols {
                    if self.out.len() >= MAX_HTML_BYTES {
                        break;
                    }
                    self.emit(" --- |");
                }
                self.emit("\n");
            }
        }
        self.ensure_newlines(2);
    }

    fn resolve_url(&self, raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed
                .get(..11)
                .is_some_and(|p| p.eq_ignore_ascii_case("javascript:"))
        {
            return None;
        }
        if trimmed
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("data:"))
        {
            return None;
        }
        if let Some(base) = self.base_url {
            base.join(trimmed).ok().map(String::from)
        } else {
            Some(trimmed.to_string())
        }
    }

    fn finish(mut self) -> String {
        while let Some(link) = self.link_stack.pop() {
            self.flush_link(link);
        }
        if let Some(table) = self.table.take() {
            self.flush_table(table);
        }
        if self.pre_depth > 0 {
            if !self.out.ends_with('\n') {
                self.emit("\n");
            }
            self.emit("```");
        }
        collapse_blank_lines(self.out, MAX_HTML_BYTES)
    }
}

impl HtmlSink for MdConverter<'_> {
    fn on_start_tag(&mut self, name: &str, raw_attrs: &str, self_closing: bool) {
        if self.skip.on_start_tag(name, self_closing) {
            return;
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = (name.as_bytes()[1].saturating_sub(b'0') as usize).clamp(1, 6);
                self.ensure_newlines(2);
                self.emit(&"######"[..level]);
                self.emit(" ");
            }
            "p" | "section" | "article" | "main" => self.ensure_newlines(2),
            "div" | "header" | "footer" | "nav" | "aside" => self.ensure_newlines(1),
            "br" => self.emit("\n"),
            "hr" => {
                self.ensure_newlines(2);
                self.emit("---");
                self.ensure_newlines(2);
            }
            "strong" | "b" => {
                if self_closing {
                    return;
                }
                self.emit("**");
            }
            "em" | "i" => {
                if self_closing {
                    return;
                }
                self.emit("*");
            }
            "del" | "s" | "strike" => {
                if self_closing {
                    return;
                }
                self.emit("~~");
            }
            "blockquote" => {
                if self_closing {
                    return;
                }
                self.ensure_newlines(2);
                self.blockquote_depth += 1;
                if self.blockquote_depth <= MAX_NESTING_DEPTH {
                    self.emit("> ");
                }
            }
            "pre" => {
                if self_closing {
                    return;
                }
                self.ensure_newlines(2);
                self.pre_depth += 1;
                self.pre_has_inner_code = false;
                self.pending_code_lang = extract_language_class(raw_attrs);
                if self.pre_depth == 1 {
                    let lang = self.pending_code_lang.take();
                    self.emit("```");
                    if let Some(ref l) = lang {
                        self.emit(l);
                    }
                    self.emit("\n");
                    self.pending_code_lang = lang;
                }
            }
            "code" => {
                if self_closing {
                    return;
                }
                if self.pre_depth > 0 {
                    if !self.pre_has_inner_code {
                        self.pre_has_inner_code = true;
                        if self.pending_code_lang.is_none() {
                            if let Some(lang) = extract_language_class(raw_attrs) {
                                // Append detected language to opening fence if nothing was emitted yet.
                                if self.out.ends_with("```\n") {
                                    self.out.pop();
                                    self.out.push_str(&lang);
                                    self.out.push('\n');
                                }
                            }
                        }
                    }
                } else {
                    self.emit("`");
                }
            }
            "a" => {
                let href = find_attr_decoded(raw_attrs, "href")
                    .and_then(|h| self.resolve_url(&h));
                if self_closing {
                    if let Some(href) = href {
                        self.emit(&href);
                    }
                    return;
                }
                if self.link_stack.len() < MAX_NESTING_DEPTH {
                    self.link_stack.push(LinkCapture {
                        href,
                        buf: String::new(),
                    });
                } else {
                    self.link_overflow += 1;
                }
            }
            "img" => {
                let alt_decoded = find_attr_decoded(raw_attrs, "alt");
                let alt = alt_decoded.as_deref().unwrap_or("").trim();
                let raw_src = find_attr_raw(raw_attrs, "src").unwrap_or("").trim();
                let is_raw_data_image = !raw_src.contains('&')
                    && raw_src
                        .get(..11)
                        .is_some_and(|p| p.eq_ignore_ascii_case("data:image/"));
                if is_raw_data_image {
                    if alt.is_empty() {
                        self.emit("[image]");
                    } else {
                        self.emit("[image: ");
                        self.emit(alt);
                        self.emit("]");
                    }
                } else {
                    let src_decoded = decode_entities(raw_src);
                    let src = src_decoded.trim();
                    if src
                        .get(..11)
                        .is_some_and(|p| p.eq_ignore_ascii_case("data:image/"))
                    {
                        if alt.is_empty() {
                            self.emit("[image]");
                        } else {
                            self.emit("[image: ");
                            self.emit(alt);
                            self.emit("]");
                        }
                    } else if let Some(resolved) = self.resolve_url(src) {
                        self.emit("![");
                        self.emit(alt);
                        self.emit("](");
                        self.emit(&resolved);
                        self.emit(")");
                    } else if !alt.is_empty() {
                        self.emit("[image: ");
                        self.emit(alt);
                        self.emit("]");
                    }
                }
            }
            "ul" => {
                if self_closing {
                    return;
                }
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
                if self.list_stack.len() < MAX_NESTING_DEPTH {
                    self.list_stack.push(ListKind::Unordered);
                } else {
                    self.list_overflow += 1;
                }
            }
            "ol" => {
                if self_closing {
                    return;
                }
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
                if self.list_stack.len() < MAX_NESTING_DEPTH {
                    self.list_stack.push(ListKind::Ordered(1));
                } else {
                    self.list_overflow += 1;
                }
            }
            "li" => {
                self.ensure_newlines(1);
                let indent_levels = self.list_stack.len().saturating_sub(1);
                for _ in 0..indent_levels {
                    self.emit("  ");
                }
                match self.list_stack.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        use std::fmt::Write as _;
                        let num = *n;
                        *n += 1;
                        let mut scratch = std::mem::take(&mut self.scratch);
                        scratch.clear();
                        let _ = write!(scratch, "{num}. ");
                        self.emit(&scratch);
                        self.scratch = scratch;
                    }
                    _ => self.emit("- "),
                }
            }
            "table" => {
                if self_closing {
                    return;
                }
                if self.table.is_none() {
                    self.ensure_newlines(2);
                    self.table = Some(TableBuffer::default());
                }
            }
            "tr" => {
                if self_closing {
                    return;
                }
                if let Some(table) = self.table.as_mut() {
                    Self::finalize_row(table);
                }
            }
            "th" | "td" => {
                if let Some(table) = self.table.as_mut() {
                    if table.in_cell {
                        Self::finalize_cell(table);
                    }
                    table.current_cell.clear();
                    if self_closing {
                        Self::finalize_cell(table);
                    } else {
                        table.in_cell = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_end_tag(&mut self, name: &str) {
        if self.skip.on_end_tag(name) {
            return;
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "section" | "article" | "main" => {
                self.ensure_newlines(2);
            }
            "div" | "header" | "footer" | "nav" | "aside" => self.ensure_newlines(1),
            "strong" | "b" => self.emit("**"),
            "em" | "i" => self.emit("*"),
            "del" | "s" | "strike" => self.emit("~~"),
            "blockquote" => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.ensure_newlines(2);
            }
            "pre" => {
                if self.pre_depth == 1 {
                    if !self.out.ends_with('\n') {
                        self.emit("\n");
                    }
                    self.emit("```");
                    self.ensure_newlines(2);
                    self.pending_code_lang = None;
                }
                self.pre_depth = self.pre_depth.saturating_sub(1);
            }
            "code" => {
                if self.pre_depth == 0 {
                    self.emit("`");
                }
            }
            "a" => {
                if self.link_overflow > 0 {
                    self.link_overflow -= 1;
                } else if let Some(link) = self.link_stack.pop() {
                    self.flush_link(link);
                }
            }
            "ul" | "ol" => {
                if self.list_overflow > 0 {
                    self.list_overflow -= 1;
                } else {
                    self.list_stack.pop();
                }
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
            }
            "li" => self.ensure_newlines(1),
            "th" | "td" => {
                if let Some(table) = self.table.as_mut() {
                    if table.in_cell {
                        Self::finalize_cell(table);
                    }
                }
            }
            "tr" => {
                if let Some(table) = self.table.as_mut() {
                    Self::finalize_row(table);
                }
            }
            "table" => {
                if let Some(table) = self.table.take() {
                    self.flush_table(table);
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.skip.is_skipping() {
            return;
        }
        let decoded = decode_entities(text);
        if self.pre_depth > 0 {
            self.emit(&decoded);
            return;
        }
        if is_already_collapsed_ascii_inline(&decoded) {
            if decoded.is_empty()
                || (decoded == " "
                    && (self.out.is_empty() || self.out.ends_with([' ', '\n']))
                    && self.link_stack.is_empty()
                    && !self.table.as_ref().is_some_and(|t| t.in_cell))
            {
                return;
            }
            self.emit(&decoded);
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        collapse_inline_whitespace_into(&decoded, &mut scratch);
        if scratch.is_empty()
            || (scratch == " "
                && (self.out.is_empty() || self.out.ends_with([' ', '\n']))
                && self.link_stack.is_empty()
                && !self.table.as_ref().is_some_and(|t| t.in_cell))
        {
            self.scratch = scratch;
            return;
        }
        self.emit(&scratch);
        self.scratch = scratch;
    }
}

struct TextExtractor {
    out: String,
    scratch: String,
    skip: SkipTracker,
}

impl TextExtractor {
    fn with_capacity(src_len: usize) -> Self {
        Self {
            out: String::with_capacity((src_len / 2).min(64 * 1024)),
            scratch: String::with_capacity(256),
            skip: SkipTracker::default(),
        }
    }
}

impl HtmlSink for TextExtractor {
    fn on_start_tag(&mut self, name: &str, _raw_attrs: &str, self_closing: bool) {
        if self.skip.on_start_tag(name, self_closing) {
            return;
        }
        if matches!(
            name,
            "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "br" | "tr"
        ) && !self.out.is_empty()
            && !self.out.ends_with('\n')
        {
            self.out.push('\n');
        }
    }

    fn on_end_tag(&mut self, name: &str) {
        let _ = self.skip.on_end_tag(name);
    }

    fn on_text(&mut self, text: &str) {
        if self.skip.is_skipping() || self.out.len() >= MAX_HTML_BYTES {
            return;
        }
        let decoded = decode_entities(text);
        if is_already_collapsed_ascii_inline(&decoded) {
            if decoded.is_empty()
                || (decoded == " " && (self.out.is_empty() || self.out.ends_with([' ', '\n'])))
            {
                return;
            }
            self.out.push_str(&decoded);
            return;
        }
        collapse_inline_whitespace_into(&decoded, &mut self.scratch);
        if self.scratch.is_empty()
            || (self.scratch == " " && (self.out.is_empty() || self.out.ends_with([' ', '\n'])))
        {
            return;
        }
        self.out.push_str(&self.scratch);
    }
}

fn is_already_collapsed_ascii_inline(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut idx = 0;
    let mut prev_space = false;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if byte > b' ' && byte < 0x80 {
            prev_space = false;
        } else if byte == b' ' {
            if prev_space {
                return false;
            }
            prev_space = true;
        } else {
            return false;
        }
        idx += 1;
    }
    true
}

fn extract_language_class(raw_attrs: &str) -> Option<String> {
    let cls = find_attr_decoded(raw_attrs, "class")?;
    for token in cls.split_whitespace() {
        if let Some(lang) = token
            .strip_prefix("language-")
            .or_else(|| token.strip_prefix("lang-"))
        {
            let clean: String = lang
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '#'))
                .collect();
            if !clean.is_empty() {
                return Some(clean);
            }
        }
    }
    None
}

fn collapse_inline_whitespace_into(s: &str, out: &mut String) {
    out.clear();
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
}

fn collapse_blank_lines(raw: String, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut empty_run = 0usize;
    for line in raw.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.is_empty() {
            if out.is_empty() {
                continue;
            }
            empty_run += 1;
            if empty_run <= 1 && out.len() < max_bytes {
                out.push('\n');
            }
        } else {
            empty_run = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                if out.len() >= max_bytes {
                    break;
                }
                out.push('\n');
            }
            let avail = max_bytes.saturating_sub(out.len());
            if avail == 0 {
                break;
            }
            let cut = crate::core::output::floor_char_boundary(trimmed_end, avail);
            out.push_str(&trimmed_end[..cut]);
            if out.len() < max_bytes {
                out.push('\n');
            }
        }
    }
    let trimmed_end = out.trim_end().len();
    out.truncate(trimmed_end);
    let leading_ws = out.len() - out.trim_start().len();
    if leading_ws > 0 {
        out.drain(..leading_ws);
    }
    out
}

pub(crate) fn decode_entities(input: &str) -> Cow<'_, str> {
    if !input.contains('&') {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let max_scan = rest.len().min(14);
        let safe_scan = crate::core::output::floor_char_boundary(rest, max_scan);
        if let Some(semi) = rest[1..safe_scan].find(';') {
            let entity = &rest[1..1 + semi];
            if let Some(ch) = decode_one_entity(entity) {
                out.push(ch);
                rest = &rest[2 + semi..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn decode_one_entity(entity: &str) -> Option<char> {
    if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        return u32::from_str_radix(hex, 16)
            .ok()
            .filter(|c| *c != 0)
            .and_then(char::from_u32);
    }
    if let Some(dec) = entity.strip_prefix('#') {
        return dec
            .parse::<u32>()
            .ok()
            .filter(|c| *c != 0)
            .and_then(char::from_u32);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "copy" => '\u{00A9}',
        "reg" => '\u{00AE}',
        "trade" => '\u{2122}',
        "laquo" => '\u{00AB}',
        "raquo" => '\u{00BB}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "bull" => '\u{2022}',
        "middot" => '\u{00B7}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_markdown_core_elements() {
        let base = Url::parse("https://docs.rs/pkg/").unwrap();
        let html = r#"
            <h1>Title &amp; Overview</h1>
            <p>Read the <strong>fast</strong> <em>async</em> <code>client</code> or <a href="/guide">Guide</a>.</p>
            <pre><code class="language-rust">fn main() { println!("hi"); }</code></pre>
            <ul><li>One</li><li>Two<ul><li>Nested</li></ul></li></ul>
            <ol><li>First</li><li>Second</li></ol>
            <blockquote>Quote line</blockquote>
            <hr>
            <table><tr><th>Col A</th><th>Col B</th></tr><tr><td>Val &lt;1&gt;</td><td>&nbsp;&#x27;ok&#39;</td></tr></table>
        "#;
        let md = html_to_markdown(html, Some(&base));
        assert!(md.contains("# Title & Overview"));
        assert!(md.contains("**fast** *async* `client`"));
        assert!(md.contains("[Guide](https://docs.rs/guide)"));
        assert!(md.contains("```rust\nfn main() { println!(\"hi\"); }\n```"));
        assert!(md.contains("- One"));
        assert!(md.contains("  - Nested"));
        assert!(md.contains("1. First"));
        assert!(md.contains("2. Second"));
        assert!(md.contains("> Quote line"));
        assert!(md.contains("---"));
        assert!(md.contains("| Col A | Col B |"));
        assert!(md.contains("| Val <1> | 'ok' |"));
    }

    #[test]
    fn test_html_to_markdown_adversarial_inputs() {
        let deep_open = "<div>".repeat(1000);
        let deep_close = "</div>".repeat(1000);
        let huge_nested = format!(
            "{deep_open}<p>Deep safe content</p>{deep_close}\
             <img alt=\"arch diagram\" src=\"data:image/png;base64,AAAAABBBBCCCC\">\
             <script>var secret = 1; if (a < b) {{ alert('xss'); }}"
        );
        let md = html_to_markdown(&huge_nested, None);
        assert!(md.contains("Deep safe content"));
        assert!(md.contains("[image: arch diagram]"));
        assert!(!md.contains("AAAAABBBBCCCC"));
        assert!(!md.contains("secret"));
        assert!(!md.contains("alert"));
    }

    #[test]
    fn test_html_to_text_skips_noise_tags() {
        let html = "<html><head><style>body{display:none}</style></head><body><script>bad()</script><p>Hello</p><iframe src=\"x\">hidden</iframe><p>World</p></body></html>";
        let txt = html_to_text(html);
        assert!(txt.contains("Hello"));
        assert!(txt.contains("World"));
        assert!(!txt.contains("bad()"));
        assert!(!txt.contains("display:none"));
        assert!(!txt.contains("hidden"));
    }

    #[test]
    fn test_looks_like_empty_spa_shell() {
        let spa = "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";
        assert!(looks_like_empty_spa_shell(spa, ""));

        let static_short = "<html><body><p>Hello world</p></body></html>";
        assert!(!looks_like_empty_spa_shell(static_short, "Hello world"));

        let large_text = "x".repeat(250);
        assert!(!looks_like_empty_spa_shell(spa, &large_text));
    }

    #[test]
    fn test_utf8_in_raw_text_end_tag_check_and_linear_time_and_skip_recovery() {
        // 1. Multi-byte UTF-8 following "</" inside <script> and <style> must not panic.
        let html_utf8 = "<script>const s = \"</a\u{4e2d}\u{6587}\";</script><p>ok1</p>\
                         <style>/* </a\u{4e2d}\u{6587} */</style><p>ok2</p>";
        let md_utf8 = html_to_markdown(html_utf8, None);
        assert!(md_utf8.contains("ok1"));
        assert!(md_utf8.contains("ok2"));

        // 2. Prose "<" followed by apostrophes must not swallow closing tags.
        let prose = "<p>x < y and don't worry</p><p>it's ok</p>";
        let md_prose = html_to_markdown(prose, None);
        assert!(md_prose.contains("don't worry"), "got: {md_prose}");
        assert!(md_prose.contains("it's ok"), "got: {md_prose}");

        // 3. Adversarial unclosed '<' / quoted tag streams must finish in linear time.
        let lt_flood = "<".repeat(40_000);
        let start = std::time::Instant::now();
        let _ = html_to_markdown(&lt_flood, None);
        assert!(start.elapsed().as_millis() < 500, "took {:?}", start.elapsed());

        let quote_flood = "<a\"".repeat(10_000);
        let start2 = std::time::Instant::now();
        let _ = html_to_markdown(&quote_flood, None);
        assert!(start2.elapsed().as_millis() < 500, "took {:?}", start2.elapsed());

        // 4. Skip tag at depth >= MAX_NESTING_DEPTH and stray outer closing tag inside skip subtree.
        let deep_open = "<div>".repeat(MAX_NESTING_DEPTH);
        let deep_close = "</div>".repeat(MAX_NESTING_DEPTH);
        let deep_skip = format!("{deep_open}<noscript>hidden</noscript><p>after-deep</p>{deep_close}");
        let md_deep = html_to_markdown(&deep_skip, None);
        assert!(!md_deep.contains("hidden"));
        assert!(md_deep.contains("after-deep"), "got: {md_deep}");

        let stray = "<div><noscript></div>secret</noscript><p>visible</p>";
        let md_stray = html_to_markdown(stray, None);
        assert!(!md_stray.contains("secret"), "leaked skip content: {md_stray}");
        assert!(md_stray.contains("visible"), "lost content after skip: {md_stray}");

        // 5. Unclosed <a> and <table> flush at EOF.
        let unclosed_link = html_to_markdown("<a href=\"https://docs.rs\">Docs", None);
        assert!(unclosed_link.contains("[Docs](https://docs.rs)"), "got: {unclosed_link}");
        let unclosed_table = html_to_markdown("<table><tr><td>Cell", None);
        assert!(unclosed_table.contains("Cell"), "got: {unclosed_table}");
    }

    #[test]
    fn test_large_base64_img_budget_exhausted_script_and_entity_table_dos() {
        // 1. >16 KiB base64 <img> must still simplify to [image: alt] without leaking raw base64.
        let big_b64 = "A".repeat(40_000);
        let img_html = format!(
            "<p>Before</p><img alt=\"arch\" src=\"data:image/png;base64,{big_b64}\"><p>After</p>"
        );
        let md_img = html_to_markdown(&img_html, None);
        assert!(md_img.contains("[image: arch]"), "got len {}", md_img.len());
        assert!(!md_img.contains("AAAA"), "leaked base64 payload");
        assert!(md_img.contains("After"));

        // 2. Unclosed <a" flood exhausting scan budget must not leak subsequent <script>/<style>.
        let unclosed_flood = "<a\"".repeat(60);
        let script_after = format!(
            "{unclosed_flood}<p>intro</p><script>SHOULD_NOT_LEAK()</script><p>safe</p>"
        );
        let md_script = html_to_markdown(&script_after, None);
        assert!(
            !md_script.contains("SHOULD_NOT_LEAK"),
            "leaked script content: {md_script}"
        );
        assert!(md_script.contains("safe"));

        // 3. 50k unclosed '&' entities and a sparse multi-column table must complete fast.
        let amp_flood = format!("<p>{}</p>", "&x".repeat(50_000));
        let t_amp = std::time::Instant::now();
        let _ = html_to_markdown(&amp_flood, None);
        assert!(t_amp.elapsed().as_millis() < 500, "took {:?}", t_amp.elapsed());

        let wide_row = format!("<tr>{}</tr>", "<td></td>".repeat(5_000));
        let tall_rows = "<tr><td>v</td></tr>".repeat(1_000);
        let sparse_table = format!("<table>{wide_row}{tall_rows}</table>");
        let t_tbl = std::time::Instant::now();
        let md_tbl = html_to_markdown(&sparse_table, None);
        assert!(t_tbl.elapsed().as_millis() < 500, "took {:?}", t_tbl.elapsed());
        assert!(md_tbl.len() <= MAX_HTML_BYTES + 256);
    }

    #[test]
    fn test_html5_unclosed_table_cells_and_rows() {
        let html = "<table><tr><th>A<th>B<tr><td>1<td>2</table>";
        let md = html_to_markdown(html, None);
        assert_eq!(md, "| A | B |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn test_self_closing_tags_and_inline_spaces_in_table_cells() {
        // C16: self-closing <a/> emits href without capturing subsequent text.
        let md_a = html_to_markdown("<a href=\"https://docs.rs/api\" />after", None);
        assert_eq!(md_a, "https://docs.rs/apiafter");

        // C16: self-closing formatting/block/list/table tags do not leak unclosed state.
        let md_sc = html_to_markdown(
            "<pre/><code/><blockquote/><strong/><b/><em/><i/><del/><s/><strike/><ul/><ol/><table/>plain",
            None,
        );
        assert_eq!(md_sc, "plain");

        // C16: self-closing <td/> / <th/> / <tr/> inside a table.
        let md_tbl = html_to_markdown(
            "<table><tr><th>H1</th><th/></tr><tr/><tr><td>A</td><td/></tr></table>",
            None,
        );
        assert_eq!(md_tbl, "| H1 |  |\n| --- | --- |\n| A |  |");

        // I35: space between inline tags inside <td> is preserved even when out is empty or ends with '\n'.
        let md_td_start = html_to_markdown(
            "<table><tr><td><b>hello</b> <i>world</i></td></tr></table>",
            None,
        );
        assert!(
            md_td_start.contains("**hello** *world*"),
            "got: {md_td_start:?}"
        );

        let md_td_after_p = html_to_markdown(
            "<p>intro</p><table><tr><td><b>hello</b> <i>world</i></td></tr></table>",
            None,
        );
        assert!(
            md_td_after_p.contains("**hello** *world*"),
            "got: {md_td_after_p:?}"
        );
    }
}
