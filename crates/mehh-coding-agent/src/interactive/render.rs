use super::*;

pub(super) fn scrollbar_char(row: usize, visible: usize, total: usize, offset: usize) -> char {
    if visible == 0 || total <= visible {
        return '│';
    }
    let max_scroll = total - visible;
    let from_top = max_scroll.saturating_sub(offset.min(max_scroll));
    let thumb_row = from_top * visible.saturating_sub(1) / max_scroll.max(1);
    if row == thumb_row {
        '┃'
    } else {
        '│'
    }
}

pub(super) fn cap_preview_lines(preview: &str, max_lines: usize) -> String {
    let mut iter = preview.lines();
    let head: Vec<&str> = iter.by_ref().take(max_lines).collect();
    let hidden = iter.count();
    if hidden == 0 {
        return preview.to_string();
    }
    let mut out = head.join("\n");
    out.push_str(&format!("\n… (+{hidden} more lines)"));
    out
}

pub(super) fn tool_duration_label(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) => format!(" {ms}ms"),
        None => String::new(),
    }
}

pub(super) fn diff_visible_lines(diff: &str) -> (Vec<&str>, usize) {
    let mut iter = diff.lines();
    let visible: Vec<&str> = iter.by_ref().take(24).collect();
    let hidden = iter.count();
    (visible, hidden)
}

pub(super) fn thinking_preview(
    thinking: &str,
    expanded: bool,
    busy: bool,
    elapsed_ms: Option<u64>,
) -> String {
    if expanded {
        return thinking.to_string();
    }
    if busy {
        let mut count = 0usize;
        let mut ring = [""; 3];
        for line in thinking.lines() {
            ring[count % 3] = line;
            count += 1;
        }
        let tail_len = count.min(3);
        let mut out = format!("⠋ Thinking ({count} lines)...\n");
        for idx in 0..tail_len {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(ring[(count - tail_len + idx) % 3]);
        }
        out
    } else {
        let count = thinking.lines().count();
        let secs = elapsed_ms.unwrap_or(0) as f64 / 1000.0;
        format!("▸ Thought for {secs:.1}s ({count} lines · Ctrl+T to expand)")
    }
}

pub(super) fn compact_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    }
}

pub(super) fn compact_path(path: &Path, max_len: usize) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.chars().count() <= max_len {
        return raw;
    }
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    let tail = parts
        .iter()
        .rev()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    let compact = format!("~/.../{tail}");
    if compact.chars().count() <= max_len {
        compact
    } else {
        let keep = max_len.saturating_sub(3);
        let tail: String = raw.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
        format!("...{tail}")
    }
}

pub(super) struct RenderedLine {
    pub(super) text: String,
    pub(super) regions: Vec<(usize, usize, ClickAction)>,
}

impl RenderedLine {
    fn plain(text: String) -> Self {
        Self {
            text,
            regions: Vec::new(),
        }
    }

    fn with_action(mut self, col_start: usize, col_end: usize, action: ClickAction) -> Self {
        if col_end > col_start {
            self.regions.push((col_start, col_end, action));
        }
        self
    }
}

struct SpanLineBuilder {
    raw: String,
    col: usize,
    regions: Vec<(usize, usize, ClickAction)>,
}

impl SpanLineBuilder {
    fn new() -> Self {
        Self {
            raw: String::new(),
            col: 0,
            regions: Vec::new(),
        }
    }

    fn push_plain(&mut self, text: &str) {
        self.raw.push_str(text);
        self.col += mehh_tui::visible_width(text);
    }

    fn push_styled(&mut self, color: &str, text: &str, _theme: &Theme) {
        if !color.is_empty() {
            self.raw.push_str(color);
            self.raw.push_str(text);
            self.raw.push_str("\x1b[0m");
        } else {
            self.raw.push_str(text);
        }
        self.col += mehh_tui::visible_width(text);
    }

    fn push_prestyled(&mut self, styled: &str) {
        self.raw.push_str(styled);
        self.col += mehh_tui::visible_width(styled);
    }

    fn push_clickable(&mut self, color: &str, text: &str, _theme: &Theme, action: ClickAction) {
        let start = self.col;
        let w = mehh_tui::visible_width(text);
        if !color.is_empty() {
            self.raw.push_str(color);
            self.raw.push_str(text);
            self.raw.push_str("\x1b[0m");
        } else {
            self.raw.push_str(text);
        }
        self.col += w;
        if w > 0 {
            self.regions.push((start, start + w, action));
        }
    }

    fn push_clickable_prestyled(&mut self, styled: &str, action: ClickAction) {
        let start = self.col;
        let w = mehh_tui::visible_width(styled);
        self.raw.push_str(styled);
        self.col += w;
        if w > 0 {
            self.regions.push((start, start + w, action));
        }
    }

    fn finish(mut self, width: usize) -> RenderedLine {
        let raw_w = self.col;
        if raw_w <= width {
            return RenderedLine {
                text: self.raw,
                regions: self.regions,
            };
        }
        let text = truncate_to_width(&self.raw, width, "...", false);
        let text_w = mehh_tui::visible_width(&text).min(width);
        let max_w = text_w.saturating_sub(3);
        self.regions.retain_mut(|(s, e, _)| {
            if *s >= max_w {
                return false;
            }
            *e = (*e).min(max_w);
            *e > *s
        });
        RenderedLine {
            text,
            regions: self.regions,
        }
    }
}

fn left_right_rendered(
    left: SpanLineBuilder,
    right: SpanLineBuilder,
    width: usize,
) -> RenderedLine {
    let right_raw_w = right.col;
    let (right_text, right_w) = if right_raw_w <= width {
        (right.raw, right_raw_w)
    } else {
        let t = truncate_to_width(&right.raw, width, "...", false);
        let w = mehh_tui::visible_width(&t);
        (t, w)
    };
    let left_budget = width.saturating_sub(right_w);
    let left_raw_w = left.col;
    let (left_text, left_w) = if left_raw_w <= left_budget {
        (left.raw, left_raw_w)
    } else {
        let t = truncate_to_width(&left.raw, left_budget, "...", false);
        let w = mehh_tui::visible_width(&t);
        (t, w)
    };
    let pad = width.saturating_sub(left_w + right_w);

    let left_clickable_w = if left_raw_w > left_budget {
        left_w.saturating_sub(3)
    } else {
        left_w
    };
    let right_clickable_w = if right_raw_w > width {
        right_w.saturating_sub(3)
    } else {
        right_w
    };

    let mut regions = Vec::with_capacity(left.regions.len() + right.regions.len());
    for (s, e, action) in left.regions {
        if s < left_clickable_w {
            let clamped_e = e.min(left_clickable_w);
            if clamped_e > s {
                regions.push((s, clamped_e, action));
            }
        }
    }
    let shift = left_w + pad;
    for (s, e, action) in right.regions {
        if s < right_clickable_w {
            let clamped_e = e.min(right_clickable_w);
            let ns = s.saturating_add(shift);
            let ne = clamped_e.saturating_add(shift).min(width);
            if ns < width && ne > ns {
                regions.push((ns, ne, action));
            }
        }
    }

    let mut combined = String::with_capacity(left_text.len() + pad + right_text.len());
    combined.push_str(&left_text);
    combined.extend(std::iter::repeat_n(' ', pad));
    combined.push_str(&right_text);
    let text = if left_w + pad + right_w <= width {
        combined
    } else {
        truncate_to_width(&combined, width, "...", false)
    };

    RenderedLine { text, regions }
}

pub(super) fn left_right(left: String, right: String, width: usize) -> String {
    let right_w = mehh_tui::visible_width(&right);
    let left_w = width.saturating_sub(right_w);
    let left = truncate_to_width(&left, left_w, "...", false);
    let pad = width.saturating_sub(mehh_tui::visible_width(&left) + right_w);
    let mut out = String::with_capacity(left.len() + pad + right.len());
    out.push_str(&left);
    out.extend(std::iter::repeat_n(' ', pad));
    out.push_str(&right);
    out
}

pub(super) fn ctx_bar(pct: u64, theme: &Theme) -> String {
    const FULL_BLOCKS: &str = "████████";
    const EMPTY_BLOCKS: &str = "░░░░░░░░";
    // Round up so any non-zero usage fills at least one block; otherwise a
    // large context window makes the meter look frozen at zero.
    let filled = pct.div_ceil(13).min(8) as usize;
    let color = if pct >= 85 {
        &theme.error
    } else if pct >= 70 {
        &theme.warning
    } else {
        &theme.accent
    };
    let mut out = theme.fg(color, &FULL_BLOCKS[..filled * 3]);
    out.push_str(&theme.fg(&theme.border_muted, &EMPTY_BLOCKS[..(8 - filled) * 3]));
    out
}

fn permission_badge(mode: crate::tool_inspection::PermissionMode, theme: &Theme) -> String {
    use crate::tool_inspection::PermissionMode;
    let (text, color) = match mode {
        PermissionMode::Auto => ("[auto]", &theme.warning),
        PermissionMode::SmartApprove => ("[smart]", &theme.accent),
        PermissionMode::Approve => ("[approve]", &theme.accent),
        PermissionMode::Chat => ("[chat]", &theme.dim),
    };
    theme.fg(color, text)
}

pub(super) fn elapsed_label(started: Option<std::time::Instant>, now: std::time::Instant) -> String {
    match started {
        Some(s) => format!("{:.1}s", now.saturating_duration_since(s).as_secs_f64()),
        None => String::new(),
    }
}

pub(super) fn render_user_turn(text: &str, width: usize, theme: &Theme) -> Vec<String> {
    let clean = mehh_tui::sanitize_terminal_text(text, true);
    let inner = width.saturating_sub(4);
    wrap_text_with_ansi(&clean, inner)
        .into_iter()
        .map(|line| {
            let body_w = mehh_tui::visible_width(&line);
            let pad = width.saturating_sub(1 + body_w);
            let mut row = String::with_capacity(
                theme.user_prefix.len()
                    + theme.user_bg.len()
                    + theme.text.len()
                    + line.len()
                    + pad
                    + 16,
            );
            row.push_str(&theme.user_prefix);
            row.push_str("▌\x1b[0m");
            row.push_str(&theme.user_bg);
            row.push_str(&theme.text);
            row.push_str(&line);
            row.extend(std::iter::repeat_n(' ', pad));
            row.push_str("\x1b[0m");
            row
        })
        .collect()
}

pub(super) fn status_icon(status: &str, is_error: bool, frame: usize, theme: &Theme) -> String {
    if is_error || status == "failed" {
        theme.fg(&theme.tool_error, "✗")
    } else if status == "completed" {
        theme.fg(&theme.tool_success, "✓")
    } else if status == "blocked" {
        theme.fg(&theme.tool_pending, "◌")
    } else {
        let frames = mehh_tui::BRAILLE_FRAMES;
        theme.fg(&theme.tool_pending, frames[frame % frames.len()])
    }
}

/// Modal body wrapper that renders an optional search header and footer hint inside `BorderedBox`.
struct ModalPanel<'a> {
    search_bar: Option<(&'a str, usize, usize)>,
    body: Box<dyn Component>,
    footer_hint: Option<&'static str>,
    max_height: Option<usize>,
    theme: &'a Theme,
}

impl ModalPanel<'_> {
    fn render_inner(&self, width: usize) -> Vec<String> {
        let mut search_lines = Vec::new();
        if let Some((filter, shown, total)) = self.search_bar {
            let clean_filter = mehh_tui::sanitize_terminal_text(filter, false);
            let left = if clean_filter.is_empty() {
                self.theme.fg(&self.theme.dim, "  / Type to filter...")
            } else {
                self.theme.fg(&self.theme.accent, &format!("  / {clean_filter}▌"))
            };
            let right = self.theme.fg(
                if clean_filter.is_empty() {
                    &self.theme.dim
                } else {
                    &self.theme.accent
                },
                &format!("{shown}/{total}  "),
            );
            search_lines.push(left_right(left, right, width));
            search_lines.push(self.theme.fg(&self.theme.border_muted, &"─".repeat(width)));
        }

        let mut hint_lines = Vec::new();
        if let Some(hint) = self.footer_hint {
            hint_lines.push(String::new());
            hint_lines.push(truncate_to_width(
                &format!("  {}", self.theme.fg(&self.theme.dim, hint)),
                width,
                "...",
                false,
            ));
        }

        let mut body_lines = self.body.render(width);
        if let Some(limit) = self.max_height {
            let chrome = search_lines.len() + hint_lines.len();
            if body_lines.len() + chrome > limit {
                if chrome >= limit {
                    hint_lines.clear();
                }
                let remaining = limit.saturating_sub(search_lines.len() + hint_lines.len());
                body_lines.truncate(remaining);
            }
        }

        let mut lines = Vec::with_capacity(search_lines.len() + body_lines.len() + hint_lines.len());
        lines.extend(search_lines);
        lines.extend(body_lines);
        lines.extend(hint_lines);
        lines
    }
}

pub(super) fn scroll_pill_text(offset: usize, max_scroll: usize, new_lines: usize) -> String {
    let pct = offset
        .min(max_scroll)
        .saturating_mul(100)
        .checked_div(max_scroll)
        .unwrap_or(100);
    format!("↑ Paused at {pct}% · +{new_lines} new lines · Ctrl+End to jump")
}

pub(super) fn hash_with_sha256(f: impl FnOnce(&mut sha2::Sha256)) -> [u8; 16] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    f(&mut h);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

pub(super) fn hash_str_128(s: &str) -> [u8; 16] {
    hash_with_sha256(|h| {
        use sha2::Digest;
        h.update((s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    })
}

pub(super) fn hash_transcript_item(item: &TranscriptItem) -> [u8; 16] {
    use sha2::Digest;
    let feed_str = |h: &mut sha2::Sha256, s: &str| {
        h.update((s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    };
    hash_with_sha256(|h| match item {
        TranscriptItem::User(text) => {
            h.update([0]);
            feed_str(h, text);
        }
        TranscriptItem::Assistant { thinking, text } => {
            h.update([1]);
            feed_str(h, thinking);
            feed_str(h, text);
        }
        TranscriptItem::ToolCall {
            id,
            name,
            status,
            is_error,
            args_summary,
            output_preview,
            diff,
            duration_ms,
        } => {
            h.update([2]);
            feed_str(h, id);
            feed_str(h, name);
            feed_str(h, status);
            h.update([u8::from(*is_error)]);
            feed_str(h, args_summary);
            h.update((output_preview.len() as u64).to_le_bytes());
            for line in output_preview {
                feed_str(h, line);
            }
            match diff {
                Some(d) => {
                    h.update([1]);
                    feed_str(h, d);
                }
                None => h.update([0]),
            }
            match duration_ms {
                Some(ms) => {
                    h.update([1]);
                    h.update(ms.to_le_bytes());
                }
                None => h.update([0]),
            }
        }
        TranscriptItem::Subagent {
            id,
            mode,
            status,
            is_error,
            children,
        } => {
            h.update([3]);
            feed_str(h, id);
            feed_str(h, mode);
            feed_str(h, status);
            h.update([u8::from(*is_error)]);
            h.update((children.len() as u64).to_le_bytes());
            for child in children {
                feed_str(h, &child.child_id);
                feed_str(h, &child.label);
                feed_str(h, &child.status);
                feed_str(h, &child.detail);
            }
        }
        TranscriptItem::System(text) => {
            h.update([4]);
            feed_str(h, text);
        }
    })
}

fn is_unsafe_markdown_boundary_line(trimmed: &str, leading_spaces: usize) -> bool {
    leading_spaces >= 2
        || trimmed.starts_with(['-', '*', '+', '>', '|', '=', '<', '['])
        || trimmed.starts_with(|c: char| c.is_ascii_digit())
}

fn update_fence_state(raw_line: &str, fence: &mut Option<(char, usize)>) -> bool {
    let trimmed = raw_line.trim_start_matches(' ');
    let leading = raw_line.len() - trimmed.len();
    if leading >= 4 {
        return false;
    }
    let first = match trimmed.bytes().next() {
        Some(b'`') => '`',
        Some(b'~') => '~',
        _ => return false,
    };
    let count = trimmed.bytes().take_while(|&b| b == first as u8).count();
    if count < 3 {
        return false;
    }
    match *fence {
        None => {
            if first == '`' && trimmed[count..].contains('`') {
                return false;
            }
            *fence = Some((first, count));
            true
        }
        Some((fc, flen)) => {
            if fc == first && count >= flen && trimmed[count..].trim().is_empty() {
                *fence = None;
                return true;
            }
            false
        }
    }
}

/// Finds the byte index (at the start of a trailing blank-line separator) of the last
/// top-level block boundary safe to cache independently of subsequent streamed text.
pub(super) fn find_safe_markdown_split(src: &str) -> Option<usize> {
    if src.len() < 256 {
        return None;
    }

    let mut lines: Vec<(usize, usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for chunk in src.split('\n') {
        let len = chunk.len();
        let line = chunk.strip_suffix('\r').unwrap_or(chunk);
        let t_start = line.trim_start();
        if t_start.starts_with('[') && line.contains("]:") {
            return None;
        }
        lines.push((offset, offset + len, line));
        offset += len + 1;
    }

    let mut fence: Option<(char, usize)> = None;
    let mut prev_non_blank_safe = false;
    let mut last_safe: Option<usize> = None;

    for idx in 0..lines.len() {
        let (line_start, _line_end, line) = lines[idx];
        let trimmed = line.trim();
        let leading = line.len() - line.trim_start_matches(' ').len();

        let prev_fence = fence;
        let toggled = update_fence_state(line, &mut fence);
        let closing_fence = prev_fence.is_some() && fence.is_none() && toggled;

        if trimmed.is_empty() && fence.is_none() && prev_non_blank_safe {
            let mut next_idx = idx + 1;
            while next_idx < lines.len() && lines[next_idx].2.trim().is_empty() {
                next_idx += 1;
            }
            if next_idx < lines.len() {
                let next_line = lines[next_idx].2;
                let next_trimmed = next_line.trim();
                let next_leading = next_line.len() - next_line.trim_start_matches(' ').len();
                let mut tmp_fence = None;
                let starts_fence = update_fence_state(next_line, &mut tmp_fence);
                let next_ok = !next_line.starts_with('\t')
                    && (starts_fence
                        || next_trimmed.starts_with('#')
                        || !is_unsafe_markdown_boundary_line(next_trimmed, next_leading));
                if next_ok && !src[line_start..].trim().is_empty() {
                    last_safe = Some(line_start);
                }
            }
            prev_non_blank_safe = false;
            continue;
        }

        if fence.is_some() || trimmed.is_empty() {
            prev_non_blank_safe = false;
        } else if closing_fence {
            prev_non_blank_safe = true;
        } else {
            let safe_line = !line.starts_with('\t')
                && !trimmed.ends_with(':')
                && !is_unsafe_markdown_boundary_line(trimmed, leading);
            prev_non_blank_safe = safe_line;
        }
    }

    last_safe
}

pub(super) fn render_markdown_incremental(
    clean_text: &str,
    theme: &Theme,
    width: usize,
    prev: Option<&super::state::StreamingMarkdownPrefix>,
    theme_generation: u64,
) -> (Vec<String>, Option<super::state::StreamingMarkdownPrefix>) {
    let Some(split_byte) = find_safe_markdown_split(clean_text) else {
        return (mehh_tui::render_markdown(clean_text, theme, width), None);
    };
    let tail = clean_text[split_byte..].trim_start_matches(['\r', '\n']);
    if tail.trim().is_empty() {
        return (mehh_tui::render_markdown(clean_text, theme, width), None);
    }

    let prefix_src = &clean_text[..split_byte];
    let prefix_hash = hash_str_128(prefix_src);

    let prefix_lines = if let Some(cached) = prev.filter(|p| {
        p.width == width && p.theme_name == theme.name && p.theme_generation == theme_generation
    }) {
        if cached.prefix_byte_len == split_byte && cached.prefix_hash == prefix_hash {
            cached.prefix_lines.clone()
        } else if cached.prefix_byte_len < split_byte
            && hash_str_128(&clean_text[..cached.prefix_byte_len]) == cached.prefix_hash
        {
            let mid = clean_text[cached.prefix_byte_len..split_byte].trim_start_matches(['\r', '\n']);
            let mut combined = cached.prefix_lines.clone();
            if !mid.trim().is_empty() {
                let mid_lines = mehh_tui::render_markdown(mid, theme, width);
                if !combined.is_empty() && !mid_lines.is_empty() {
                    combined.push(String::new());
                }
                combined.extend(mid_lines);
            }
            combined
        } else {
            mehh_tui::render_markdown(prefix_src, theme, width)
        }
    } else {
        mehh_tui::render_markdown(prefix_src, theme, width)
    };

    let tail_lines = mehh_tui::render_markdown(tail, theme, width);
    let mut full = Vec::with_capacity(prefix_lines.len() + tail_lines.len() + 1);
    full.extend(prefix_lines.iter().cloned());
    if !prefix_lines.is_empty() && !tail_lines.is_empty() {
        full.push(String::new());
    }
    full.extend(tail_lines);

    let next_cache = super::state::StreamingMarkdownPrefix {
        prefix_byte_len: split_byte,
        prefix_hash,
        prefix_lines,
        width,
        theme_name: theme.name.clone(),
        theme_generation,
    };
    (full, Some(next_cache))
}


/// Renders a full-screen frame with header, scrollable transcript tail, bottom controls/editor, and footer.
pub fn render_interactive_frame(
    state: &InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    width: usize,
    height: usize,
) -> Vec<String> {
    let width = width.max(20);
    let height = height.max(5);
    let theme = &state.theme;
    let is_faux = session.model.provider.eq_ignore_ascii_case("faux");
    let no_provider = state.connected_provider_count == 0 && !is_faux;

    // 1. Header — a welcome card replaces the status strip on an empty session.
    let model_name = mehh_tui::sanitize_terminal_text(&session.model.name, false);
    let model_provider = mehh_tui::sanitize_terminal_text(&session.model.provider, false);
    let header_lines: Vec<RenderedLine> =
        if state.items.is_empty() && editor.autocomplete.is_none() && state.selector.is_none() {
            let cwd_str = mehh_tui::sanitize_terminal_text(&session.cwd.to_string_lossy(), false);
            let rule = "─".repeat(width.saturating_sub(4).max(1));

            let mut row1 = SpanLineBuilder::new();
            row1.push_plain("  ");
            row1.push_styled(&theme.accent, "mehh", theme);
            row1.push_plain(&format!(" v{}", env!("CARGO_PKG_VERSION")));

            let mut row2 = SpanLineBuilder::new();
            row2.push_plain("  ");
            if no_provider {
                row2.push_clickable(
                    &theme.warning,
                    "No provider connected",
                    theme,
                    ClickAction::OpenConnectProvider,
                );
            } else {
                row2.push_clickable(
                    &theme.text,
                    &model_name,
                    theme,
                    ClickAction::OpenModelPicker,
                );
            }
            row2.push_plain(" ");
            row2.push_clickable_prestyled(
                &permission_badge(effective_permission_mode(session), theme),
                ClickAction::OpenModePicker,
            );
            row2.push_styled(&theme.dim, &format!(" · {cwd_str}"), theme);

            let mut rows = vec![
                RenderedLine::plain(String::new()),
                row1.finish(width),
                row2.finish(width),
            ];

            if no_provider {
                let mut cta = SpanLineBuilder::new();
                cta.push_plain("  ");
                cta.push_styled(&theme.warning, "No provider connected · click ", theme);
                cta.push_clickable(
                    &theme.accent,
                    "[Connect Provider]",
                    theme,
                    ClickAction::OpenConnectProvider,
                );
                cta.push_styled(&theme.dim, " or run /connect", theme);
                rows.push(cta.finish(width));
            } else {
                rows.push(RenderedLine::plain(String::new()));
            }

            let mut quick = SpanLineBuilder::new();
            quick.push_plain("  ");
            quick.push_clickable(
                &theme.accent,
                "[Connect]",
                theme,
                ClickAction::OpenConnectProvider,
            );
            quick.push_styled(&theme.dim, " · ", theme);
            quick.push_clickable(&theme.accent, "[Models]", theme, ClickAction::OpenModelPicker);
            quick.push_styled(&theme.dim, " · ", theme);
            quick.push_clickable(
                &theme.accent,
                "[Sessions]",
                theme,
                ClickAction::OpenSessionSwitcher,
            );
            quick.push_styled(&theme.dim, " · ", theme);
            quick.push_clickable(
                &theme.accent,
                "[Settings]",
                theme,
                ClickAction::OpenSettings,
            );
            quick.push_styled(&theme.dim, " · ", theme);
            quick.push_clickable(&theme.dim, "/help", theme, ClickAction::OpenHelp);
            quick.push_styled(&theme.dim, "   @file attach   Shift+Enter newline", theme);
            rows.push(quick.finish(width));

            rows.push(RenderedLine::plain(truncate_to_width(
                &theme.fg(&theme.border_muted, &format!("  {rule}")),
                width,
                "...",
                false,
            )));
            rows
        } else {
            let clean_session_id =
                mehh_tui::sanitize_terminal_text(session.session().session_id(), false);
            let short_id: String = clean_session_id.chars().take(8).collect();
            let sess_label = session
                .session()
                .session_name()
                .map(|n| {
                    let clean_name = mehh_tui::sanitize_terminal_text(&n, false);
                    format!("session: {} ▾", clean_name.chars().take(16).collect::<String>())
                })
                .unwrap_or_else(|| format!("session: {short_id} ▾"));

            let mut left = SpanLineBuilder::new();
            left.push_styled(&theme.accent, "mehh", theme);
            left.push_plain(" ");
            if no_provider {
                left.push_clickable(
                    &theme.warning,
                    "No provider connected",
                    theme,
                    ClickAction::OpenConnectProvider,
                );
            } else {
                left.push_clickable(
                    &theme.text,
                    &model_name,
                    theme,
                    ClickAction::OpenModelPicker,
                );
                left.push_styled(
                    &theme.dim,
                    &format!(" ({model_provider})"),
                    theme,
                );
            }
            left.push_plain(" ");
            left.push_clickable_prestyled(
                &permission_badge(effective_permission_mode(session), theme),
                ClickAction::OpenModePicker,
            );
            left.push_plain(" ");
            left.push_clickable(
                &theme.accent,
                "[+ connect]",
                theme,
                ClickAction::OpenConnectProvider,
            );

            let mut right = SpanLineBuilder::new();
            right.push_clickable(
                &theme.warning,
                &sess_label,
                theme,
                ClickAction::OpenSessionSwitcher,
            );
            right.push_plain(" ");
            right.push_clickable(&theme.accent, "[+ new]", theme, ClickAction::NewSession);
            let cwd_compact =
                mehh_tui::sanitize_terminal_text(&compact_path(&session.cwd, 14), false);
            right.push_styled(
                &theme.dim,
                &format!(" · {cwd_compact}"),
                theme,
            );

            let sep_line = theme.fg(&theme.border_muted, &"─".repeat(width));
            vec![
                left_right_rendered(left, right, width),
                RenderedLine::plain(truncate_to_width(&sep_line, width, "", false)),
            ]
        };

    // 2. Footer with a live context-window meter and clickable controls.
    let thinking_profile = mehh_ai::model_thinking_profile(&session.model);
    let effective_thinking = session.effective_thinking_level();
    let thinking_str = if !thinking_profile.supported {
        "n/a"
    } else {
        effective_thinking
            .map(thinking_level_label)
            .unwrap_or("off")
    };
    let thinking_active = thinking_profile.supported
        && effective_thinking.is_some_and(|l| l != ThinkingLevel::Off);
    let ctx_tokens = {
        let mut guard = state
            .cached_ctx_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = *guard {
            cached
        } else {
            let fresh = crate::compaction::estimate_context_tokens(
                &session.session().build_context_messages(),
            ) as u64;
            *guard = Some(fresh);
            fresh
        }
    };
    let ctx_pct = ctx_tokens
        .saturating_mul(100)
        .div_ceil(session.model.context_window.max(1))
        .min(999);
    // The header already carries provider/model; the footer keeps live metrics only.
    let ctx_display_color = if ctx_pct >= 85 {
        &theme.error
    } else if ctx_pct >= 70 {
        &theme.warning
    } else {
        &theme.dim
    };

    let mut f_left = SpanLineBuilder::new();
    f_left.push_clickable(
        if thinking_active {
            &theme.accent
        } else {
            &theme.dim
        },
        &format!("thinking: {thinking_str}"),
        theme,
        ClickAction::CycleThinking,
    );
    f_left.push_styled(&theme.dim, " · ctx ", theme);
    f_left.push_styled(ctx_display_color, &format!("{ctx_pct}%"), theme);
    f_left.push_plain(" ");
    f_left.push_prestyled(&ctx_bar(ctx_pct, theme));

    let mut f_right = SpanLineBuilder::new();
    f_right.push_styled(
        &theme.dim,
        &format!(
            "{} in / {} out · ${:.4} · ",
            compact_count(state.total_input_tokens),
            compact_count(state.total_output_tokens),
            state.total_cost
        ),
        theme,
    );
    f_right.push_clickable(&theme.accent, "/help", theme, ClickAction::OpenHelp);

    let footer_lines = vec![left_right_rendered(f_left, f_right, width)];

    // 3. Bottom controls
    let mut busy_lines = Vec::new();
    if state.is_busy {
        let spinner = mehh_tui::BRAILLE_FRAMES[state.loader.frame % mehh_tui::BRAILLE_FRAMES.len()];
        let now = std::time::Instant::now();
        let started = *state.turn_started_at.lock().unwrap_or_else(|e| e.into_inner());
        let completed = *state.turn_completed_at.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = elapsed_label(started.or(completed), completed.unwrap_or(now));
        let clean_label = mehh_tui::sanitize_terminal_text(&state.loader.label, false);
        let loader_line = format!(
            "{} {} {} · Esc to cancel",
            theme.fg(&theme.accent, spinner),
            theme.fg(&theme.text, &clean_label),
            theme.fg(&theme.dim, &elapsed)
        );
        busy_lines = wrap_text_with_ansi(&loader_line, width)
            .into_iter()
            .map(RenderedLine::plain)
            .collect();
        if let Some(ref st) = state.status {
            let clean_st = mehh_tui::sanitize_terminal_text(st, false);
            busy_lines.push(RenderedLine::plain(truncate_to_width(
                &format!("  {}", theme.fg(&theme.warning, &clean_st)),
                width,
                "...",
                false,
            )));
        }
    }
    if let Some(ref disc) = state.discovery_status {
        let clean_disc = mehh_tui::sanitize_terminal_text(disc, false);
        busy_lines.push(RenderedLine::plain(truncate_to_width(
            &format!("  {}", theme.fg(&theme.accent, &clean_disc)),
            width,
            "...",
            false,
        )));
    }

    // Queue counters are pills: print only the queues that actually hold messages.
    let steering = session.pending_steering_count();
    let follow_up = session.pending_follow_up_count();
    let mut queued = Vec::new();
    if steering > 0 {
        queued.push(format!("steering: {steering}"));
    }
    if follow_up > 0 {
        queued.push(format!("follow-up: {follow_up}"));
    }
    let queue_line = (!queued.is_empty()).then(|| {
        RenderedLine::plain(truncate_to_width(
            &format!("  {}", theme.fg(&theme.dim, &queued.join(" • "))),
            width,
            "...",
            false,
        ))
    });

    let has_tool_approval_early = state.has_tool_approval();
    let post_modal_rows = if has_tool_approval_early {
        usize::from(state.scroll_offset > 0)
    } else {
        busy_lines.len() + usize::from(queue_line.is_some()) + usize::from(state.scroll_offset > 0)
    };
    let bottom_budget = height
        .saturating_sub(header_lines.len() + footer_lines.len() + 1)
        .max(1);
    let modal_budget = bottom_budget.saturating_sub(post_modal_rows).max(1);
    let mut bottom_lines: Vec<RenderedLine> = Vec::new();
    if let Some(ref selector) = state.selector {
        let mut preview_lines = Vec::new();
        if selector.kind == SelectorKind::ToolApproval {
            if let Some(ref pending) = state.pending_approval {
                let clean_tool = mehh_tui::sanitize_terminal_text(&pending.tool_name, false);
                let clean_reason = mehh_tui::sanitize_terminal_text(&pending.reason, true);
                let mut detail = format!(
                    "{}  {}\n{}\n",
                    theme.fg(&theme.accent, "tool"),
                    theme.fg(&theme.text, &clean_tool),
                    theme.fg(&theme.muted, &clean_reason)
                );
                if let Some(ref preview) = pending.preview {
                    let clean_preview = mehh_tui::sanitize_terminal_text_cow(preview, true);
                    let preview_budget = height.saturating_sub(18).clamp(1, 6);
                    let wrap_w = width.saturating_sub(4);
                    let mut wrapped: Vec<String> = Vec::with_capacity(preview_budget + 1);
                    for line in clean_preview.lines() {
                        wrapped.extend(wrap_text_with_ansi(line, wrap_w));
                        if wrapped.len() > preview_budget {
                            break;
                        }
                    }
                    detail.push_str(&cap_preview_lines(&wrapped.join("\n"), preview_budget));
                }
                let panel = BorderedBox::new(Box::new(Text::new(detail)))
                    .with_title("Command preview · verify before approving")
                    .with_style(BorderStyle::Rounded)
                    .with_border_color(&theme.warning)
                    .with_title_color(&theme.warning);
                preview_lines = panel.render(width);
            }
        }
        let inner_modal_budget = modal_budget
            .saturating_sub(preview_lines.len())
            .saturating_sub(2)
            .max(1);
        bottom_lines.extend(preview_lines.into_iter().map(RenderedLine::plain));

        if selector.kind == SelectorKind::ConnectInput {
            let input_state = state.connect_input.as_ref();
            let (title, field_label, placeholder) = match input_state.map(|i| &i.kind) {
                Some(ConnectInputKind::ApiKey { display_name, .. }) => (
                    format!(
                        "Connect {} · API Key",
                        mehh_tui::sanitize_terminal_text(display_name, false)
                    ),
                    "API Key:",
                    "Paste or type your API key...",
                ),
                Some(ConnectInputKind::CustomProviderId) => (
                    "Custom Provider · Step 1/3".to_string(),
                    "Provider ID:",
                    "e.g. ollama, vllm, lmstudio",
                ),
                Some(ConnectInputKind::CustomBaseUrl { provider_id }) => (
                    format!(
                        "Custom Provider ({}) · Step 2/3",
                        mehh_tui::sanitize_terminal_text(provider_id, false)
                    ),
                    "Base URL:",
                    "e.g. http://localhost:11434/v1",
                ),
                Some(ConnectInputKind::CustomApiKey { provider_id, .. }) => (
                    format!(
                        "Custom Provider ({}) · Step 3/3",
                        mehh_tui::sanitize_terminal_text(provider_id, false)
                    ),
                    "API Key (optional):",
                    "Leave blank for keyless local endpoint",
                ),
                None => (
                    "Connect Provider".to_string(),
                    "Input:",
                    "Type value...",
                ),
            };
            let display_val = if let Some(inp) = input_state {
                if inp.value.is_empty() {
                    theme.fg(&theme.dim, &format!("{placeholder}▌"))
                } else if inp.masked {
                    let count = inp.value.chars().count().min(48);
                    theme.fg(&theme.text, &format!("{}▌", "•".repeat(count)))
                } else {
                    let clean_val = mehh_tui::sanitize_terminal_text(&inp.value, false);
                    theme.fg(&theme.text, &format!("{clean_val}▌"))
                }
            } else {
                theme.fg(&theme.dim, "▌")
            };
            let mut body_text = format!(
                "{}\n  {}\n",
                theme.fg(&theme.muted, field_label),
                display_val
            );
            if let Some(err) = input_state.and_then(|i| i.error.as_deref()) {
                let clean_err = mehh_tui::sanitize_terminal_text(err, false);
                body_text.push_str(&format!("\n{}\n", theme.fg(&theme.error, &format!("✗ {clean_err}"))));
            }
            body_text.push_str(&format!(
                "\n{}",
                theme.fg(
                    &theme.dim,
                    "Paste or type · Enter Confirm · Ctrl+U Clear · Esc Back"
                )
            ));
            let boxed = BorderedBox::new(Box::new(Text::new(body_text)))
                .with_title(title)
                .with_style(BorderStyle::Rounded)
                .with_border_color(&theme.border_accent)
                .with_title_color(&theme.text);
            bottom_lines.extend(boxed.render(width).into_iter().map(RenderedLine::plain));
        } else {
            let title = match selector.kind {
                SelectorKind::Model => "Select Model",
                SelectorKind::Mode => "Permission Mode",
                SelectorKind::Thinking => "Thinking Effort",
                SelectorKind::Theme => "Color Theme · live preview",
                SelectorKind::Tree => "Session Branches",
                SelectorKind::Resume => "Sessions",
                SelectorKind::Settings => "Settings",
                SelectorKind::Login => "Connect Provider",
                SelectorKind::ConnectMethod => "Connection Method",
                SelectorKind::ConnectInput => "Connect Provider",
                SelectorKind::ToolApproval => {
                    "Tool Approval Required (y/Enter: allow · a: session · A: always · n/Esc: deny)"
                }
            };
            let selector_border = if selector.kind == SelectorKind::ToolApproval {
                &theme.warning
            } else {
                &theme.border_accent
            };
            let show_search = selector.kind != SelectorKind::ToolApproval
                && selector.kind != SelectorKind::ConnectMethod;
            let footer_hint = if !show_search || selector.kind == SelectorKind::Settings {
                None
            } else if !selector.list.filter.is_empty() {
                Some("↑↓ Move · Enter Select · Backspace · Esc Clear filter")
            } else if selector.kind == SelectorKind::Theme {
                Some("↑↓ Preview · Enter Apply · Type to filter · Esc Cancel")
            } else {
                Some("↑↓ Move · Enter Select · Type to filter · Esc Close")
            };

            let search_rows = usize::from(show_search) * 2;
            let inner_width = width.saturating_sub(2);
            let mut row_actions: Vec<(usize, ClickAction)> = Vec::new();
            let body_start_row = 1 + search_rows;

            let (shown, total, body): (usize, usize, Box<dyn Component>) =
                if selector.kind == SelectorKind::Settings {
                    if let Some(ref sl) = selector.settings_list {
                        let max_h = Some(inner_modal_budget.saturating_sub(search_rows).max(1));
                        let (settings_lines, row_map, shown) =
                            sl.render_with_row_map_themed(inner_width, theme, max_h);
                        let total = sl.items.len();
                        for (i, mapping) in row_map.into_iter().enumerate() {
                            if let Some((filtered_idx, setting_id)) = mapping {
                                row_actions.push((
                                    body_start_row + i,
                                    ClickAction::ClickSettingsRow {
                                        filtered_idx,
                                        setting_id,
                                    },
                                ));
                            }
                        }
                        (shown, total, Box::new(mehh_tui::RawLines { lines: settings_lines }))
                    } else {
                        let filtered = selector.list.filtered_indices();
                        let shown = filtered.len();
                        let total = selector.list.items.len();
                        let (list_lines, _, _) = selector.list.render_with_filtered(
                            inner_width,
                            theme,
                            selector.list.max_visible,
                            &filtered,
                        );
                        (shown, total, Box::new(mehh_tui::RawLines { lines: list_lines }))
                    }
                } else {
                    let filtered = selector.list.filtered_indices();
                    let shown = filtered.len();
                    let total = selector.list.items.len();
                    let max_vis = if show_search {
                        let overhead = search_rows + usize::from(footer_hint.is_some()) * 2;
                        let max_rows = inner_modal_budget.saturating_sub(overhead).clamp(1, 10);
                        selector.list.max_visible.min(max_rows)
                    } else {
                        selector.list.max_visible
                    };
                    let (list_lines, vis_start, vis_end) =
                        selector.list.render_with_filtered(inner_width, theme, max_vis, &filtered);
                    for (i, pos) in (vis_start..vis_end).enumerate() {
                        let action = if selector.kind == SelectorKind::ToolApproval {
                            ClickAction::ApproveToolChoice(
                                selector.list.items[filtered[pos]].id.clone(),
                            )
                        } else {
                            ClickAction::SelectModalRow(pos)
                        };
                        row_actions.push((body_start_row + i, action));
                    }
                    (shown, total, Box::new(mehh_tui::RawLines { lines: list_lines }))
                };

            let modal = ModalPanel {
                search_bar: show_search.then_some((selector.list.filter.as_str(), shown, total)),
                body,
                footer_hint,
                max_height: Some(inner_modal_budget),
                theme,
            };
            let boxed = BorderedBox::new(Box::new(mehh_tui::RawLines {
                lines: modal.render_inner(inner_width),
            }))
                .with_title(title)
                .with_style(BorderStyle::Rounded)
                .with_border_color(selector_border)
                .with_title_color(&theme.text);
            let rendered_box = boxed.render(width);
            let mut box_rows: Vec<RenderedLine> =
                rendered_box.into_iter().map(RenderedLine::plain).collect();
            let col_end = width.saturating_sub(1);
            let search_chrome = if show_search { search_rows } else { 0 };
            let hint_chrome = usize::from(footer_hint.is_some()) * 2;
            let total_chrome = search_chrome + hint_chrome;
            let max_body_rows = if total_chrome >= inner_modal_budget {
                inner_modal_budget.saturating_sub(search_chrome)
            } else {
                inner_modal_budget.saturating_sub(total_chrome)
            };
            for (rel_row, action) in row_actions {
                if rel_row < body_start_row + max_body_rows
                    && rel_row + 1 < box_rows.len()
                    && col_end > 1
                {
                    box_rows[rel_row]
                        .regions
                        .push((1, col_end, action));
                }
            }
            bottom_lines.extend(box_rows);
        }
    }

    if has_tool_approval_early {
        let reserved = usize::from(state.scroll_offset > 0);
        if bottom_lines.len() + busy_lines.len() + reserved <= bottom_budget {
            bottom_lines.extend(busy_lines);
            if let Some(q_line) = queue_line {
                if bottom_lines.len() + 1 + reserved <= bottom_budget {
                    bottom_lines.push(q_line);
                }
            }
        }
    } else {
        bottom_lines.extend(busy_lines);
        if let Some(q_line) = queue_line {
            bottom_lines.push(q_line);
        }
    }

    // Prompt dock: a thin two-line frame keeps the input visually anchored.
    // Re-theme directly without cloning `Editor` (history, undo stack, and paste payloads).
    if state.selector.is_none() {
        let inner_dock_w = width.saturating_sub(2);
        let (ac_lines, vis_start, vis_end, ac_total) =
            editor.render_autocomplete_with_theme(inner_dock_w, theme);
        if !ac_lines.is_empty() {
            let ac_title = if let Some(ref ac) = editor.autocomplete {
                let pos = if ac_total == 0 {
                    0
                } else {
                    ac.selected.min(ac_total - 1) + 1
                };
                if width >= 56 && ac_total > 0 {
                    format!("Suggestions ({pos}/{ac_total}) · ↑↓ Move · Tab/Enter Accept")
                } else if width >= 26 && ac_total > 0 {
                    format!("Suggestions ({pos}/{ac_total})")
                } else {
                    "Suggestions".to_string()
                }
            } else {
                "Suggestions".to_string()
            };
            let panel = BorderedBox::new(Box::new(mehh_tui::RawLines { lines: ac_lines }))
                .with_title(ac_title)
                .with_style(BorderStyle::Single)
                .with_border_color(theme.border_accent.as_str())
                .with_title_color(theme.text.as_str());
            let mut ac_box: Vec<RenderedLine> = panel
                .render(width)
                .into_iter()
                .map(RenderedLine::plain)
                .collect();
            if editor.autocomplete.is_some() {
                let col_end = width.saturating_sub(1);
                for (i, pos) in (vis_start..vis_end).enumerate() {
                    let rel_row = 1 + i;
                    if rel_row + 1 < ac_box.len() && col_end > 1 {
                        ac_box[rel_row]
                            .regions
                            .push((1, col_end, ClickAction::SelectAutocompleteRow(pos)));
                    }
                }
            }
            bottom_lines.extend(ac_box);
        }
        let mut dock_title = if state.is_busy {
            "Steer (Enter) · Follow-up (Alt+Enter) · Cancel (Esc)".to_string()
        } else {
            "Prompt".to_string()
        };
        if editor.lines.len() > 1 {
            dock_title.push_str(&format!(
                " · Ln {}/{}",
                editor.cursor_row + 1,
                editor.lines.len()
            ));
        }
        let dock_border = if state.is_busy {
            theme.warning.as_str()
        } else {
            theme.border.as_str()
        };
        let max_lines = (height / 3).clamp(3, 8);
        let dock_lines = editor.render_with_options(inner_dock_w, theme, max_lines);
        let dock = BorderedBox::new(Box::new(mehh_tui::RawLines { lines: dock_lines }))
            .with_style(BorderStyle::Single)
            .with_border_color(dock_border)
            .with_title_color(theme.text.as_str())
            .with_title(dock_title);
        bottom_lines.extend(dock.render(width).into_iter().map(RenderedLine::plain));
    }

    // 4. Transcript — store cached Arc<[String]> segments and materialize only
    //    visible viewport lines so off-screen history does not clone strings every tick.
    let transcript_width = width.saturating_sub(1).max(1);
    let hide_thinking = session.settings.hide_thinking_block.unwrap_or(false);
    let mut segments: Vec<Arc<[String]>> = Vec::with_capacity(state.items.len().saturating_mul(2));
    let mut total_transcript_lines = 0usize;
    let blank_sep: Arc<[String]> = Arc::from([String::new()]);
    let thinking_elapsed_map = mehh_ai::lock_recover(&state.thinking_elapsed);
    let turn_started = *mehh_ai::lock_recover(&state.turn_started_at);
    let turn_completed = *mehh_ai::lock_recover(&state.turn_completed_at);
    let live_elapsed_ms = match (turn_started, turn_completed) {
        (Some(s), None) => Some(s.elapsed().as_millis() as u64),
        (Some(s), Some(c)) => Some(c.saturating_duration_since(s).as_millis() as u64),
        (None, _) => None,
    };
    let mut cache = mehh_ai::lock_recover(&state.transcript_cache);
    if cache.len() > state.items.len() {
        cache.truncate(state.items.len());
    } else if cache.len() < state.items.len() {
        cache.resize_with(state.items.len(), || None);
    }
    let is_animating_status = |s: &str, err: bool| {
        !err && !matches!(s, "failed" | "completed" | "blocked")
    };
    let last_user_idx = state
        .items
        .iter()
        .rposition(|i| matches!(i, TranscriptItem::User(_)));
    let last_assistant_idx = state
        .items
        .iter()
        .rposition(|i| matches!(i, TranscriptItem::Assistant { .. }))
        .filter(|&a_idx| last_user_idx.is_none_or(|u_idx| a_idx > u_idx));
    for (item_idx, item) in state.items.iter().enumerate() {
        // Breathing room between turns; the transcript is a timeline, not a log dump.
        if item_idx > 0 {
            segments.push(Arc::clone(&blank_sep));
            total_transcript_lines += 1;
        }
        let needs_frame = match item {
            TranscriptItem::ToolCall {
                status, is_error, ..
            } => is_animating_status(status, *is_error),
            TranscriptItem::Subagent {
                status,
                is_error,
                children,
                ..
            } => {
                is_animating_status(status, *is_error)
                    || children
                        .iter()
                        .any(|r| is_animating_status(&r.status, false))
            }
            _ => false,
        };
        let is_current_item = state.is_busy && item_idx + 1 == state.items.len();
        let allow_live_elapsed = Some(item_idx) == last_assistant_idx;
        let thinking_elapsed_opt = thinking_elapsed_map.get(&item_idx).copied();
        let shows_collapsed_elapsed = matches!(
            item,
            TranscriptItem::Assistant { thinking, .. }
            if !thinking.is_empty() && !hide_thinking && !state.thinking_expanded && !is_current_item
        );
        let raw_elapsed_ms = if shows_collapsed_elapsed {
            thinking_elapsed_opt
                .or_else(|| allow_live_elapsed.then_some(live_elapsed_ms).flatten())
        } else {
            None
        };
        let effective_elapsed_tenths = raw_elapsed_ms
            .map_or(0, |ms| ((ms as f64 / 1000.0) * 10.0).round() as u64);
        let flags = (hide_thinking as u8)
            | ((state.thinking_expanded as u8) << 1)
            | ((is_current_item as u8) << 2);
        let frame_key = if needs_frame { state.loader.frame } else { 0 };
        let item_hash = hash_transcript_item(item);
        if let Some(ref entry) = cache[item_idx] {
            if entry.width == transcript_width
                && entry.flags == flags
                && entry.frame == frame_key
                && entry.thinking_elapsed == effective_elapsed_tenths
                && entry.theme_name == theme.name
                && entry.theme_generation == state.theme_generation
                && entry.item_hash == item_hash
            {
                total_transcript_lines += entry.lines.len();
                segments.push(Arc::clone(&entry.lines));
                continue;
            }
        }
        let mut thinking_hash = [0u8; 16];
        let mut thinking_lines: Arc<[String]> = Arc::from([]);
        let mut text_hash = [0u8; 16];
        let mut text_lines: Arc<[String]> = Arc::from([]);
        let mut streaming_md: Option<super::state::StreamingMarkdownPrefix> = None;
        let mut item_lines = Vec::new();
        match item {
            TranscriptItem::User(text) => {
                item_lines.extend(render_user_turn(text, transcript_width, theme));
            }
            TranscriptItem::Assistant { thinking, text } => {
                let prev_entry = cache[item_idx].as_ref();
                if !thinking.is_empty() && !hide_thinking {
                    thinking_hash = hash_str_128(thinking);
                    let reuse_thinking = prev_entry.is_some_and(|prev| {
                        prev.width == transcript_width
                            && prev.flags == flags
                            && prev.thinking_elapsed == effective_elapsed_tenths
                            && prev.theme_name == theme.name
                            && prev.theme_generation == state.theme_generation
                            && prev.thinking_hash == thinking_hash
                    });
                    if reuse_thinking {
                        if let Some(prev) = prev_entry {
                            thinking_lines = Arc::clone(&prev.thinking_lines);
                        }
                    } else {
                        let clean_thinking = mehh_tui::sanitize_terminal_text_cow(thinking, true);
                        let preview = thinking_preview(
                            &clean_thinking,
                            state.thinking_expanded,
                            is_current_item,
                            raw_elapsed_ms,
                        );
                        let formatted = theme.fg(&theme.thinking_text, &preview);
                        let rendered_thinking = wrap_text_with_ansi(&formatted, transcript_width);
                        thinking_lines = Arc::from(rendered_thinking);
                    }
                    item_lines.extend(thinking_lines.iter().cloned());
                }
                if !text.is_empty() {
                    text_hash = hash_str_128(text);
                    let reuse_text = prev_entry.is_some_and(|prev| {
                        prev.width == transcript_width
                            && prev.theme_name == theme.name
                            && prev.theme_generation == state.theme_generation
                            && prev.text_hash == text_hash
                    });
                    if reuse_text {
                        if let Some(prev) = prev_entry {
                            text_lines = Arc::clone(&prev.text_lines);
                            streaming_md = prev.streaming_md.clone();
                        }
                    } else {
                        let clean_text = mehh_tui::sanitize_terminal_text_cow(text, true);
                        let (rendered_md, md_cache) = render_markdown_incremental(
                            &clean_text,
                            theme,
                            transcript_width,
                            prev_entry.and_then(|prev| prev.streaming_md.as_ref()),
                            state.theme_generation,
                        );
                        text_lines = Arc::from(rendered_md);
                        streaming_md = md_cache;
                    }
                    item_lines.extend(text_lines.iter().cloned());
                }
            }
            TranscriptItem::ToolCall {
                id: _id,
                name,
                status,
                is_error,
                args_summary,
                output_preview,
                diff,
                duration_ms,
            } => {
                let clean_name = mehh_tui::sanitize_terminal_text_cow(name, false);
                let clean_status = mehh_tui::sanitize_terminal_text_cow(status, false);
                let clean_args = mehh_tui::sanitize_terminal_text_cow(args_summary, false);
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let summary_part = if !clean_args.is_empty() {
                    format!(" {}", theme.fg(&theme.accent, &clean_args))
                } else {
                    String::new()
                };
                let status_part = if status == "completed" {
                    String::new()
                } else {
                    format!(" {}", theme.fg(&theme.dim, &clean_status))
                };
                let duration_part = tool_duration_label(*duration_ms);
                let formatted = format!(
                    "  {sym} {}{summary_part}{status_part}{duration_part}",
                    theme.fg(&theme.text, &clean_name)
                );
                item_lines.extend(wrap_text_with_ansi(&formatted, transcript_width));

                for line in output_preview {
                    let clean_line = mehh_tui::sanitize_terminal_text_cow(line, false);
                    let preview_line = format!("    {}", theme.fg(&theme.dim, &clean_line));
                    item_lines.extend(wrap_text_with_ansi(&preview_line, transcript_width));
                }

                if let Some(diff_str) = diff {
                    let clean_diff = mehh_tui::sanitize_terminal_text_cow(diff_str, true);
                    let (visible, hidden) = diff_visible_lines(&clean_diff);
                    for line in visible {
                        let (color, bg): (&str, &str) =
                            if line.starts_with('+') && !line.starts_with("+++") {
                                (theme.tool_success.as_str(), theme.diff_add_bg.as_str())
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                (theme.tool_error.as_str(), theme.diff_del_bg.as_str())
                            } else if line.starts_with("@@") {
                                (theme.accent.as_str(), "")
                            } else {
                                (theme.dim.as_str(), "")
                            };
                        let colorized = format!("{color}{bg}{line}");
                        for wrapped in wrap_text_with_ansi(&format!("    {colorized}"), transcript_width) {
                            item_lines.push(format!("{wrapped}\x1b[0m"));
                        }
                    }
                    if hidden > 0 {
                        item_lines.push(format!(
                            "    {}",
                            theme.fg(
                                &theme.dim,
                                &format!("… (+{hidden} more lines)")
                            )
                        ));
                    }
                }
            }
            TranscriptItem::Subagent {
                id: _id,
                mode,
                status,
                is_error,
                children,
            } => {
                let clean_mode = mehh_tui::sanitize_terminal_text_cow(mode, false);
                let clean_status = mehh_tui::sanitize_terminal_text_cow(status, false);
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let formatted = format!(
                    "  {sym} subagent {} {}",
                    theme.fg(&theme.accent, &clean_mode),
                    theme.fg(&theme.dim, &clean_status)
                );
                item_lines.extend(wrap_text_with_ansi(&formatted, transcript_width));

                for row in children {
                    let clean_label = mehh_tui::sanitize_terminal_text_cow(&row.label, false);
                    let clean_row_status =
                        mehh_tui::sanitize_terminal_text_cow(&row.status, false);
                    let clean_detail = mehh_tui::sanitize_terminal_text_cow(&row.detail, false);
                    let row_sym =
                        status_icon(&row.status, false, state.loader.frame, theme);
                    let mut row_line =
                        format!("    {row_sym} {}", theme.fg(&theme.text, &clean_label));
                    if row.label != row.status && !clean_row_status.is_empty() {
                        row_line.push_str(&format!(": {}", theme.fg(&theme.dim, &clean_row_status)));
                    }
                    // A finished tool row has detail == status ("completed");
                    // omit the redundant suffix instead of printing it twice.
                    if !clean_detail.is_empty() && row.detail != row.status {
                        row_line.push_str(&format!(" {}", theme.fg(&theme.dim, &clean_detail)));
                    }
                    item_lines.extend(wrap_text_with_ansi(&row_line, transcript_width));
                }
            }
            TranscriptItem::System(text) => {
                let clean_text = mehh_tui::sanitize_terminal_text_cow(text, true);
                for sub in clean_text.split('\n') {
                    let formatted = theme.fg(&theme.dim, &format!("• {sub}"));
                    item_lines.extend(wrap_text_with_ansi(&formatted, transcript_width));
                }
            }
        }
        let rendered: Arc<[String]> = Arc::from(item_lines);
        total_transcript_lines += rendered.len();
        cache[item_idx] = Some(super::state::CachedTranscriptEntry {
            width: transcript_width,
            flags,
            frame: frame_key,
            thinking_elapsed: effective_elapsed_tenths,
            theme_name: theme.name.clone(),
            theme_generation: state.theme_generation,
            item_hash,
            thinking_hash,
            thinking_lines,
            text_hash,
            text_lines,
            streaming_md,
            lines: Arc::clone(&rendered),
        });
        segments.push(rendered);
    }
    drop(cache);
    drop(thinking_elapsed_map);

    let prev_len = state.prev_transcript_len.load(Ordering::Relaxed);
    let growth = total_transcript_lines.saturating_sub(prev_len);
    state
        .prev_transcript_len
        .store(total_transcript_lines, Ordering::Relaxed);
    let anchor = anchored_scroll_offset(
        state.scroll_anchor.load(Ordering::Relaxed),
        growth,
        state.scroll_offset,
    );
    state.scroll_anchor.store(anchor, Ordering::Relaxed);
    let anchored = state.scroll_offset.saturating_add(anchor);

    let has_tool_approval = state.has_tool_approval();
    let has_modal = state.selector.is_some() || has_tool_approval;
    let has_dismissable_modal = state.selector.is_some() && !has_tool_approval;

    if state.scroll_offset > 0 {
        let pill_height = 1usize;
        let pre_height = height.saturating_sub(
            header_lines.len() + footer_lines.len() + bottom_lines.len() + pill_height,
        );
        let max_scroll = total_transcript_lines.saturating_sub(pre_height);
        state.last_max_scroll.store(max_scroll, Ordering::Relaxed);
        if max_scroll > 0 {
            let pill = scroll_pill_text(anchored.min(max_scroll), max_scroll, anchor);
            let pill_line = truncate_to_width(
                &theme.fg(&theme.warning, &pill),
                width,
                "...",
                false,
            );
            let mut line = RenderedLine::plain(pill_line);
            if !has_modal {
                line = line.with_action(0, width, ClickAction::ScrollToBottom);
            } else if has_dismissable_modal {
                line = line.with_action(0, width, ClickAction::CloseModal);
            }
            bottom_lines.push(line);
        }
    }

    // Keep the assembled frame within the terminal height: drop optional panel
    // rows from the front while preserving the dock at the end and approval actions.
    let bottom_budget = height
        .saturating_sub(header_lines.len() + footer_lines.len() + 1)
        .max(1);
    if bottom_lines.len() > bottom_budget {
        let mut excess = bottom_lines.len() - bottom_budget;
        let mut idx = 0;
        while excess > 0 && idx < bottom_lines.len().saturating_sub(3) {
            let line = &bottom_lines[idx].text;
            if line.contains("Allow")
                || line.contains("Deny")
                || line.contains("Suggestions")
                || (has_tool_approval
                    && (line.contains("Command preview")
                        || line.contains("tool")
                        || line.contains("… (+")
                        || idx <= 3))
            {
                idx += 1;
                continue;
            }
            bottom_lines.remove(idx);
            excess -= 1;
        }
        while excess > 0 && bottom_lines.len() > bottom_budget {
            let drop_idx = if has_tool_approval {
                (0..bottom_lines.len()).rev().find(|&i| {
                    let t = &bottom_lines[i].text;
                    !t.contains("Command preview")
                        && !t.contains("tool")
                        && !t.contains("Allow")
                        && !t.contains("Deny")
                        && i > 3
                })
            } else {
                None
            };
            match drop_idx {
                Some(i) => {
                    bottom_lines.remove(i);
                    excess -= 1;
                }
                None => break,
            }
        }
    }

    // 5. Fit transcript into remaining vertical space
    let used_lines = header_lines.len() + footer_lines.len() + bottom_lines.len();
    let available_height = height.saturating_sub(used_lines);

    let mut visible_transcript = if available_height == 0 {
        state.last_max_scroll.store(0, Ordering::Relaxed);
        Vec::new()
    } else if total_transcript_lines > available_height {
        let max_scroll = total_transcript_lines - available_height;
        state.last_max_scroll.store(max_scroll, Ordering::Relaxed);
        let offset = anchored.min(max_scroll);
        let end = total_transcript_lines - offset;
        let start = end.saturating_sub(available_height);
        slice_transcript_segments(&segments, start, end)
    } else {
        state.last_max_scroll.store(0, Ordering::Relaxed);
        slice_transcript_segments(&segments, 0, total_transcript_lines)
    };

    if total_transcript_lines > available_height && width > 0 {
        let shown = visible_transcript.len();
        let thumb_marker = theme.fg(&theme.accent, "┃");
        let track_marker = theme.fg(&theme.border_muted, "│");
        for (row, line) in visible_transcript.iter_mut().enumerate() {
            let base = truncate_to_width(line, transcript_width, "...", true);
            let marker = scrollbar_char(row, shown, total_transcript_lines, anchored);
            let colored = if marker == '┃' {
                &thumb_marker
            } else {
                &track_marker
            };
            let mut out = String::with_capacity(base.len() + colored.len());
            out.push_str(&base);
            out.push_str(colored);
            *line = out;
        }
    }

    // Anchor the prompt dock and footer to the bottom row. Without this pad the
    // editor sits directly under the header and drifts down with every message.
    visible_transcript.resize_with(available_height, String::new);

    let mut frame = Vec::with_capacity(height);
    let mut regions = Vec::new();

    for hl in header_lines {
        let row = frame.len();
        if !has_modal {
            for (col_start, col_end, action) in hl.regions {
                regions.push(ClickRegion {
                    row,
                    col_start,
                    col_end,
                    action,
                });
            }
        } else if has_dismissable_modal {
            regions.push(ClickRegion {
                row,
                col_start: 0,
                col_end: width,
                action: ClickAction::CloseModal,
            });
        }
        frame.push(hl.text);
    }

    for line in visible_transcript {
        let row = frame.len();
        if has_dismissable_modal {
            regions.push(ClickRegion {
                row,
                col_start: 0,
                col_end: width,
                action: ClickAction::CloseModal,
            });
        }
        frame.push(line);
    }

    for bl in bottom_lines {
        let row = frame.len();
        for (col_start, col_end, action) in bl.regions {
            regions.push(ClickRegion {
                row,
                col_start,
                col_end,
                action,
            });
        }
        frame.push(bl.text);
    }

    for fl in footer_lines {
        let row = frame.len();
        if !has_modal {
            for (col_start, col_end, action) in fl.regions {
                regions.push(ClickRegion {
                    row,
                    col_start,
                    col_end,
                    action,
                });
            }
        } else if has_dismissable_modal {
            regions.push(ClickRegion {
                row,
                col_start: 0,
                col_end: width,
                action: ClickAction::CloseModal,
            });
        }
        frame.push(fl.text);
    }

    frame.truncate(height);
    regions.retain(|r| r.row < frame.len());
    *state.click_regions.lock().unwrap_or_else(|e| e.into_inner()) = regions;

    frame
}


/// Renders the current frame to the terminal.
///
/// `TuiRenderer` positions changed rows absolutely, so only changed rows are
/// rewritten. `reset()` is reserved for a resize, where the previous frame's
/// geometry is no longer a valid baseline.
pub(super) fn render_now(
    state: &InteractiveState,
    editor: &Editor,
    session: &AgentSession,
    terminal: &mut dyn Terminal,
    renderer: &mut TuiRenderer,
) {
    let (c, r) = terminal.size();
    let lines = render_interactive_frame(state, editor, session, c as usize, r as usize);

    // The renderer positions changed rows absolutely, so no homing or reset is
    // needed here. A software cursor marks the input position.
    let _ = renderer.render_frame(&mut *terminal, lines, false);
}

