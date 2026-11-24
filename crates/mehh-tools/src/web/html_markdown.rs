//! Zero-dependency, bounded streaming HTML-to-Markdown and text extractor.

use reqwest::Url;

pub const MAX_HTML_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_NESTING_DEPTH: usize = 256;

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
    "source", "track", "wbr",
];

const SKIP_SUBTREE_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "object", "embed", "meta", "link",
];

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
    let mut conv = MdConverter::new(base_url);
    scan_html(src, &mut conv);
    conv.finish()
}

/// Extracts plain readable text from an HTML document, dropping scripts, styles, and embeds.
pub fn html_to_text(html: &str) -> String {
    let input_limit = crate::core::output::floor_char_boundary(html, MAX_HTML_BYTES);
    let src = &html[..input_limit];
    let mut extractor = TextExtractor::default();
    scan_html(src, &mut extractor);
    collapse_blank_lines(&extractor.out, MAX_HTML_BYTES)
}

/// Strips common Markdown formatting syntax into plain readable text.
pub fn strip_markdown_to_text(md: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            continue;
        }
        let without_heading = trimmed.trim_start_matches('#').trim_start();
        let cleaned = without_heading
            .replace("**", "")
            .replace("__", "")
            .replace("~~", "")
            .replace('`', "");
        out.push(cleaned);
    }
    collapse_blank_lines(&out.join("\n"), MAX_HTML_BYTES)
}

/// Returns true when `raw_html` produced almost no visible content yet carries SPA shell markers.
pub fn looks_like_empty_spa_shell(raw_html: &str, rendered: &str) -> bool {
    let non_ws = rendered.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws >= 200 {
        return false;
    }
    let scan_limit = crate::core::output::floor_char_boundary(raw_html, 256 * 1024);
    let lower = raw_html[..scan_limit].to_ascii_lowercase();
    lower.contains("<script")
        || lower.contains("enable javascript")
        || lower.contains("__next_data__")
        || lower.contains("id=\"root\"")
        || lower.contains("id='root'")
        || lower.contains("id=\"app\"")
        || lower.contains("id='app'")
        || lower.contains("id=\"__next\"")
}

trait HtmlSink {
    fn on_start_tag(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool);
    fn on_end_tag(&mut self, name: &str);
    fn on_text(&mut self, text: &str);
}

fn scan_html(src: &str, sink: &mut impl HtmlSink) {
    let bytes = src.as_bytes();
    let mut pos = 0;

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
            if let Some((tag_end, raw_tag)) = read_tag_slice(src, pos) {
                pos = tag_end;
                let inner = raw_tag.trim();
                if let Some(closing) = inner.strip_prefix('/') {
                    let name = closing
                        .split(|c: char| c.is_ascii_whitespace())
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if !name.is_empty() {
                        sink.on_end_tag(&name);
                    }
                } else if !inner.is_empty() {
                    let self_closing = inner.ends_with('/');
                    let body = if self_closing {
                        inner[..inner.len() - 1].trim_end()
                    } else {
                        inner
                    };
                    let (name, attrs) = parse_tag_and_attrs(body);
                    if !name.is_empty() {
                        let is_raw_text = !self_closing && matches!(name.as_str(), "script" | "style");
                        sink.on_start_tag(&name, &attrs, self_closing);
                        if is_raw_text {
                            if let Some(close_end) = skip_raw_text_element(src, pos, &name) {
                                pos = close_end;
                                sink.on_end_tag(&name);
                            } else {
                                // Unclosed <script>/<style>: consume to EOF cleanly.
                                break;
                            }
                        }
                    }
                }
            } else {
                sink.on_text(&src[pos..pos + 1]);
                pos += 1;
            }
        } else {
            sink.on_text(&src[pos..]);
            break;
        }
    }
}

fn read_tag_slice(src: &str, lt_pos: usize) -> Option<(usize, &str)> {
    let bytes = src.as_bytes();
    let mut i = lt_pos + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some((i + 1, &src[lt_pos + 1..i])),
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_raw_text_element(src: &str, start: usize, tag_name: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let needle = format!("</{tag_name}");
    let mut i = start;
    while i < src.len() {
        if let Some(rel) = src[i..].find("</") {
            let abs = i + rel;
            let rest = &src[abs..];
            if rest.len() >= needle.len() && rest[..needle.len()].eq_ignore_ascii_case(&needle) {
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
        } else {
            return None;
        }
    }
    None
}

fn parse_tag_and_attrs(tag_body: &str) -> (String, Vec<(String, String)>) {
    let mut chars = tag_body.char_indices().peekable();
    let mut name_end = tag_body.len();
    for (idx, ch) in &mut chars {
        if ch.is_ascii_whitespace() {
            name_end = idx;
            break;
        }
    }
    let name = tag_body[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let rest = &tag_body[name_end..];
    let bytes = rest.as_bytes();
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
        let key = rest[key_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
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
                value = decode_entities(&rest[v_start..i]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let v_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = decode_entities(&rest[v_start..i]);
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }

    (name, attrs)
}

fn attr_value<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn is_void_tag(name: &str) -> bool {
    VOID_TAGS.contains(&name)
}

struct MdConverter<'a> {
    base_url: Option<&'a Url>,
    out: String,
    open_stack: Vec<String>,
    skip_depth: usize,
    pre_depth: usize,
    blockquote_depth: usize,
    list_stack: Vec<ListKind>,
    link_stack: Vec<LinkCapture>,
    table: Option<TableBuffer>,
    pending_code_lang: Option<String>,
    pre_has_inner_code: bool,
}

impl<'a> MdConverter<'a> {
    fn new(base_url: Option<&'a Url>) -> Self {
        Self {
            base_url,
            out: String::new(),
            open_stack: Vec::new(),
            skip_depth: 0,
            pre_depth: 0,
            blockquote_depth: 0,
            list_stack: Vec::new(),
            link_stack: Vec::new(),
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
        let trailing = self
            .out
            .bytes()
            .rev()
            .take_while(|b| *b == b'\n')
            .count();
        for _ in trailing..count {
            self.emit("\n");
            if self.blockquote_depth > 0 {
                self.emit(&"> ".repeat(self.blockquote_depth));
            }
        }
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

    fn finish(self) -> String {
        collapse_blank_lines(&self.out, MAX_HTML_BYTES)
    }
}

impl HtmlSink for MdConverter<'_> {
    fn on_start_tag(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        if self.skip_depth > 0 {
            if !self_closing && !is_void_tag(name) && self.open_stack.len() < MAX_NESTING_DEPTH {
                self.skip_depth += 1;
                self.open_stack.push(name.to_string());
            }
            return;
        }
        if SKIP_SUBTREE_TAGS.contains(&name) {
            if !self_closing && !is_void_tag(name) {
                self.skip_depth = 1;
                if self.open_stack.len() < MAX_NESTING_DEPTH {
                    self.open_stack.push(name.to_string());
                }
            }
            return;
        }

        if !self_closing && !is_void_tag(name) && self.open_stack.len() < MAX_NESTING_DEPTH {
            self.open_stack.push(name.to_string());
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name.as_bytes()[1].saturating_sub(b'0') as usize;
                self.ensure_newlines(2);
                self.emit(&"#".repeat(level.clamp(1, 6)));
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
            "strong" | "b" => self.emit("**"),
            "em" | "i" => self.emit("*"),
            "del" | "s" | "strike" => self.emit("~~"),
            "blockquote" => {
                self.ensure_newlines(2);
                self.blockquote_depth += 1;
                self.emit("> ");
            }
            "pre" => {
                self.ensure_newlines(2);
                self.pre_depth += 1;
                self.pre_has_inner_code = false;
                self.pending_code_lang = extract_language_class(attrs);
                if self.pre_depth == 1 {
                    let lang = self.pending_code_lang.clone().unwrap_or_default();
                    self.emit(&format!("```{lang}\n"));
                }
            }
            "code" => {
                if self.pre_depth > 0 {
                    if !self.pre_has_inner_code {
                        self.pre_has_inner_code = true;
                        if self.pending_code_lang.is_none() {
                            if let Some(lang) = extract_language_class(attrs) {
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
                let href = attr_value(attrs, "href").and_then(|h| self.resolve_url(h));
                self.link_stack.push(LinkCapture {
                    href,
                    buf: String::new(),
                });
            }
            "img" => {
                let alt = attr_value(attrs, "alt").unwrap_or("").trim();
                let src = attr_value(attrs, "src").unwrap_or("").trim();
                if src
                    .get(..11)
                    .is_some_and(|p| p.eq_ignore_ascii_case("data:image/"))
                {
                    if alt.is_empty() {
                        self.emit("[image]");
                    } else {
                        self.emit(&format!("[image: {alt}]"));
                    }
                } else if let Some(resolved) = self.resolve_url(src) {
                    self.emit(&format!("![{alt}]({resolved})"));
                } else if !alt.is_empty() {
                    self.emit(&format!("[image: {alt}]"));
                }
            }
            "ul" => {
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
                self.list_stack.push(ListKind::Unordered);
            }
            "ol" => {
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
                self.list_stack.push(ListKind::Ordered(1));
            }
            "li" => {
                self.ensure_newlines(1);
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let marker = match self.list_stack.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        s
                    }
                    _ => "- ".to_string(),
                };
                self.emit(&indent);
                self.emit(&marker);
            }
            "table" => {
                if self.table.is_none() {
                    self.ensure_newlines(2);
                    self.table = Some(TableBuffer::default());
                }
            }
            "tr" => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row.clear();
                }
            }
            "th" | "td" => {
                if let Some(table) = self.table.as_mut() {
                    table.current_cell.clear();
                    table.in_cell = true;
                }
            }
            _ => {}
        }
    }

    fn on_end_tag(&mut self, name: &str) {
        if self.skip_depth > 0 {
            if let Some(pos) = self.open_stack.iter().rposition(|t| t == name) {
                let removed = self.open_stack.len() - pos;
                self.open_stack.truncate(pos);
                self.skip_depth = self.skip_depth.saturating_sub(removed);
            }
            return;
        }

        if let Some(pos) = self.open_stack.iter().rposition(|t| t == name) {
            self.open_stack.truncate(pos);
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
                if let Some(link) = self.link_stack.pop() {
                    let label = link.buf.trim();
                    match (label.is_empty(), link.href) {
                        (false, Some(href)) => self.emit(&format!("[{label}]({href})")),
                        (false, None) => self.emit(label),
                        (true, Some(href)) => self.emit(&href),
                        (true, None) => {}
                    }
                }
            }
            "ul" | "ol" => {
                self.list_stack.pop();
                self.ensure_newlines(if self.list_stack.is_empty() { 2 } else { 1 });
            }
            "li" => self.ensure_newlines(1),
            "th" | "td" => {
                if let Some(table) = self.table.as_mut() {
                    table.in_cell = false;
                    let cell = table
                        .current_cell
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace('|', "\\|");
                    table.current_row.push(cell);
                    table.current_cell.clear();
                }
            }
            "tr" => {
                if let Some(table) = self.table.as_mut() {
                    if !table.current_row.is_empty() {
                        let row = std::mem::take(&mut table.current_row);
                        table.rows.push(row);
                    }
                }
            }
            "table" => {
                if let Some(table) = self.table.take() {
                    if !table.rows.is_empty() {
                        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                        for (idx, row) in table.rows.iter().enumerate() {
                            self.emit("| ");
                            for c in 0..cols {
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
                                    self.emit(" --- |");
                                }
                                self.emit("\n");
                            }
                        }
                        self.ensure_newlines(2);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.skip_depth > 0 {
            return;
        }
        let decoded = decode_entities(text);
        if self.pre_depth > 0 {
            self.emit(&decoded);
            return;
        }
        let collapsed = collapse_inline_whitespace(&decoded);
        if collapsed.is_empty() {
            return;
        }
        if collapsed == " "
            && (self.out.is_empty() || self.out.ends_with([' ', '\n']))
            && self.link_stack.is_empty()
        {
            return;
        }
        self.emit(&collapsed);
    }
}

#[derive(Default)]
struct TextExtractor {
    out: String,
    skip_depth: usize,
    open_stack: Vec<String>,
}

impl HtmlSink for TextExtractor {
    fn on_start_tag(&mut self, name: &str, _attrs: &[(String, String)], self_closing: bool) {
        if self.skip_depth > 0 {
            if !self_closing && !is_void_tag(name) && self.open_stack.len() < MAX_NESTING_DEPTH {
                self.skip_depth += 1;
                self.open_stack.push(name.to_string());
            }
            return;
        }
        if SKIP_SUBTREE_TAGS.contains(&name) {
            if !self_closing && !is_void_tag(name) {
                self.skip_depth = 1;
                if self.open_stack.len() < MAX_NESTING_DEPTH {
                    self.open_stack.push(name.to_string());
                }
            }
            return;
        }
        if !self_closing && !is_void_tag(name) && self.open_stack.len() < MAX_NESTING_DEPTH {
            self.open_stack.push(name.to_string());
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
        if self.skip_depth > 0 {
            if let Some(pos) = self.open_stack.iter().rposition(|t| t == name) {
                let removed = self.open_stack.len() - pos;
                self.open_stack.truncate(pos);
                self.skip_depth = self.skip_depth.saturating_sub(removed);
            }
            return;
        }
        if let Some(pos) = self.open_stack.iter().rposition(|t| t == name) {
            self.open_stack.truncate(pos);
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.skip_depth > 0 || self.out.len() >= MAX_HTML_BYTES {
            return;
        }
        let decoded = decode_entities(text);
        let collapsed = collapse_inline_whitespace(&decoded);
        if collapsed.is_empty() || (collapsed == " " && (self.out.is_empty() || self.out.ends_with([' ', '\n']))) {
            return;
        }
        self.out.push_str(&collapsed);
    }
}

fn extract_language_class(attrs: &[(String, String)]) -> Option<String> {
    let cls = attr_value(attrs, "class")?;
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

fn collapse_inline_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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
    out
}

fn collapse_blank_lines(raw: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut empty_run = 0usize;
    for line in raw.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.trim().is_empty() {
            if out.is_empty() {
                continue;
            }
            empty_run += 1;
            if empty_run <= 1 && out.len() + 1 <= max_bytes {
                out.push('\n');
            }
        } else {
            empty_run = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                if out.len() + 1 > max_bytes {
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
            if out.len() + 1 <= max_bytes {
                out.push('\n');
            }
        }
    }
    out.trim().to_string()
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        if let Some(semi) = rest[1..].find(';').filter(|i| *i <= 12) {
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
    out
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
}
