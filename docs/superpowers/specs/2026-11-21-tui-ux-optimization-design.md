# TUI UX optimization design (2026-11-21)

## Intent

MEHH Code's TUI already has a solid foundation: truecolor tokenised theming (`Theme`),
token-split fuzzy search, paste collapsing, synchronized output (`\x1b[?2026h`), and a
diff renderer in `mehh-tui`. Judged against developer-tool / CLI standards (flat,
high-contrast, keyboard-first, clear information density, zero flicker), several
bottlenecks remain that directly reduce smoothness, readability, and visual
consistency. This design captures the optimization roadmap supplied by the human
partner and turns it into implementable work.

Source of truth: the partner's 2026-11-21 evaluation message, reproduced in
structured form below. Where this document guesses, it says so.

## Goals

- Eliminate full-screen redraws and per-frame recomputation during streaming so the
  TUI is zero-flicker and low-CPU.
- Make `Theme` tokens actually drive borders, selection, headers, footers, and markdown
  so `/theme` changes every component and contrast reaches WCAG AA.
- Anchor the viewport while reading history during streaming; make modals, autocomplete,
  and approval panels keyboard-first and layout-stable.
- Raise information density: compact metrics, semantic status icons, progressive
  disclosure for thinking blocks and tool calls.

## Non-goals

- No redesign of the agent loop, tool execution, or permission semantics.
- No new CLI flags or settings files beyond what the existing `/theme` and settings
  machinery already supports.
- No refactoring of `interactive.rs` into a module directory in this batch.
- No commit unless explicitly requested later.
- Do not touch unrelated uncommitted work from other sessions, specifically the
  permission-rule block at `crates/mehh-coding-agent/src/interactive.rs:3316-3352`.

## Constraints

- Rust workspace, edition 2021, crossterm 0.28, unicode-width 0.2.
- Debug checks only: `cargo test -p <crate> --lib` and `cargo check -p <crate> --tests`.
  No release builds on this laptop.
- TDD for every fix: failing test first, then minimal code, then a green run.
- Every color must come from `Theme`; no hardcoded SGR color codes in render paths.
- Width math must go through `ansi::visible_width` / `wrap_text_with_ansi` /
  `truncate_to_width` so wide characters, CJK, and existing SGR survive.
- Serialize new tokens in `Theme` with `#[serde(default)]` so existing JSON themes
  keep loading; update `apply_json_value` to resolve new token names.

## Priority matrix (as supplied)

| Priority | Item | Files | Core problem | UX impact |
| --- | --- | --- | --- | --- |
| P0 | Render & performance | `interactive.rs`, `tui.rs` | `render_now()` calls `renderer.reset()` and redraws the whole screen every 80ms; context tokens recomputed every frame | Anti-flicker on slow terminals, lower CPU while streaming |
| P0 | Contrast & theme wiring | `theme.rs`, `basic.rs`, `settings_list.rs` | `BorderedBox` ignores `theme.border`; `selected_bg` unused; `dim` contrast ~2.5:1 | Frames do not outshine text, clear focus, WCAG AA |
| P1 | Interaction & scroll | `interactive.rs`, `editor.rs` | History drifts during streaming; autocomplete trapped in editor box; no PageUp/Down or Space filter in modals | Smooth scrolling, filtering, typing without layout jumps |
| P1 | Visual hierarchy & layout | `interactive.rs`, `markdown.rs` | Header lacks permission mode; `user_bg` breaks on wrapped lines; tables mix ASCII `\|` with Unicode `─`; emoji `⚙` status icon | Cleaner hierarchy, consistent geometry |
| P2 | Progressive disclosure | `interactive.rs` | Thinking block too long or fully hidden; tool calls ignore `duration_ms` and do not report truncated diff lines | Less visual noise, faster scanning |

## Roadmap (as supplied)

### Phase 1 - Smooth and clear foundation (1-2h)

1. Real differential rendering in `render_now()`: switch `TuiRenderer` to absolute row
   positioning (`\x1b[{row};1H\x1b[2K{line}`) for changed lines inside the
   `\x1b[?2026h ... \x1b[?2026l` block; drop `renderer.reset()` except on
   `Event::Resize` or `/theme` change. Use the alternate screen buffer in
   `CrosstermTerminal::new()` / `Drop` and hide the hardware cursor.
   Rationale from partner: the hardware-cursor offset comment is wrong because
   `show_hardware_cursor = false` and the cursor is already hidden with `\x1b[?25l`.
2. Cache transcript lines and context tokens: generalise `MarkdownCache` into a
   `TranscriptRenderCache` keyed by `(item_idx, width, theme.name)`, invalidating only
   the last item on `TextDelta` / `ThinkingDelta` / `ToolExecutionUpdate`; store
   `cached_ctx_tokens` in `InteractiveState` and recompute only at context-changing
   milestones (`MessageEnd`, `/compact`, `/new`, `/tree`, `/resume`, `/import`).
3. Raise contrast tokens: `dark.dim` `rgb(82,82,91)` -> `rgb(142,142,152)`;
   `dark.muted` `rgb(113,113,122)` -> `rgb(161,161,170)`; `light.dim`
   `rgb(161,161,170)` -> `rgb(100,100,110)`; add `surface_elevated` dark `#1e2028` /
   light `#f4f4f5`.
4. Wire theme into `BorderedBox`: add `border_color` and `title_color` fields supplied
   from `theme.border` / `theme.border_accent` / `theme.warning` / `theme.error` and
   `theme.text` / `theme.accent`; focus-based hierarchy for prompt dock, modal
   selectors, and approval panels. Unify corner geometry (recommend `Single` for
   docks/code/tables, `Rounded` for floating overlays).
5. Activate `selected_bg` in `SelectList` and `SettingsList`: pad the selected row to
   full width, wrap with `theme.selected_bg`, left indicator `▎`/`▸` in `theme.accent`,
   bold label; split label and description into fixed columns like `SettingsList`.

### Phase 2 - Fix interaction and layout (2-3h)

6. Pin viewport during streaming reads: track `prev_transcript_len`; when
   `scroll_offset > 0` and length grows, add the delta to `scroll_offset`. Replace the
   pill text with `↑ Paused at 64% · +12 new lines · End to jump`. Draw a 1-column
   scrollbar on the transcript's right edge when content exceeds available height.
7. Lift autocomplete out of the editor's `BorderedBox`: render it as its own panel
   immediately above the fixed prompt dock. Cap `Editor` height with
   `max_lines = (height / 3).clamp(3, 8)` and show `Ln 6/14` on the dock title.
8. Selector / settings fixes: match `PageUp` / `PageDown` in the modal key loop
   (`SelectList` and `SettingsList` already implement `page_up()` / `page_down()`);
   allow `Space` in filter for all `SelectorKind` except `Settings` (where it toggles);
   always show the `/ Type to filter...` bar plus `(5/32)` counter; sync `QueueMode`
   values with `apply_setting_cycle()` (`"one-at-a-time"` and `"all"` only); cap
   `ToolApproval` command preview at `height / 3` and color actions semantically
   (`Allow` = `success`, `Deny` = `error`).
9. Fix wrapped `user_bg`: wrap text to `width.saturating_sub(4)` first, then paint each
   line with left accent bar `▌`, `theme.user_bg`, padding to `width`, and trailing
   `\x1b[0m`. Replace `⚙` with width-1 monospace status icons (`BRAILLE_FRAMES` while
   running, `✓` done, `✗` error, `◌` blocked).

### Phase 3 - Polish and information density (2-3h)

10. Redesign header/footer left-right: permission-mode badge with semantic colors
    (`auto` = `warning`, `approve` = `accent`, `chat` = `dim`), compact cwd
    (`~/.../folder`), 8-cell context mini bar changing color at 70% and 85%, compact
    token counts (`12.4k`).
11. Busy loader elapsed timer: store `turn_started_at: Option<Instant>` and render
    `⠋ Thinking... 3.4s · Esc to cancel`.
12. Upgrade `MarkdownView`: full Unicode table box-drawing, heading hierarchy
    (H1 bold + `md_heading` + underline, H2 bold + `md_heading`, H3+ bold + `text`),
    replace hardcoded `\x1b[32m[x]` / `\x1b[2m[ ]` with `theme.success` / `theme.dim`,
    lift inline code with `md_code` plus `surface_elevated` background.
13. Progressive disclosure: collapsible thinking block (3-line preview while
    streaming, `▸ Thought for 4.2s (18 lines · Ctrl+T to expand)` when done); store
    `duration_ms` from `AgentEvent::ToolExecutionEnd` on `TranscriptItem::ToolCall`,
    show `✓ read src/main.rs  18ms`, diff counts `+14 -6`, and `… (+N more lines)`
    when diff exceeds 24 lines.

## Verification checklist (as supplied)

- Contrast: `dim`, `muted`, `thinking_text`, `md_code` readable on dark `#121215` and
  light `#ffffff`.
- Zero flicker: high-speed streaming does not flash header/footer; `reset()` only on
  resize/theme change.
- Narrow viewport: clean render with no column overflow at `60x20` and `160x45`.
- Keyboard-first: all selectors support Up/Down, PageUp/PageDown, multi-token fuzzy
  search with spaces, Enter, Esc; shortcuts consistent with `/hotkeys`.
- Safety UI: `ToolApproval` always shows all four actions even with a very long
  command preview.
- No hardcoded ANSI: no `\x1b[32m` / `\x1b[2m` outside `Theme`, so `/theme` recolors
  100% of components.
