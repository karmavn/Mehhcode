use super::*;

pub(super) fn scrollbar_char(row: usize, visible: usize, total: usize, offset: usize) -> char {
    if visible == 0 || total <= visible {
        return '│';
    }
    let max_scroll = total - visible;
    let thumb_row = 1 + offset.min(max_scroll) * visible.saturating_sub(2) / max_scroll.max(1);
    if row == thumb_row {
        '┃'
    } else {
        '│'
    }
}

pub(super) fn cap_preview_lines(preview: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = preview.lines().collect();
    if lines.len() <= max_lines {
        return preview.to_string();
    }
    let hidden = lines.len() - max_lines;
    let mut out = lines[..max_lines].join("\n");
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
    let all: Vec<&str> = diff.lines().collect();
    let hidden = all.len().saturating_sub(24);
    (all.into_iter().take(24).collect(), hidden)
}

pub(super) fn thinking_preview(
    thinking: &str,
    expanded: bool,
    busy: bool,
    elapsed_ms: Option<u64>,
) -> String {
    let lines: Vec<&str> = thinking.lines().collect();
    if expanded {
        return thinking.to_string();
    }
    let count = lines.len();
    if busy {
        let tail = lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        format!("⠋ Thinking ({count} lines)...\n{tail}")
    } else {
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

    fn push_styled(&mut self, color: &str, text: &str, theme: &Theme) {
        self.raw.push_str(&theme.fg(color, text));
        self.col += mehh_tui::visible_width(text);
    }

    fn push_prestyled(&mut self, styled: &str) {
        self.raw.push_str(styled);
        self.col += mehh_tui::visible_width(styled);
    }

    fn push_clickable(&mut self, color: &str, text: &str, theme: &Theme, action: ClickAction) {
        let start = self.col;
        let w = mehh_tui::visible_width(text);
        self.raw.push_str(&theme.fg(color, text));
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
        let text = truncate_to_width(&self.raw, width, "...", false);
        let max_w = mehh_tui::visible_width(&text).min(width);
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
    let right_text = truncate_to_width(&right.raw, width, "...", false);
    let right_w = mehh_tui::visible_width(&right_text);
    let left_budget = width.saturating_sub(right_w);
    let left_text = truncate_to_width(&left.raw, left_budget, "...", false);
    let left_w = mehh_tui::visible_width(&left_text);
    let pad = width.saturating_sub(left_w + right_w);

    let mut regions = Vec::with_capacity(left.regions.len() + right.regions.len());
    for (s, e, action) in left.regions {
        if s < left_w {
            let clamped_e = e.min(left_w);
            if clamped_e > s {
                regions.push((s, clamped_e, action));
            }
        }
    }
    let shift = left_w + pad;
    for (s, e, action) in right.regions {
        let ns = s.saturating_add(shift);
        let ne = e.saturating_add(shift).min(width);
        if ns < width && ne > ns {
            regions.push((ns, ne, action));
        }
    }

    RenderedLine {
        text: truncate_to_width(
            &format!("{left_text}{}{right_text}", " ".repeat(pad)),
            width,
            "...",
            false,
        ),
        regions,
    }
}

pub(super) fn left_right(left: String, right: String, width: usize) -> String {
    let right_w = mehh_tui::visible_width(&right);
    let left_w = width.saturating_sub(right_w);
    let left = truncate_to_width(&left, left_w, "...", false);
    let pad = width.saturating_sub(mehh_tui::visible_width(&left) + right_w);
    format!("{left}{}{right}", " ".repeat(pad))
}

pub(super) fn ctx_bar(pct: u64, theme: &Theme) -> String {
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
    let mut out = theme.fg(color, &"█".repeat(filled));
    out.push_str(&theme.fg(&theme.border_muted, &"░".repeat(8 - filled)));
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
    let inner = width.saturating_sub(4);
    wrap_text_with_ansi(text, inner)
        .into_iter()
        .map(|line| {
            let body_w = mehh_tui::visible_width(&line);
            let padding = " ".repeat(width.saturating_sub(1 + body_w));
            format!(
                "{prefix}▌{reset}{bg}{color}{line}{padding}{reset}",
                prefix = theme.user_prefix,
                reset = "\x1b[0m",
                bg = theme.user_bg,
                color = theme.text,
            )
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
struct ModalPanel {
    search_bar: Option<(String, usize, usize)>,
    body: Box<dyn Component>,
    footer_hint: Option<&'static str>,
    max_height: Option<usize>,
    theme: Theme,
}

impl Component for ModalPanel {
    fn render(&self, width: usize) -> Vec<String> {
        let mut search_lines = Vec::new();
        if let Some((ref filter, shown, total)) = self.search_bar {
            let left = if filter.is_empty() {
                self.theme.fg(&self.theme.dim, "  / Type to filter...")
            } else {
                self.theme.fg(&self.theme.accent, &format!("  / {filter}▌"))
            };
            let right = self.theme.fg(
                if filter.is_empty() {
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
    format!("↑ Paused at {pct}% · +{new_lines} new lines · End to jump")
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
    let header_lines: Vec<RenderedLine> =
        if state.items.is_empty() && editor.autocomplete.is_none() && state.selector.is_none() {
            let cwd_str = session.cwd.to_string_lossy();
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
                    &session.model.name,
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
            let short_id: String = session.session().session_id().chars().take(8).collect();
            let sess_label = session
                .session()
                .session_name()
                .map(|n| format!("session: {} ▾", n.chars().take(16).collect::<String>()))
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
                    &session.model.name,
                    theme,
                    ClickAction::OpenModelPicker,
                );
                left.push_styled(
                    &theme.dim,
                    &format!(" ({})", session.model.provider),
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
            right.push_styled(
                &theme.dim,
                &format!(" · {}", compact_path(&session.cwd, 14)),
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
    let thinking_str = if !thinking_profile.supported {
        "n/a"
    } else {
        session
            .thinking_level()
            .map(thinking_level_label)
            .unwrap_or("off")
    };
    let thinking_active = thinking_profile.supported
        && session
            .thinking_level()
            .is_some_and(|l| l != ThinkingLevel::Off);
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
        let loader_line = format!(
            "{} {} {} · Esc to cancel",
            theme.fg(&theme.accent, spinner),
            theme.fg(&theme.text, &state.loader.label),
            theme.fg(&theme.dim, &elapsed)
        );
        busy_lines = wrap_text_with_ansi(&loader_line, width)
            .into_iter()
            .map(RenderedLine::plain)
            .collect();
        if let Some(ref st) = state.status {
            busy_lines.push(RenderedLine::plain(truncate_to_width(
                &format!("  {}", theme.fg(&theme.warning, st)),
                width,
                "...",
                false,
            )));
        }
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

    let post_modal_rows =
        busy_lines.len() + usize::from(queue_line.is_some()) + usize::from(state.scroll_offset > 0);
    let bottom_budget = height
        .saturating_sub(header_lines.len() + footer_lines.len() + 1)
        .max(1);
    let modal_budget = bottom_budget.saturating_sub(post_modal_rows).max(1);
    let mut bottom_lines: Vec<RenderedLine> = Vec::new();
    if let Some(ref selector) = state.selector {
        let mut preview_lines = Vec::new();
        if selector.kind == SelectorKind::ToolApproval {
            if let Some(ref pending) = state.pending_approval {
                let mut detail = format!(
                    "{}  {}\n{}\n",
                    theme.fg(&theme.accent, "tool"),
                    theme.fg(&theme.text, &pending.tool_name),
                    theme.fg(&theme.muted, &pending.reason)
                );
                if let Some(ref preview) = pending.preview {
                    let wrapped: Vec<String> = preview
                        .lines()
                        .flat_map(|line| wrap_text_with_ansi(line, width.saturating_sub(4)))
                        .collect();
                    let preview_budget = height.saturating_sub(18).clamp(1, 6);
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
                    format!("Connect {display_name} · API Key"),
                    "API Key:",
                    "Paste or type your API key...",
                ),
                Some(ConnectInputKind::CustomProviderId) => (
                    "Custom Provider · Step 1/3".to_string(),
                    "Provider ID:",
                    "e.g. ollama, vllm, lmstudio",
                ),
                Some(ConnectInputKind::CustomBaseUrl { provider_id }) => (
                    format!("Custom Provider ({provider_id}) · Step 2/3"),
                    "Base URL:",
                    "e.g. http://localhost:11434/v1",
                ),
                Some(ConnectInputKind::CustomApiKey { provider_id, .. }) => (
                    format!("Custom Provider ({provider_id}) · Step 3/3"),
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
                    theme.fg(&theme.text, &format!("{}▌", inp.value))
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
                body_text.push_str(&format!("\n{}\n", theme.fg(&theme.error, &format!("✗ {err}"))));
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
                        let mut fitted = sl.clone().with_theme(theme.clone());
                        let shown = fitted.filtered_indices().len();
                        let total = fitted.items.len();
                        fitted.max_height =
                            Some(inner_modal_budget.saturating_sub(search_rows).max(1));
                        let (settings_lines, row_map) = fitted.render_with_row_map(inner_width);
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
                        let shown = selector.list.filtered_indices().len();
                        let total = selector.list.items.len();
                        (
                            shown,
                            total,
                            Box::new(selector.list.clone().with_theme(theme.clone())),
                        )
                    }
                } else {
                    let mut fitted = selector.list.clone().with_theme(theme.clone());
                    let shown = fitted.filtered_indices().len();
                    let total = fitted.items.len();
                    if show_search {
                        let overhead = search_rows + usize::from(footer_hint.is_some()) * 2;
                        let max_rows = inner_modal_budget.saturating_sub(overhead).clamp(1, 10);
                        fitted.max_visible = fitted.max_visible.min(max_rows);
                    }
                    let (vis_start, vis_end, filtered) = fitted.visible_range();
                    for (i, pos) in (vis_start..vis_end).enumerate() {
                        let action = if selector.kind == SelectorKind::ToolApproval {
                            ClickAction::ApproveToolChoice(fitted.items[filtered[pos]].id.clone())
                        } else {
                            ClickAction::SelectModalRow(pos)
                        };
                        row_actions.push((body_start_row + i, action));
                    }
                    (shown, total, Box::new(fitted))
                };

            let modal = ModalPanel {
                search_bar: show_search.then(|| (selector.list.filter.clone(), shown, total)),
                body,
                footer_hint,
                max_height: Some(inner_modal_budget),
                theme: theme.clone(),
            };
            let boxed = BorderedBox::new(Box::new(modal))
                .with_title(title)
                .with_style(BorderStyle::Rounded)
                .with_border_color(selector_border)
                .with_title_color(&theme.text);
            let rendered_box = boxed.render(width);
            let mut box_rows: Vec<RenderedLine> =
                rendered_box.into_iter().map(RenderedLine::plain).collect();
            let col_end = width.saturating_sub(1);
            let max_body_rows = if show_search {
                inner_modal_budget
                    .saturating_sub(search_rows + usize::from(footer_hint.is_some()) * 2)
            } else {
                inner_modal_budget
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

    bottom_lines.extend(busy_lines);
    if let Some(q_line) = queue_line {
        bottom_lines.push(q_line);
    }

    // Prompt dock: a thin two-line frame keeps the input visually anchored.
    // Re-theme the cloned view so `/theme` applies instantly, including to
    // autocomplete and paste chips.
    if state.selector.is_none() {
        let mut themed_editor = editor.clone();
        themed_editor.theme = theme.clone();
        if let Some(ac) = themed_editor.autocomplete.as_mut() {
            ac.theme = theme.clone();
        }
        let max_lines = (height / 3).clamp(3, 8);
        themed_editor.max_lines = max_lines;
        let ac_lines = themed_editor.render_autocomplete(width.saturating_sub(2));
        if !ac_lines.is_empty() {
            let ac_title = if let Some(ref ac) = themed_editor.autocomplete {
                let total = ac.filtered_indices().len();
                let pos = if total == 0 {
                    0
                } else {
                    ac.selected.min(total - 1) + 1
                };
                if width >= 56 && total > 0 {
                    format!("Suggestions ({pos}/{total}) · ↑↓ Move · Tab/Enter Accept")
                } else if width >= 26 && total > 0 {
                    format!("Suggestions ({pos}/{total})")
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
            if let Some(ref ac) = themed_editor.autocomplete {
                let (vis_start, vis_end, _) = ac.visible_range();
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
        if themed_editor.lines.len() > 1 {
            dock_title.push_str(&format!(
                " · Ln {}/{}",
                themed_editor.cursor_row + 1,
                themed_editor.lines.len()
            ));
        }
        let dock_border = if state.is_busy {
            theme.warning.as_str()
        } else {
            theme.border.as_str()
        };
        let dock = BorderedBox::new(Box::new(themed_editor))
            .with_style(BorderStyle::Single)
            .with_border_color(dock_border)
            .with_title_color(theme.text.as_str())
            .with_title(dock_title);
        bottom_lines.extend(dock.render(width).into_iter().map(RenderedLine::plain));
    }

    // 4. Transcript — store cached Arc<[String]> segments and materialize only
    //    visible viewport lines so off-screen history does not clone strings every tick.
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
    if cache.len() > state.items.len().saturating_mul(2).saturating_add(16) {
        cache.clear();
    }
    for (item_idx, item) in state.items.iter().enumerate() {
        // Breathing room between turns; the transcript is a timeline, not a log dump.
        if item_idx > 0 {
            segments.push(Arc::clone(&blank_sep));
            total_transcript_lines += 1;
        }
        let needs_frame = match item {
            TranscriptItem::ToolCall { status, .. }
            | TranscriptItem::Subagent { status, .. } => status == "running",
            _ => false,
        };
        let thinking_elapsed_opt = thinking_elapsed_map.get(&item_idx).copied();
        let thinking_elapsed = thinking_elapsed_opt.unwrap_or(0);
        let flags = (hide_thinking as u8)
            | ((state.thinking_expanded as u8) << 1)
            | (((state.is_busy && item_idx + 1 == state.items.len()) as u8) << 2);
        let key = (
            item_idx,
            width,
            flags,
            if needs_frame { state.loader.frame } else { 0 },
            thinking_elapsed,
        );
        if let Some((cached_theme, cached_item, cached_lines)) = cache.get(&key) {
            if cached_theme == &theme.name && cached_item == item {
                total_transcript_lines += cached_lines.len();
                segments.push(Arc::clone(cached_lines));
                continue;
            }
        }
        let mut item_lines = Vec::new();
        match item {
            TranscriptItem::User(text) => {
                item_lines.extend(render_user_turn(text, width, theme));
            }
            TranscriptItem::Assistant { thinking, text } => {
                if !thinking.is_empty() && !hide_thinking {
                    let is_current = state.is_busy && item_idx + 1 == state.items.len();
                    let elapsed_ms = thinking_elapsed_opt.or(live_elapsed_ms);
                    let preview = thinking_preview(
                        thinking,
                        state.thinking_expanded,
                        is_current,
                        elapsed_ms,
                    );
                    let formatted = theme.fg(&theme.thinking_text, &preview);
                    item_lines.extend(wrap_text_with_ansi(&formatted, width));
                }
                if !text.is_empty() {
                    let rendered = MarkdownView::new(text)
                        .with_theme(theme.clone())
                        .render(width);
                    item_lines.extend(rendered);
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
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let summary_part = if !args_summary.is_empty() {
                    format!(" {}", theme.fg(&theme.accent, args_summary))
                } else {
                    String::new()
                };
                let status_part = if status == "completed" {
                    String::new()
                } else {
                    format!(" {}", theme.fg(&theme.dim, status))
                };
                let duration_part = tool_duration_label(*duration_ms);
                let formatted = format!(
                    "  {sym} {}{summary_part}{status_part}{duration_part}",
                    theme.fg(&theme.text, name)
                );
                item_lines.extend(wrap_text_with_ansi(&formatted, width));

                for line in output_preview {
                    let preview_line = format!("    {}", theme.fg(&theme.dim, line));
                    item_lines.extend(wrap_text_with_ansi(&preview_line, width));
                }

                if let Some(diff_str) = diff {
                    let (visible, hidden) = diff_visible_lines(diff_str);
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
                        for wrapped in wrap_text_with_ansi(&format!("    {colorized}"), width) {
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
                let sym = status_icon(status, *is_error, state.loader.frame, theme);
                let formatted = format!(
                    "  {sym} subagent {} {}",
                    theme.fg(&theme.accent, mode),
                    theme.fg(&theme.dim, status)
                );
                item_lines.extend(wrap_text_with_ansi(&formatted, width));

                for row in children {
                    let row_sym =
                        status_icon(&row.status, false, state.loader.frame, theme);
                    let mut row_line =
                        format!("    {row_sym} {}", theme.fg(&theme.text, &row.label));
                    if row.label != row.status && !row.status.is_empty() {
                        row_line.push_str(&format!(": {}", theme.fg(&theme.dim, &row.status)));
                    }
                    // A finished tool row has detail == status ("completed");
                    // omit the redundant suffix instead of printing it twice.
                    if !row.detail.is_empty() && row.detail != row.status {
                        row_line.push_str(&format!(" {}", theme.fg(&theme.dim, &row.detail)));
                    }
                    item_lines.extend(wrap_text_with_ansi(&row_line, width));
                }
            }
            TranscriptItem::System(text) => {
                for sub in text.split('\n') {
                    let formatted = theme.fg(&theme.dim, &format!("• {sub}"));
                    item_lines.extend(wrap_text_with_ansi(&formatted, width));
                }
            }
        }
        let rendered: Arc<[String]> = Arc::from(item_lines);
        total_transcript_lines += rendered.len();
        cache.insert(key, (theme.name.clone(), item.clone(), Arc::clone(&rendered)));
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

    if state.scroll_offset > 0 {
        let pill_height = 1usize;
        let pre_height = height.saturating_sub(
            header_lines.len() + footer_lines.len() + bottom_lines.len() + pill_height,
        );
        let max_scroll = total_transcript_lines.saturating_sub(pre_height);
        state.last_max_scroll.store(max_scroll, Ordering::Relaxed);
        let pill = scroll_pill_text(anchored.min(max_scroll), max_scroll, anchor);
        let pill_line = truncate_to_width(
            &theme.fg(&theme.warning, &pill),
            width,
            "...",
            false,
        );
        bottom_lines.push(
            RenderedLine::plain(pill_line).with_action(0, width, ClickAction::ScrollToBottom),
        );
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
            if line.contains("Allow") || line.contains("Deny") || line.contains("Suggestions") {
                idx += 1;
                continue;
            }
            bottom_lines.remove(idx);
            excess -= 1;
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
        for (row, line) in visible_transcript.iter_mut().enumerate() {
            let base = truncate_to_width(line, width.saturating_sub(1), "...", false);
            let marker = scrollbar_char(row, shown, total_transcript_lines, anchored);
            let marker_s = marker.to_string();
            let colored = if marker == '┃' {
                theme.fg(&theme.accent, &marker_s)
            } else {
                theme.fg(&theme.border_muted, &marker_s)
            };
            *line = format!("{base}{colored}");
        }
    }

    // Anchor the prompt dock and footer to the bottom row. Without this pad the
    // editor sits directly under the header and drifts down with every message.
    let pad = available_height.saturating_sub(visible_transcript.len());
    visible_transcript.extend(vec![String::new(); pad]);

    let has_dismissable_modal = state
        .selector
        .as_ref()
        .is_some_and(|s| s.kind != SelectorKind::ToolApproval);

    let mut frame = Vec::with_capacity(height);
    let mut regions = Vec::new();

    for hl in header_lines {
        let row = frame.len();
        for (col_start, col_end, action) in hl.regions {
            regions.push(ClickRegion {
                row,
                col_start,
                col_end,
                action,
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
        for (col_start, col_end, action) in fl.regions {
            regions.push(ClickRegion {
                row,
                col_start,
                col_end,
                action,
            });
        }
        frame.push(fl.text);
    }

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

