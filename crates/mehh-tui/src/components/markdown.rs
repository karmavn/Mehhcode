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

impl Component for MarkdownView {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);

        let parser = Parser::new_ext(&self.source, options);

        let mut output_lines: Vec<String> = Vec::new();
        let mut current_line = String::new();

        let mut in_code_block = false;
        let mut code_block_lang = String::new();
        let mut code_block_lines: Vec<String> = Vec::new();

        let mut blockquote_depth = 0;
        let mut list_stack: Vec<Option<u64>> = Vec::new();

        let quote_color = if !self.theme.md_quote.is_empty() {
            &self.theme.md_quote
        } else {
            "\x1b[2m"
        };

        let flush_current_line = |current: &mut String, lines: &mut Vec<String>, quote_depth: usize| {
            if !current.is_empty() {
                let line_content = std::mem::take(current);
                if quote_depth > 0 {
                    let quote_prefix = format!("{quote_color}│\x1b[0m ").repeat(quote_depth);
                    lines.push(format!("{quote_prefix}{line_content}"));
                } else {
                    lines.push(line_content);
                }
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
                    let heading_style = if !self.theme.md_heading.is_empty() {
                        &self.theme.md_heading
                    } else {
                        "\x1b[1;36m"
                    };
                    current_line.push_str(heading_style);
                    current_line.push_str(prefix);
                }
                Event::End(TagEnd::Heading(_)) => {
                    current_line.push_str("\x1b[0m");
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
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
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_block_lines.clear();
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    let inner_w = width.saturating_sub(4).max(10);
                    let border_color = if !self.theme.border_muted.is_empty() {
                        &self.theme.border_muted
                    } else {
                        "\x1b[2m"
                    };
                    let code_color = if !self.theme.md_code.is_empty() {
                        &self.theme.md_code
                    } else {
                        "\x1b[33m"
                    };

                    // Top border with optional language tag
                    let lang_str = if code_block_lang.is_empty() {
                        String::new()
                    } else {
                        format!(" {} ", code_block_lang)
                    };
                    let lang_w = visible_width(&lang_str);
                    let bar_w = inner_w.saturating_sub(lang_w);
                    let top = format!("{border_color}┌─{lang_str}{}\x1b[0m", "─".repeat(bar_w));
                    output_lines.push(top);

                    for line in &code_block_lines {
                        output_lines.push(format!("{border_color}│\x1b[0m {code_color}{line}\x1b[0m"));
                    }

                    let bottom = format!("{border_color}└{}\x1b[0m", "─".repeat(inner_w + 2));
                    output_lines.push(bottom);
                    output_lines.push(String::new());
                    code_block_lines.clear();
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
                Event::Start(Tag::Emphasis) => {
                    current_line.push_str("\x1b[3m");
                }
                Event::End(TagEnd::Emphasis) => {
                    current_line.push_str("\x1b[23m");
                }
                Event::Start(Tag::Strong) => {
                    current_line.push_str("\x1b[1m");
                }
                Event::End(TagEnd::Strong) => {
                    current_line.push_str("\x1b[22m");
                }
                Event::Start(Tag::Strikethrough) => {
                    current_line.push_str("\x1b[9m");
                }
                Event::End(TagEnd::Strikethrough) => {
                    current_line.push_str("\x1b[29m");
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let url = dest_url.to_string();
                    let link_style = if !self.theme.md_link.is_empty() {
                        &self.theme.md_link
                    } else {
                        "\x1b[4;34m"
                    };
                    current_line.push_str(&format!("\x1b]8;;{url}\x07{link_style}"));
                }
                Event::End(TagEnd::Link) => {
                    current_line.push_str("\x1b]8;;\x07\x1b[0m");
                }
                Event::Code(text) => {
                    if in_code_block {
                        for l in text.split('\n') {
                            code_block_lines.push(l.trim_end_matches('\r').to_string());
                        }
                    } else {
                        let code_color = if !self.theme.md_code.is_empty() {
                            &self.theme.md_code
                        } else {
                            "\x1b[33m"
                        };
                        current_line.push_str(&format!("{code_color}{text}\x1b[0m"));
                    }
                }
                Event::Text(text) => {
                    if in_code_block {
                        for l in text.split('\n') {
                            code_block_lines.push(l.trim_end_matches('\r').to_string());
                        }
                    } else {
                        current_line.push_str(&text);
                    }
                }
                Event::Rule => {
                    flush_current_line(&mut current_line, &mut output_lines, blockquote_depth);
                    let rule_w = width.min(40);
                    let border_color = if !self.theme.border_muted.is_empty() {
                        &self.theme.border_muted
                    } else {
                        "\x1b[2m"
                    };
                    output_lines.push(format!("{border_color}{}\x1b[0m", "─".repeat(rule_w)));
                    output_lines.push(String::new());
                }
                Event::SoftBreak if !in_code_block => {
                    current_line.push(' ');
                }
                Event::HardBreak if !in_code_block => {
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
