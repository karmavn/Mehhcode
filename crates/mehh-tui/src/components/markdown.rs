use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::ansi::{visible_width, wrap_text_with_ansi};
use crate::theme::Theme;
use crate::tui::Component;

/// Formatted markdown viewer component using `pulldown_cmark`.
#[derive(Debug, Clone)]
pub struct MarkdownView {
    pub source: String,
    pub theme: Theme,
}

impl MarkdownView {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            theme: Theme::dark(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }
}

fn sanitize_markdown_text(s: &str, allow_newlines: bool) -> String {
    let stripped = crate::ansi::strip_ansi(s);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        match ch {
            '\n' if allow_newlines => out.push(ch),
            '\r' if allow_newlines => {}
            '\t' => out.push_str("   "),
            c if (c as u32) < 0x20 || c == '\x7f' || ('\u{0080}'..='\u{009f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
}

fn sanitize_osc8_url(raw: &str) -> Option<String> {
    let clean = sanitize_markdown_text(raw, false);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:") {
        return None;
    }
    Some(trimmed.to_string())
}

fn format_markdown_table(
    rows: &[Vec<String>],
    has_header: bool,
    max_width: usize,
    border_color: &str,
) -> Vec<String> {
    if rows.is_empty() || max_width == 0 {
        return Vec::new();
    }
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    if max_width < 3 {
        return rows
            .iter()
            .map(|row| {
                crate::ansi::truncate_to_width(
                    &row.join(" "),
                    max_width,
                    "…",
                    false,
                )
            })
            .collect();
    }

    let overhead = col_count * 3 + 1;
    if max_width <= overhead {
        let cols = col_count.min(max_width.saturating_sub(1) / 2).max(1);
        let compact_border = |left: char, mid: char, right: char| {
            let mut s = format!("{border_color}{left}");
            for i in 0..cols {
                s.push('─');
                s.push(if i + 1 == cols { right } else { mid });
            }
            s.push_str("\x1b[0m");
            s
        };
        let mut out = vec![compact_border('┌', '┬', '┐')];
        for (r_idx, row) in rows.iter().enumerate() {
            let mut line = format!("{border_color}│\x1b[0m");
            for c in 0..cols {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                line.push(cell.chars().next().unwrap_or(' '));
                line.push_str(&format!("{border_color}│\x1b[0m"));
            }
            out.push(line);
            if has_header && r_idx == 0 {
                out.push(compact_border('├', '┼', '┤'));
            }
        }
        out.push(compact_border('└', '┴', '┘'));
        return out;
    }

    let mut col_widths = vec![3usize; col_count];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(visible_width(cell));
        }
    }

    if max_width > overhead {
        let avail_cells = max_width - overhead;
        while col_widths.iter().sum::<usize>() > avail_cells {
            if let Some((idx, _)) = col_widths
                .iter()
                .enumerate()
                .filter(|(_, &w)| w > 1)
                .max_by_key(|(_, &w)| w)
            {
                col_widths[idx] -= 1;
            } else {
                break;
            }
        }
    } else {
        col_widths.fill(1);
    }

    let border = |left: char, mid: char, right: char| {
        let mut s = format!("{border_color}{left}");
        for (i, &w) in col_widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if i + 1 == col_widths.len() { right } else { mid });
        }
        s.push_str("\x1b[0m");
        s
    };

    let mut lines = vec![border('┌', '┬', '┐')];
    for (r_idx, row) in rows.iter().enumerate() {
        let mut line = format!("{border_color}│\x1b[0m");
        for (c_idx, &col_w) in col_widths.iter().enumerate() {
            let cell = row.get(c_idx).map(String::as_str).unwrap_or("");
            let formatted = crate::ansi::truncate_to_width(cell, col_w, "…", true);
            line.push(' ');
            if has_header && r_idx == 0 {
                line.push_str("\x1b[1m");
                line.push_str(&formatted);
                line.push_str("\x1b[0m");
            } else {
                line.push_str(&formatted);
            }
            line.push_str(&format!(" {border_color}│\x1b[0m"));
        }
        lines.push(line);

        if has_header && r_idx == 0 {
            lines.push(border('├', '┼', '┤'));
        }
    }
    lines.push(border('└', '┴', '┘'));
    lines
}

fn push_blockquote_lines(
    target: &mut Vec<String>,
    lines: Vec<String>,
    quote_depth: usize,
    quote_color: &str,
) {
    if quote_depth == 0 {
        target.extend(lines);
        return;
    }
    let quote_prefix = format!("{quote_color}│\x1b[0m ").repeat(quote_depth);
    for l in lines {
        target.push(format!("{quote_prefix}{l}"));
    }
}

impl Component for MarkdownView {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);

        let clean_source = crate::ansi::strip_ansi(&self.source);
        let parser = Parser::new_ext(&clean_source, options);

        let mut output_lines: Vec<String> = Vec::new();
        let mut current_line = String::new();

        let mut in_code_block = false;
        let mut code_block_lang = String::new();
        let mut code_block_buf = String::new();

        let mut in_table = false;
        let mut table_has_header = false;
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_current_row: Vec<String> = Vec::new();

        let mut blockquote_depth = 0usize;
        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut link_osc8_stack: Vec<bool> = Vec::new();
        let mut active_heading_style: Option<String> = None;
        let mut active_heading_is_h1 = false;

        let quote_color = if !self.theme.md_quote.is_empty() {
            &self.theme.md_quote
        } else {
            "\x1b[2m"
        };
        let border_color = if !self.theme.border_muted.is_empty() {
            &self.theme.border_muted
        } else {
            "\x1b[2m"
        };
        let code_color = if !self.theme.md_code.is_empty() {
            &self.theme.md_code
        } else {
            &self.theme.text
        };

        let flush_current_line =
            |current: &mut String, lines: &mut Vec<String>, quote_depth: usize| {
                if !current.is_empty() {
                    let line_content = std::mem::take(current);
                    push_blockquote_lines(lines, vec![line_content], quote_depth, quote_color);
                }
            };

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    let prefix = match level {
                        HeadingLevel::H1 => "# ",
                        HeadingLevel::H2 => "## ",
                        HeadingLevel::H3 => "### ",
                        HeadingLevel::H4 => "#### ",
                        HeadingLevel::H5 => "##### ",
                        HeadingLevel::H6 => "###### ",
                    };
                    active_heading_is_h1 = matches!(level, HeadingLevel::H1);
                    let heading_style = match level {
                        HeadingLevel::H1 | HeadingLevel::H2 => {
                            if !self.theme.md_heading.is_empty() {
                                self.theme.md_heading.clone()
                            } else {
                                "\x1b[1;36m".to_string()
                            }
                        }
                        _ => format!("\x1b[1m{}", self.theme.text),
                    };
                    current_line.push_str(&heading_style);
                    current_line.push_str(prefix);
                    active_heading_style = Some(heading_style);
                }
                Event::End(TagEnd::Heading(_)) => {
                    active_heading_style = None;
                    current_line.push_str("\x1b[0m");
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    if active_heading_is_h1 {
                        output_lines.push(format!(
                            "{}{}\x1b[0m",
                            self.theme.border_muted,
                            "─".repeat(width.min(40))
                        ));
                        active_heading_is_h1 = false;
                    }
                    output_lines.push(String::new());
                }
                Event::Start(Tag::Paragraph) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                }
                Event::End(TagEnd::Paragraph) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    output_lines.push(String::new());
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    blockquote_depth += 1;
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                    output_lines.push(String::new());
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    in_code_block = true;
                    code_block_lang = match kind {
                        CodeBlockKind::Fenced(lang) => sanitize_markdown_text(&lang, false),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_block_buf.clear();
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    let clean_code = sanitize_markdown_text(&code_block_buf, true);
                    code_block_buf.clear();

                    let raw_lines: Vec<&str> = if clean_code.is_empty() {
                        vec![""]
                    } else {
                        clean_code.lines().collect()
                    };

                    let effective_w = width.saturating_sub(blockquote_depth * 2).max(1);
                    let mut block_lines = Vec::new();

                    if effective_w < 5 {
                        for line in raw_lines {
                            for wrapped in wrap_text_with_ansi(line, effective_w) {
                                block_lines.push(format!("{code_color}{wrapped}\x1b[0m"));
                            }
                        }
                    } else {
                        let middle_w = effective_w - 2;
                        let inner_w = effective_w - 4;
                        let lang_tag = if code_block_lang.trim().is_empty() {
                            String::new()
                        } else {
                            crate::ansi::truncate_to_width(
                                &format!(" {} ", code_block_lang.trim()),
                                middle_w.saturating_sub(1),
                                "…",
                                false,
                            )
                        };
                        let lang_w = visible_width(&lang_tag);
                        let right_dashes = middle_w.saturating_sub(1 + lang_w);
                        let top = format!(
                            "{border_color}┌─{lang_tag}{}┐\x1b[0m",
                            "─".repeat(right_dashes)
                        );
                        block_lines.push(top);

                        for line in raw_lines {
                            let wrapped = wrap_text_with_ansi(line, inner_w);
                            for wline in wrapped {
                                let fitted =
                                    crate::ansi::truncate_to_width(&wline, inner_w, "", true);
                                block_lines.push(format!(
                                    "{border_color}│\x1b[0m {code_color}{fitted}\x1b[0m {border_color}│\x1b[0m"
                                ));
                            }
                        }

                        let bottom = format!("{border_color}└{}┘\x1b[0m", "─".repeat(middle_w));
                        block_lines.push(bottom);
                    }

                    push_blockquote_lines(
                        &mut output_lines,
                        block_lines,
                        blockquote_depth,
                        quote_color,
                    );
                    output_lines.push(String::new());
                }
                Event::Start(Tag::Table(_)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    in_table = true;
                    table_has_header = false;
                    table_rows.clear();
                    table_current_row.clear();
                }
                Event::End(TagEnd::Table) => {
                    in_table = false;
                    let effective_w = width.saturating_sub(blockquote_depth * 2).max(1);
                    let table_lines = format_markdown_table(
                        &table_rows,
                        table_has_header,
                        effective_w,
                        border_color,
                    );
                    push_blockquote_lines(
                        &mut output_lines,
                        table_lines,
                        blockquote_depth,
                        quote_color,
                    );
                    output_lines.push(String::new());
                    table_rows.clear();
                    table_current_row.clear();
                }
                Event::Start(Tag::TableHead) => {
                    table_current_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    table_has_header = true;
                    table_rows.push(std::mem::take(&mut table_current_row));
                }
                Event::Start(Tag::TableRow) => {
                    table_current_row.clear();
                }
                Event::End(TagEnd::TableRow) => {
                    table_rows.push(std::mem::take(&mut table_current_row));
                }
                Event::Start(Tag::TableCell) => {
                    current_line.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    table_current_row.push(std::mem::take(&mut current_line).trim().to_string());
                }
                Event::Start(Tag::List(first_num)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    list_stack.push(first_num);
                }
                Event::End(TagEnd::List(_)) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    list_stack.pop();
                    if list_stack.is_empty() {
                        output_lines.push(String::new());
                    }
                }
                Event::Start(Tag::Item) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    let depth = list_stack.len().saturating_sub(1);
                    let indent = "  ".repeat(depth);
                    if let Some(Some(ref mut n)) = list_stack.last_mut() {
                        current_line.push_str(&format!("{indent}{n}. "));
                        *n += 1;
                    } else {
                        current_line.push_str(&format!("{indent}- "));
                    }
                }
                Event::End(TagEnd::Item) => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                }
                Event::TaskListMarker(checked) => {
                    let (color, marker) = if checked {
                        (&self.theme.success, "[x]")
                    } else {
                        (&self.theme.dim, "[ ]")
                    };
                    current_line.push_str(&format!("{color}{marker}\x1b[0m "));
                }
                Event::Start(Tag::Emphasis) => {
                    current_line.push_str("\x1b[3m");
                }
                Event::End(TagEnd::Emphasis) => {
                    current_line.push_str("\x1b[23m");
                    if let Some(ref h_style) = active_heading_style {
                        current_line.push_str(h_style);
                    }
                }
                Event::Start(Tag::Strong) => {
                    current_line.push_str("\x1b[1m");
                }
                Event::End(TagEnd::Strong) => {
                    current_line.push_str("\x1b[22m");
                    if let Some(ref h_style) = active_heading_style {
                        current_line.push_str(h_style);
                    }
                }
                Event::Start(Tag::Strikethrough) => {
                    current_line.push_str("\x1b[9m");
                }
                Event::End(TagEnd::Strikethrough) => {
                    current_line.push_str("\x1b[29m");
                    if let Some(ref h_style) = active_heading_style {
                        current_line.push_str(h_style);
                    }
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let link_style = if !self.theme.md_link.is_empty() {
                        &self.theme.md_link
                    } else {
                        "\x1b[4;34m"
                    };
                    if let Some(safe_url) = sanitize_osc8_url(&dest_url) {
                        current_line.push_str(&format!("\x1b]8;;{safe_url}\x07{link_style}"));
                        link_osc8_stack.push(true);
                    } else {
                        current_line.push_str(link_style);
                        link_osc8_stack.push(false);
                    }
                }
                Event::End(TagEnd::Link) => {
                    let opened = link_osc8_stack.pop().unwrap_or(false);
                    let restore = active_heading_style.as_deref().unwrap_or("");
                    if opened {
                        current_line.push_str(&format!("\x1b]8;;\x07\x1b[0m{restore}"));
                    } else {
                        current_line.push_str(&format!("\x1b[0m{restore}"));
                    }
                }
                Event::Code(text) => {
                    if in_code_block {
                        code_block_buf.push_str(&text);
                    } else {
                        let clean = sanitize_markdown_text(&text, false);
                        let inline = format!("{}{}{clean}\x1b[0m", self.theme.surface_elevated, code_color);
                        if let Some(ref h_style) = active_heading_style {
                            current_line.push_str(&format!("{inline}{h_style}"));
                        } else {
                            current_line.push_str(&inline);
                        }
                    }
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_block_buf.push_str(&text);
                    } else {
                        current_line.push_str(&sanitize_markdown_text(&text, false));
                    }
                }
                Event::Rule => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    let rule_w = width.min(40);
                    output_lines.push(format!("{border_color}{}\x1b[0m", "─".repeat(rule_w)));
                    output_lines.push(String::new());
                }
                Event::SoftBreak if !in_code_block => {
                    current_line.push(' ');
                }
                Event::HardBreak if !in_code_block && !in_table => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                }
                _ => {}
            }
        }

        flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);

        // Strip trailing empty lines
        while output_lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            output_lines.pop();
        }

        // Clean wrap to target terminal width
        let mut final_lines = Vec::new();
        for line in output_lines {
            if line.is_empty() {
                final_lines.push(String::new());
            } else {
                for wrapped in wrap_text_with_ansi(&line, width) {
                    final_lines.push(wrapped);
                }
            }
        }

        final_lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    #[test]
    fn narrow_table_stays_within_width() {
        let md = "| Name | Status | Owner |\n|---|---|---|\n| Auth | OK | team |";
        let lines = MarkdownView::new(md)
            .with_theme(Theme::dark())
            .render(10);
        assert!(!lines.is_empty());
        assert!(
            lines.iter().all(|l| crate::ansi::visible_width(l) <= 10),
            "{lines:?}"
        );
    }

    #[test]
    fn table_uses_unicode_box_drawing() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = MarkdownView::new(md)
            .with_theme(Theme::dark())
            .render(40);
        let joined = lines.join("\n");
        assert!(joined.contains('┌') && joined.contains('┬') && joined.contains('┐'));
        assert!(joined.contains('├') && joined.contains('┼') && joined.contains('┤'));
        assert!(joined.contains('└') && joined.contains('┴') && joined.contains('┘'));
        assert!(!joined.contains('|'), "{joined}");
    }

    #[test]
    fn task_markers_come_from_theme() {
        let t = Theme::dark();
        let lines = MarkdownView::new("- [x] done\n- [ ] todo")
            .with_theme(t.clone())
            .render(40);
        let joined = lines.join("\n");
        assert!(joined.contains(&t.success), "{joined}");
        assert!(!joined.contains("\x1b[32m"), "{joined}");
    }

    #[test]
    fn headings_have_distinct_hierarchy() {
        let lines = MarkdownView::new("# One\n\n### Three")
            .with_theme(Theme::dark())
            .render(40);
        let h1 = lines.iter().find(|l| l.contains("One")).unwrap();
        let h3 = lines.iter().find(|l| l.contains("Three")).unwrap();
        assert_ne!(h1, h3);
        assert!(lines.iter().any(|l| l.contains('─')), "{lines:?}");
    }

    #[test]
    fn test_markdown_renders_tables_and_tasklists() {
        let src = "\
| Name | Status |
| --- | --- |
| Auth | OK |

- [x] Done task
- [ ] Pending task";
        let lines = MarkdownView::new(src).render(50);
        let plain = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(plain.contains("Name"));
        assert!(plain.contains("Auth"));
        assert!(plain.contains("──"));
        assert!(plain.contains("[x] Done task"));
        assert!(plain.contains("[ ] Pending task"));
    }

    #[test]
    fn test_markdown_strips_terminal_control_sequences() {
        let src = "Hello \x1b[31mRED\x1b[0m and \x1b]2;pwned\x07world\n\n```rust\nlet s = \"\x1b[2Jclear\";\n```";
        let lines = MarkdownView::new(src).render(60);
        let joined = lines.join("\n");
        assert!(!joined.contains("\x1b[31m"));
        assert!(!joined.contains("\x1b]2;pwned"));
        assert!(!joined.contains("\x1b[2J"));
        let plain: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("Hello RED and world"));
    }

    #[test]
    fn test_markdown_code_block_fits_narrow_width() {
        let src = "```rust\nlet very_long_identifier = \"abcdefghijklmnopqrstuvwxyz\";\n```";
        for w in [10usize, 24, 40] {
            let lines = MarkdownView::new(src).render(w);
            for line in &lines {
                assert!(
                    visible_width(line) <= w,
                    "Line width {} exceeded box width {w}: {line:?}",
                    visible_width(line)
                );
            }
        }
    }

    #[test]
    fn test_markdown_heading_preserves_styling_after_inline_code() {
        let src = "### Why `sourceInfo` should not be optional";
        let theme = Theme::dark();
        let lines = MarkdownView::new(src).render(80);
        let joined = lines.join("\n");
        assert!(
            joined.contains(&theme.md_code),
            "inline code must use theme token"
        );
        let code_marker = "sourceInfo\x1b[0m";
        let code_end = joined.find(code_marker).unwrap() + code_marker.len();
        let after_idx = joined.find("should not be optional").unwrap();
        let between = &joined[code_end..after_idx];
        assert!(
            between.contains(&theme.md_heading)
                || between.contains(&theme.accent)
                || between.contains(&theme.text),
            "Heading style must be re-applied immediately after inline code: {between:?}"
        );
    }

    #[test]
    fn test_markdown_table_narrow_width_does_not_overflow() {
        let src = "| ColumnOne | ColumnTwo |\n| --- | --- |\n| very_long_value_one | very_long_value_two |";
        // Narrow width smaller than overhead
        let lines = MarkdownView::new(src).render(6);
        for line in &lines {
            // Should not produce an unconstrained 100-character line
            assert!(
                crate::ansi::visible_width(line) <= 12,
                "Table line exceeded expected bounds: {:?}",
                line
            );
        }
    }

    #[test]
    fn test_markdown_softbreak_preserves_space() {
        let src = "Hello\nworld";
        let lines = MarkdownView::new(src).render(40);
        let plain = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("Hello world"),
            "Paragraph softbreak must have space between lines: {plain}"
        );
    }
}
