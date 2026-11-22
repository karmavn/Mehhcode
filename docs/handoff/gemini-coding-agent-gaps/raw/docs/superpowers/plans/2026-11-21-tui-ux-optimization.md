# TUI UX Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make MEHH Code's full-screen TUI zero-flicker, high-contrast, keyboard-first, and information-dense without changing agent semantics.

**Architecture:** Three phases over two crates. Phase 1 fixes the rendering foundation (`TuiRenderer` absolute positioning, removal of per-frame `reset()`, theme contrast and theme-driven borders/selection). Phase 2 fixes interaction (scroll anchoring, autocomplete panel, modal keys/filters, wrapped user turns, status icons). Phase 3 polishes hierarchy and disclosure (header/footer, markdown, thinking/tool call metadata). Every change is pinned by a pure unit test or a `MockTerminal` output assertion so no TTY is required.

**Tech Stack:** Rust workspace (edition 2021), crossterm 0.28, `mehh-tui`, `mehh-coding-agent`, `cargo test -p <crate> --lib`, `cargo check -p <crate> --tests`.

**Spec:** `docs/superpowers/specs/2026-11-21-tui-ux-optimization-design.md`

## Global Constraints

- Do not commit unless the user explicitly asks. When asked, stage only paths listed in the task.
- Do not touch the unrelated uncommitted permission-rule block at `crates/mehh-coding-agent/src/interactive.rs:3316-3352`. Verify with `git diff -- crates/mehh-coding-agent/src/interactive.rs` before the first edit to `interactive.rs`.
- Debug checks only: `cargo test -p mehh-tui --lib`, `cargo test -p mehh-coding-agent --lib`, `cargo check -p mehh-tui --tests`, `cargo check -p mehh-coding-agent --tests`. No release builds.
- TDD: write the failing test, watch it fail, implement minimal code, watch it pass, before moving to the next task.
- All colors come from `Theme`. No new hardcoded SGR color codes in render paths. Existing `\x1b[0m` resets and control sequences (`\x1b[H`, `\x1b[J`, `\x1b[?25l/h`, `\x1b[?2026h/l`) are allowed.
- Width math must use `mehh_tui::visible_width`, `wrap_text_with_ansi`, `truncate_to_width`, and `slice_by_column`. Never `str.len()` for display width.
- Preserve `CURSOR_MARKER = "\x1b_mehh:c\x07"` in every editor render path.
- New `Theme` fields must carry `#[serde(default)]` and be wired into `dark()`, `light()`, and `apply_json_value` (`theme.rs:239-337`).
- Key strings produced by `normalize_crossterm_key` are `pageup` / `pagedown` (no underscore). Config aliases `page_up` / `page_down` are handled by `matches_key` in `keys.rs:125-126`; use the canonical names in direct `match key_str.as_str()` arms.
- Keep signatures unchanged: `render_interactive_frame`, `run_interactive_mode`, `process_file_arguments`, `expand_prompt_with_files`. Internal helpers may be added freely.

## Review Focus

- 60x20 viewport with a long wrapped prompt and an open autocomplete: menu must sit above a fixed prompt dock and neither overflow nor push the footer off-screen; pinned by Task 7 `editor_max_lines_window_centers_on_cursor` and Task 5 `render_interactive_frame_is_deterministic_at_60x20`.
- Streaming while `scroll_offset > 0`: the line being read must stay on screen when new lines arrive; pinned by Task 6 `anchored_scroll_offset_holds_position_during_stream`.
- User JSON theme with no `surfaceElevated` key: must load via `#[serde(default)]` and still recolor new surfaces; pinned by Task 1 `custom_theme_without_surface_elevated_loads`.
- `ToolApproval` with a 300-line command preview: all four action rows must remain visible; pinned by Task 8 `cap_preview_lines_preserves_action_rows`.
- Multi-token filter typed as `anthropic sonnet`: space must enter the filter for `Model`/`Tree`/`Resume`/`Login` but toggle values in `Settings`; pinned by Task 8 `filter_accepts_space_except_for_settings`.

---

### Task 1: Theme contrast tokens and `surface_elevated`

**Files:**
- Modify: `crates/mehh-tui/src/theme.rs:6-39`, `:117-174`, `:239-337`
- Test: `cargo test -p mehh-tui --lib theme::tests`

**Interfaces:**
- Consumes: existing `resolve_color_value`, `hex_to_fg_ansi`, `hex_to_bg_ansi` (`theme.rs:48-115`).
- Produces: `Theme::surface_elevated: String` (bg SGR, `#[serde(default)]`); updated `dim` / `muted` values in both built-in themes. Later tasks read `theme.surface_elevated`.

- [ ] **Step 1: Write failing tests in `theme.rs::tests`**

Add `contrast_tokens_match_spec`:
```rust
assert_eq!(super::Theme::dark().dim, "\x1b[38;2;142;142;152m");
assert_eq!(super::Theme::dark().muted, "\x1b[38;2;161;161;170m");
assert_eq!(super::Theme::light().dim, "\x1b[38;2;100;100;110m");
assert_eq!(super::Theme::dark().surface_elevated, "\x1b[48;2;30;32;40m");
assert_eq!(super::Theme::light().surface_elevated, "\x1b[48;2;244;244;245m");
```
Add `custom_theme_without_surface_elevated_loads`:
```rust
let tmp = std::env::temp_dir().join(format!("mehh-theme-test-{}.json", std::process::id()));
std::fs::write(&tmp, r#"{"name":"custom","colors":{"dim":"#123456"}}"#).unwrap();
let t = super::Theme::load(Some(tmp.to_str().unwrap()));
assert_eq!(t.name, "custom");
assert_eq!(t.dim, "\x1b[38;2;18;52;86m");
assert_eq!(t.surface_elevated, super::Theme::dark().surface_elevated);
let _ = std::fs::remove_file(tmp);
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-tui --lib theme::tests`
Expected: FAIL — no field `surface_elevated`, wrong token values.

- [ ] **Step 3: Add the field and values**

Add `#[serde(default)] pub surface_elevated: String` next to `user_bg` in the struct. In `dark()` set `dim = fg(142,142,152)`, `muted = fg(161,161,170)`, `surface_elevated = bg(30,32,40)`; in `light()` set `dim = fg(100,100,110)`, `surface_elevated = bg(244,244,245)`. Leave `light.muted`, `thinking_text`, and all other tokens unchanged.

- [ ] **Step 4: Wire JSON loading**

In `apply_json_value`, after the `selectedBg` block, add:
```rust
if let Some(c) = get_color("surfaceElevated", "surface_elevated", true) {
    theme.surface_elevated = c;
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mehh-tui --lib theme::tests`
Expected: PASS.

---

### Task 2: Theme-driven `BorderedBox` borders and focus hierarchy

**Files:**
- Modify: `crates/mehh-tui/src/components/basic.rs:81-154`
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2205-2245`, `:2306-2310`
- Test: `cargo test -p mehh-tui --lib components::basic::tests`

**Interfaces:**
- Consumes: `Theme::fg` / `Theme::bg` (`theme.rs:340`, `:349`), `BorderStyle::chars()` (`basic.rs:69-79`).
- Produces: `BorderedBox::border_color: String`, `BorderedBox::title_color: String`, builders `with_border_color`, `with_title_color`. Empty string means unpainted (backward compatible). `pub struct RawLines { pub lines: Vec<String> }` implementing `Component` (returns `self.lines.clone()`), used in Task 7.

- [ ] **Step 1: Write failing tests in `basic.rs::tests`**

Add `bordered_box_uses_theme_colors_without_changing_width`:
```rust
let boxed = super::BorderedBox::new(Box::new(super::Text::new("hi")))
    .with_border_color("\x1b[38;2;1;2;3m".into())
    .with_title_color("\x1b[38;2;4;5;6m".into())
    .with_title("T".into());
let lines = boxed.render(20);
let joined = lines.join("\n");
assert!(joined.contains("\x1b[38;2;1;2;3m"));
assert!(joined.contains("\x1b[38;2;4;5;6m"));
assert!(lines.iter().all(|l| crate::ansi::visible_width(l) == 20));
```
Add `raw_lines_returns_input_verbatim` asserting `RawLines { lines: vec!["a".into(), "b".into()] }.render(10) == vec!["a", "b"]`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-tui --lib components::basic::tests`
Expected: FAIL — unknown fields / type.

- [ ] **Step 3: Implement the fields, builders, and painting**

Add the two `String` fields (default `String::new()` in `new`), plus `with_border_color` / `with_title_color`. Add a private `fn paint(code: &str, s: &str) -> String` in `basic.rs` returning `format!("{code}{s}\x1b[0m")` when `code` is non-empty, else `s.to_string()`. In `render`, wrap top, each child line's verticals, and bottom with `paint(&self.border_color, ...)`; wrap the title text with `paint(&self.title_color, title)` while keeping all width math on `title_clean` as today. Add `RawLines` with a `Component` impl returning cloned lines.

- [ ] **Step 4: Apply focus hierarchy at call sites in `interactive.rs`**

- Selector box (`2241-2244`): `BorderStyle::Rounded`, `border_color = border_accent` for `Model`/`Tree`/`Resume`/`Settings`/`Login`; `warning` (or `error` when `pending` is an error) for `ToolApproval`. `title_color = theme.text`.
- Command-preview panel (`2235-2237`): same warning/error border, `title_color = theme.warning`.
- Prompt dock idle (`2307`): `BorderStyle::Single`, `border_color = theme.border`, `title_color = theme.text`.
- Prompt dock while `state.is_busy` (steer mode): `border_color = theme.warning`, title `Steer (Enter) · Follow-up (Alt+Enter) · Cancel (Esc)`.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mehh-tui --lib components::basic::tests && cargo check -p mehh-coding-agent --tests`
Expected: PASS, no type errors from new call-site arguments.

---

### Task 3: Activate `selected_bg` and align list columns

**Files:**
- Modify: `crates/mehh-tui/src/components/basic.rs:321-369`
- Modify: `crates/mehh-tui/src/components/settings_list.rs:186-288`
- Test: `cargo test -p mehh-tui --lib components`

**Interfaces:**
- Consumes: `Theme::fg`, `Theme::bg`, `truncate_to_width(s, width, "", true)` for full-width padding.
- Produces: selected row layout for both lists: `▎` indicator in `theme.accent`, bold label, description in `theme.dim`, full-width `theme.selected_bg`. No signature changes.

- [ ] **Step 1: Write failing tests**

In `basic.rs::tests`, add `select_list_selected_row_uses_selected_bg_and_full_width`:
```rust
let mut list = super::SelectList::new(
    vec![super::SelectItem::new("a", "Alpha").with_description("first")], 5);
list.theme.selected_bg = "\x1b[48;2;9;9;9m".into();
let lines = crate::Component::render(&list, 30);
assert_eq!(lines.len(), 1);
assert!(lines[0].contains("\x1b[48;2;9;9;9m"));
assert!(lines[0].contains('▎'));
assert_eq!(crate::ansi::visible_width(&lines[0]), 30);
```
In `settings_list.rs::tests`, add the equivalent `settings_list_selected_row_uses_selected_bg_and_full_width` with one `SettingItem::new("x", "Label", "value").with_description("desc")`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-tui --lib components`
Expected: FAIL — no background code, no `▎`, width < 30.

- [ ] **Step 3: Implement `SelectList::render`**

Compute `max_label_width` over the *filtered* items (clamp `10..=36`) like `SettingsList`. For the selected row, build the plain text `▎ {label}{pad}  {description}`, pad to `width` with `truncate_to_width(&plain, width, "", true)`, then emit `selected_bg` first and re-emit it after every inner `\x1b[0m` so the background survives nested `fg` calls. Indicator uses `theme.accent`, label uses `\x1b[1m`, description uses `theme.dim`. Unselected rows keep the current two-space layout but align label to `max_label_width`.

- [ ] **Step 4: Implement `SettingsList::render`**

Keep the existing label/value two-column math. Replace the selected prefix `"> "` with `▎` in `theme.accent`; build a full-width plain row and wrap it in `theme.selected_bg` with the same re-emit rule. Preserve the description pane, `(n/total)` counter, and hint line exactly as today.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mehh-tui --lib components`
Expected: PASS, including the existing paging tests.

---

### Task 4: Real differential rendering and alternate screen

**Files:**
- Modify: `crates/mehh-tui/src/tui.rs:21-67`, `:172-329`, `:331-366`
- Modify: `crates/mehh-coding-agent/src/interactive.rs:3147-3166`, `:3213-3219`
- Test: `cargo test -p mehh-tui --lib tui::tests && cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `MockTerminal` (`tui.rs:69-108`), `TuiRenderer::render_frame(terminal, lines, show_hardware_cursor)`.
- Produces: `TuiRenderer` with absolute row positioning; `CrosstermTerminal` entering/leaving the alternate screen. `render_now` no longer homes or resets the renderer; Resize branch calls `renderer.reset()` explicitly.

- [ ] **Step 1: Write failing renderer tests in `tui.rs::tests`**

Add `differential_render_uses_absolute_rows`:
```rust
let mut t = super::MockTerminal::new(40, 3);
let mut r = super::TuiRenderer::new();
r.render_frame(&mut t, vec!["a".into(), "b".into(), "c".into()], false).unwrap();
t.clear_output();
r.render_frame(&mut t, vec!["a".into(), "B".into(), "c".into()], false).unwrap();
let out = t.full_output();
assert!(out.contains("\x1b[2;1H\x1b[2KB"), "absolute row positioning: {out:?}");
assert!(!out.contains("\x1b[1A"), "no relative up move: {out:?}");
assert!(!out.contains("\x1b[1B"), "no relative down move: {out:?}");
```
Add `shorter_frame_erases_stale_rows`:
```rust
let mut t = super::MockTerminal::new(40, 3);
let mut r = super::TuiRenderer::new();
r.render_frame(&mut t, vec!["a".into(), "b".into(), "c".into()], false).unwrap();
t.clear_output();
r.render_frame(&mut t, vec!["x".into()], false).unwrap();
assert!(t.full_output().contains("\x1b[2;1H\x1b[J"), "erase stale rows: {:?}", t.full_output());
```

- [ ] **Step 2: Write failing `render_now` test in `interactive.rs::tests`**

Reuse the existing fixture around line 4726 that already calls `render_now`. Add `render_now_keeps_renderer_state_and_does_not_home`:
```rust
let mut term = mehh_tui::MockTerminal::new(80, 24);
let mut renderer = mehh_tui::TuiRenderer::new();
render_now(&state, &editor, &session, &mut term, &mut renderer);
term.clear_output();
render_now(&state, &editor, &session, &mut term, &mut renderer);
let out = term.full_output();
assert!(!out.contains("\x1b[H"), "no home: {out:?}");
assert!(!out.contains("\x1b[J"), "no clear-below: {out:?}");
assert!(!renderer.previous_lines.is_empty(), "renderer keeps baseline");
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p mehh-tui --lib tui::tests && cargo test -p mehh-coding-agent --lib render_now_keeps`
Expected: FAIL.

- [ ] **Step 4: Rewrite the diff branch in `render_frame`**

Keep marker extraction, software cursor, and synchronized-output brackets unchanged. Replace lines 249-297 with: iterate `0..max_len`, collect changed row indices; for each changed `i` emit `\x1b[{i+1};1H\x1b[2K{line}` separated by nothing (the absolute move resets the column). If the new frame is shorter than `previous_lines`, emit `\x1b[{lines.len()+1};1H\x1b[J`. Set `final_cursor_row = lines.len().saturating_sub(1)`. Replace the hardware-cursor relative moves with `\x1b[{target_row+1};{target_col+1}H`. Remove the now-unused `cursor_row` field from `TuiRenderer`, `new()`, `reset()`, and `Default` (struct derives `Default`, so zero-initialisation is fine — delete the explicit initialiser). Keep `\x1b[?2026h` / `\x1b[?2026l`.

- [ ] **Step 5: Enter the alternate screen**

In `CrosstermTerminal::new`, after enabling raw mode, execute `crossterm::terminal::EnterAlternateScreen` and `crossterm::cursor::Hide`. In `Drop`, execute `crossterm::cursor::Show` and `crossterm::terminal::LeaveAlternateScreen` before disabling raw mode. Remove the now-redundant `\x1b[?25l` / `\x1b[?25h` writes in `interactive.rs:3178` and `:3707` (the trailing `\n` at 3708 stays).

- [ ] **Step 6: Simplify `render_now` and reset on resize**

Delete lines 3157-3159 and 3163-3165. `render_now` becomes: compute lines, call `renderer.render_frame(&mut *terminal, lines, false)`. In the Resize branch (`3217-3219`) call `renderer.reset()` immediately before `render_now`. Theme changes need no reset because absolute positioning rewrites every changed row; this is a deliberate simplification over the spec's literal "reset on `/theme`".

- [ ] **Step 7: Run to verify pass**

Run: `cargo test -p mehh-tui --lib tui::tests && cargo test -p mehh-coding-agent --lib interactive::tests && cargo check -p mehh-coding-agent --tests`
Expected: PASS, including the existing `test_differential_render_cursor_position_when_partial_lines_change` (update its assertions to absolute rows if it asserts old relative sequences).

---

### Task 5: Cache context tokens and all transcript items

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:90`, `:144-197`, `:1791-1809`, `:2163-2194`, `:2313-2365`
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `crate::compaction::estimate_context_tokens(&[Message]) -> usize`, `format!("{item:?}")` from `#[derive(Debug)] TranscriptItem`.
- Produces: `InteractiveState.cached_ctx_tokens: Mutex<Option<u64>>` with `fn invalidate_context_tokens(&self)`; `TranscriptCache` keyed `(usize, usize)` with value `(String, String, Vec<String>)` = `(theme name, fingerprint, rendered lines)`. `fn transcript_fingerprint(item: &TranscriptItem) -> String`.

- [ ] **Step 1: Write failing tests**

Add `context_token_cache_invalidates_on_message_end`:
```rust
*state.cached_ctx_tokens.lock().unwrap() = Some(4242);
state.invalidate_context_tokens();
assert!(state.cached_ctx_tokens.lock().unwrap().is_none());
```
Add `transcript_fingerprint_tracks_item_content`:
```rust
let a = super::TranscriptItem::User("one".into());
let b = super::TranscriptItem::User("two".into());
assert_ne!(super::transcript_fingerprint(&a), super::transcript_fingerprint(&b));
assert_eq!(super::transcript_fingerprint(&a), super::transcript_fingerprint(&a));
```
Add `render_interactive_frame_is_deterministic_at_60x20` using the existing fixture: render twice at `(60, 20)` with unchanged state and assert equal `Vec<String>`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL — missing symbols.

- [ ] **Step 3: Add the caches**

Rename the alias at line 90 to `type TranscriptCache = HashMap<(usize, usize), (String, String, Vec<String>)>;` and rename the field to `transcript_cache` (keep it private). Add `pub(crate) cached_ctx_tokens: Mutex<Option<u64>>` initialised to `Mutex::new(None)`, and `pub(crate) fn invalidate_context_tokens(&self) { *self.cached_ctx_tokens.lock().unwrap() = None; }` (use the existing poison-tolerant pattern if preferred). Add `transcript_fingerprint`.

- [ ] **Step 4: Use the token cache in the footer**

Replace lines 2168-2170 with a lock/check/compute/store block: if `Some(n)` use `n`; otherwise compute `estimate_context_tokens(&session.session().build_context_messages()) as u64`, store, and use it.

- [ ] **Step 5: Invalidate at the milestones**

Call `state.invalidate_context_tokens()` in: `AgentEvent::MessageEnd` (1817), the `/compact`, `/new`, `/tree`, `/resume`, `/import` slash-command success paths (locate via grep for those command names), and the `/theme` success path (1096-1130, which also changes cache keys). Do not invalidate on `TextDelta` / `ThinkingDelta` / `ToolExecutionUpdate`.

- [ ] **Step 6: Generalise the transcript cache**

In the loop at 2323, compute `let fp = transcript_fingerprint(item);` and use `(theme.name.clone(), fp)` as the cache validation pair instead of `(theme.name.clone(), source text)`. Apply it to every `TranscriptItem` variant: on hit, `transcript_lines.extend(cached_lines.iter().cloned())` and `continue`; on miss, render that item's lines (existing match-arm bodies), then insert. Keep the `len > items.len()*2 + 16` clear rule.

- [ ] **Step 7: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS.

---

### Task 6: Pin the viewport during streaming and draw a scrollbar

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:144-197`, `:2266-2277`, `:2473-2490`
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `transcript_lines.len()`, `available_height`, `state.last_max_scroll` (`AtomicUsize`).
- Produces: `InteractiveState.prev_transcript_len: AtomicUsize`; pure helpers `anchored_scroll_offset(prev_len, new_len, offset) -> usize`, `scrollbar_char(row, visible, total, offset) -> char`, `scroll_pill_text(offset, max_scroll, new_lines) -> String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn anchored_scroll_offset_holds_position_during_stream() {
    assert_eq!(super::anchored_scroll_offset(100, 112, 5), 17);
    assert_eq!(super::anchored_scroll_offset(100, 90, 5), 5);
    assert_eq!(super::anchored_scroll_offset(100, 112, 0), 0);
}
#[test] fn scrollbar_char_marks_thumb_and_track() {
    assert_eq!(super::scrollbar_char(1, 10, 100, 0), '┃');
    assert_eq!(super::scrollbar_char(5, 10, 100, 0), '│');
}
#[test] fn scroll_pill_reports_pct_and_new_lines() {
    assert!(super::scroll_pill_text(32, 100, 12).contains("+12 new lines"));
    assert!(super::scroll_pill_text(32, 100, 12).contains("32%"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL — missing helpers.

- [ ] **Step 3: Add field and helpers**

Add `pub(crate) prev_transcript_len: AtomicUsize` (init 0) to `InteractiveState`. Implement the three helpers as pure functions: `anchored_scroll_offset` adds `new_len - prev_len` only when `offset > 0 && new_len > prev_len`; `scrollbar_char` maps the viewport fraction `offset/max_scroll` to a 1-row thumb inside `visible` rows and returns `│` elsewhere; `scroll_pill_text` formats `↑ Paused at {pct}% · +{new_lines} new lines · End to jump`.

- [ ] **Step 4: Use the anchor and draw the scrollbar**

At 2473-2490, read `prev = state.prev_transcript_len.load(Relaxed)`, compute `let offset = anchored_scroll_offset(prev, transcript_lines.len(), state.scroll_offset).min(max_scroll)`, use it for `end`, and store the new len into `prev_transcript_len` after slicing. If `transcript_lines.len() > available_height`, append the scrollbar to each visible row: pad/truncate content to `width-1` via `truncate_to_width(&line, width-1, "...", false)`, then push `scrollbar_char(row, visible_transcript.len(), transcript_lines.len(), offset)`. Use `theme.accent` for `┃` and `theme.border_muted` for `│` via `theme.fg`.

- [ ] **Step 5: Replace the pill text**

At 2266-2277 call `scroll_pill_text(...)` instead of the current `-{scroll_offset}` message. Keep it in `theme.warning`.

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS, including Task 5's 60x20 determinism test.

---

### Task 7: Lift autocomplete above the prompt dock and cap editor height

**Files:**
- Modify: `crates/mehh-tui/src/components/editor.rs:301-320`, `:1081-1128`
- Modify: `crates/mehh-tui/src/components/basic.rs` (`RawLines` from Task 2)
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2298-2311`
- Test: `cargo test -p mehh-tui --lib components::editor::tests && cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `RawLines` (`basic.rs`, Task 2), `BorderedBox::with_border_color` (Task 2).
- Produces: `Editor::render_autocomplete(&self, width: usize) -> Vec<String>`; `Editor::max_lines: usize` (default `usize::MAX`) and `with_max_lines(usize)`; `Editor::visible_window(&self, width: usize, max_lines: usize) -> (usize, usize)`. `Editor::render` no longer emits autocomplete lines.

- [ ] **Step 1: Write failing tests in `editor.rs::tests`**

```rust
#[test] fn editor_render_excludes_autocomplete() {
    let mut e = super::Editor::new();
    e.set_autocomplete(vec![super::SelectItem::new("x", "extra")]);
    let lines = crate::Component::render(&e, 40);
    assert!(lines.iter().all(|l| !l.contains("extra")), "{lines:?}");
    assert!(e.render_autocomplete(40).iter().any(|l| l.contains("extra")));
}
#[test] fn editor_max_lines_window_centers_on_cursor() {
    let mut e = super::Editor::new();
    e.lines = (0..14).map(|i| format!("line {i}")).collect();
    e.cursor_row = 6;
    let (start, end) = e.visible_window(40, 4);
    assert!(end - start <= 4);
    assert!(start <= 6 && 6 < end);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-tui --lib components::editor::tests`
Expected: FAIL — missing methods, autocomplete still inline.

- [ ] **Step 3: Implement the editor changes**

Remove lines 1111-1116 from `Component::render`. Add `render_autocomplete(width)` returning `self.autocomplete.as_ref().map(|ac| ac.render(width)).unwrap_or_default()`. Add `max_lines` field + builder. Implement `visible_window` by wrapping each logical line with `wrap_text_with_ansi(&line, width)`, accumulating counts until the segment containing the marker/cursor row, then returning a window of at most `max_lines` rows centred on it. In `render`, if `result.len() > max_lines`, emit only `result[start..end]`. Keep `CURSOR_MARKER` inside the emitted window; if it falls outside, clamp the window so the cursor row is always included.

- [ ] **Step 4: Render the autocomplete panel above the dock**

In `interactive.rs:2298-2311`, after computing `themed_editor`, set `let max_lines = (height / 3).clamp(3, 8); themed_editor.max_lines = max_lines;`. When `state.selector.is_none()`: compute `ac_lines = themed_editor.render_autocomplete(width.saturating_sub(2))`; if non-empty, push a `BorderedBox::new(Box::new(RawLines { lines: ac_lines })).with_title("Suggestions").with_style(BorderStyle::Single).with_border_color(theme.border_accent.into()).with_title_color(theme.text.into()).render(width)`. Then render the prompt dock with `.with_title(format!("Prompt · Ln {}/{}", themed_editor.cursor_row + 1, themed_editor.lines.len()))` when `lines.len() > 1`. When a selector is open, render only `themed_editor.render(width)` (no autocomplete panel) as today.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p mehh-tui --lib components::editor::tests && cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS.

---

### Task 8: Selector keyboard fixes, search bar, queue values, approval preview cap

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:607-622`, `:686-705`, `:2196-2245`, `:3267-3356`, `:3358-3496`
- Modify: `crates/mehh-tui/src/components/basic.rs:198-219`, `:321-369` (`SelectItem::label_color`)
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests && cargo test -p mehh-tui --lib components::basic::tests`

**Interfaces:**
- Consumes: `SelectList::page_up()` / `page_down()` (`basic.rs:293-309`), `SettingsList::page_up()` / `page_down()` (`settings_list.rs:122-138`).
- Produces: `SelectItem::label_color: Option<String>` with `#[serde(default)]`; pure helpers `cap_preview_lines(preview: &str, max_lines: usize) -> String`, `search_bar_text(filter: &str, shown: usize, total: usize, theme: &Theme) -> String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn filter_accepts_space_except_for_settings() {
    let mut model = super::Selector { kind: super::SelectorKind::Model,
        list: mehh_tui::SelectList::new(vec![], 5), settings_list: None };
    assert!(super::update_selector_filter(&mut model, " "));
    assert_eq!(model.list.filter, " ");
    let mut settings = super::Selector { kind: super::SelectorKind::Settings,
        list: mehh_tui::SelectList::new(vec![], 5), settings_list: None };
    assert!(!super::update_selector_filter(&mut settings, " "));
}
#[test] fn queue_mode_values_match_enum() {
    let s = super::build_settings_list(&Default::default());
    let item = s.items.iter().find(|i| i.id == "steering_mode").unwrap();
    assert_eq!(item.values, vec!["one-at-a-time".to_string(), "all".to_string()]);
}
#[test] fn cap_preview_lines_preserves_action_rows() {
    let preview = (0..300).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let capped = super::cap_preview_lines(&preview, 6);
    assert!(capped.lines().count() <= 6);
    assert!(capped.contains("+294 more lines"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL.

- [ ] **Step 3: Add `label_color` and use it in `SelectList::render`**

Add `#[serde(default)] pub label_color: Option<String>` to `SelectItem`. In `SelectList::render`, choose `item.label_color.as_deref().unwrap_or(&theme.text)` for the label instead of `theme.text`. Default `None` preserves all current behavior.

- [ ] **Step 4: Fix PageUp/PageDown in both selector branches**

In the ToolApproval match (`3271-3312`) and the general selector match (`3365-3496`), add arms:
```rust
"pageup" => { sel.list.page_up(); if let Some(sl) = sel.settings_list.as_mut() { sl.page_up(); } }
"pagedown" => { sel.list.page_down(); if let Some(sl) = sel.settings_list.as_mut() { sl.page_down(); } }
```
Keep `up` / `down` unchanged. The canonical strings from `normalize_crossterm_key` are `pageup` / `pagedown` (keys.rs:63-64).

- [ ] **Step 5: Allow Space in filters except Settings**

In `update_selector_filter`, insert before the existing single-char branch:
```rust
} else if key == " " && selector.kind != SelectorKind::Settings {
    format!("{}{}", selector.list.filter, " ")
```
Keep backspace and the non-whitespace branch. Sync `settings_list.filter` as today.

- [ ] **Step 6: Always render the search row**

Replace `if !selector.list.filter.is_empty() { ... Search: ... }` at 2216-2223 with an unconditional call to `search_bar_text(&selector.list.filter, selector.list.filtered_indices().len(), selector.list.items.len(), theme)`. With an empty filter the text is `/ Type to filter... (n/total)` in `theme.dim`; with a filter it is `Search: {filter} (n/total)` in `theme.accent`.

- [ ] **Step 7: Sync QueueMode values and ToolApproval colors**

At 607-611 and 618-622 replace the three values with `vec!["one-at-a-time".into(), "all".into()]`. At 2543-2552 set `label_color` to `Some(theme.success.clone())` for the three allow items and `Some(theme.error.clone())` for deny (use the `state.theme` available there, or clone from a local `Theme::default()` if no state is in scope — prefer the state theme).

- [ ] **Step 8: Cap the approval preview**

At 2224-2239, before building `detail`, replace `pending.preview` with `Some(cap_preview_lines(preview, (height / 3).max(4)))`. Implement `cap_preview_lines` to keep the first `max_lines` lines and append `… (+N more lines)` when truncated. The four action rows come from `SelectList::new(items, 6)` and are rendered after the panel, so the cap protects them.

- [ ] **Step 9: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests && cargo test -p mehh-tui --lib components::basic::tests`
Expected: PASS.

---

### Task 9: Wrap-safe user turns and monospaced status icons

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2329-2344`, `:2375-2381`, `:2428-2462`
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `mehh_tui::BRAILLE_FRAMES`, `theme.user_bg`, `theme.user_prefix`, `theme.accent`.
- Produces: pure helpers `render_user_turn(text: &str, width: usize, theme: &Theme) -> Vec<String>` and `status_icon(status: &str, is_error: bool, frame: usize, theme: &Theme) -> String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn user_turn_paints_every_wrapped_line_full_width() {
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    let lines = super::render_user_turn(text, 20, &mehh_tui::Theme::dark());
    assert!(lines.len() > 1, "must wrap: {lines:?}");
    assert!(lines.iter().all(|l| mehh_tui::visible_width(l) == 20), "{lines:?}");
    assert!(lines.iter().all(|l| l.contains("\x1b[48;2;35;38;46m")));
}
#[test] fn status_icon_uses_monospace_glyphs() {
    let t = mehh_tui::Theme::dark();
    assert!(super::status_icon("running", false, 0, &t).contains('⠋'));
    assert!(super::status_icon("completed", false, 0, &t).contains('✓'));
    assert!(super::status_icon("failed", true, 0, &t).contains('✗'));
    assert!(super::status_icon("blocked", false, 0, &t).contains('◌'));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL.

- [ ] **Step 3: Implement `render_user_turn`**

Wrap `text` to `width.saturating_sub(4)` first with `wrap_text_with_ansi`. For each wrapped line emit `▌` in `theme.user_prefix`, then `theme.user_bg` + the body padded with spaces to fill `width - 1`, then `\x1b[0m` at the end of every line. Assert width with `visible_width` in the test, not `len()`.

- [ ] **Step 4: Replace the inline User arm**

At 2329-2344 call `render_user_turn(text, width, theme)` and extend `transcript_lines` with the result. Remove the old `pad` / `marker` computation.

- [ ] **Step 5: Implement and use `status_icon`**

`status_icon` maps `is_error || status == "failed"` to `theme.fg(theme.tool_error, "✗")`, `"completed"` to `theme.fg(theme.tool_success, "✓")`, `"blocked"` to `theme.fg(theme.tool_pending, "◌")`, and everything else to `theme.fg(theme.tool_pending, BRAILLE_FRAMES[frame % len])`. Replace the three `⚙` sites at 2380, 2434, 2449 (and keep the existing `✗` / `✓` branches by routing through the helper).

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS.

---

### Task 10: Left-right header/footer, permission badge, context bar, elapsed timer

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:144-197`, `:2115-2194`, `:2247-2264`
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `session.settings.permission_mode: Option<PermissionMode>` (`tool_inspection.rs:26-37`), `queue_mode_label` / `thinking_level_label` helpers in `interactive.rs`.
- Produces: pure helpers `permission_badge(mode: Option<PermissionMode>, theme: &Theme) -> String`, `compact_path(path: &Path, max_len: usize) -> String`, `left_right(left: String, right: String, width: usize) -> String`, `ctx_bar(pct: u64, theme: &Theme) -> String`, `compact_count(n: u64) -> String`, `elapsed_label(started: Option<Instant>, now: Instant) -> String`. `InteractiveState.turn_started_at: Mutex<Option<Instant>>` and `turn_completed_at: Mutex<Option<Instant>>`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn context_formatting_helpers() {
    assert_eq!(super::compact_count(12_432), "12.4k");
    assert_eq!(super::compact_count(999), "999");
    assert!(super::ctx_bar(72, &mehh_tui::Theme::dark()).matches('█').count() >= 5);
}
#[test] fn left_right_layout_aligns_right_edge() {
    let line = super::left_right("left".into(), "right".into(), 40);
    assert_eq!(mehh_tui::visible_width(&line), 40);
    assert!(line.ends_with("right"));
}
#[test] fn compact_path_keeps_final_folder() {
    let p = std::path::Path::new("C:/Users/Admin/Projects/mehh-code");
    let s = super::compact_path(p, 24);
    assert!(s.contains("mehh-code"), "{s}");
    assert!(s.contains("..."), "{s}");
}
#[test] fn elapsed_label_formats_seconds() {
    let now = std::time::Instant::now();
    let s = super::elapsed_label(Some(now - std::time::Duration::from_millis(3400)), now);
    assert!(s.contains("3.4s"), "{s}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL.

- [ ] **Step 3: Add helper implementations**

`permission_badge`: `Auto` -> `theme.warning`, `SmartApprove` -> `theme.accent`, `Approve` -> `theme.accent`, `Chat` -> `theme.dim`; text `[auto]`, `[smart]`, `[approve]`, `[chat]`, default `[smart]`. `ctx_bar`: 8 cells, filled = `pct * 8 / 100`, `█` coloured by the existing 70/85 thresholds, empty `░` in `theme.border_muted`. `left_right`: pad the left part with spaces to `width - visible_width(right)`, then append right; truncate the left with `truncate_to_width(..., width.saturating_sub(right_w), "...", false)` when needed. `compact_path`: keep last two components prefixed with `~/.../` when the full path exceeds `max_len`. `elapsed_label`: `format!("{:.1}s", secs)` or `""` when no start.

- [ ] **Step 4: Rebuild the header**

At 2115-2161, for the non-empty session use `left_right(format!("mehh | {} {}", model.name, permission_badge(session.settings.permission_mode, theme)), format!("{} · {}", compact_path(&session.cwd, 24), short_id), width)`, then keep the separator line. For the empty-session welcome card keep the existing welcome layout but append `permission_badge` to the model line.

- [ ] **Step 5: Rebuild the footer**

At 2163-2194 keep the computed `ctx_pct` and `ctx_color`, but format as `left_right(format!("thinking: {} · ctx {} {}", thinking_str, ctx_bar(ctx_pct, theme), ...), format!("{} in / {} out · ${:.4} · /help", compact_count(state.total_input_tokens), compact_count(state.total_output_tokens), state.total_cost), width)`.

- [ ] **Step 6: Add the elapsed timer to the busy loader**

Add the two `Mutex<Option<Instant>>` fields (init `None`). Set `turn_started_at` when `turn_running` becomes true (3206 and the `RetryStarted` branch at 3687); set `turn_completed_at` and clear `turn_started_at` on `TurnEnded` (3683-3685). At 2247-2264 change the loader line to `⠋ {label} {elapsed} · Esc to cancel` where `elapsed = elapsed_label(start, now_or_completed)`.

- [ ] **Step 7: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS, including Task 5's determinism test (now with live `Instant`s the frame changes between renders — pin the test by setting both instants to fixed values before rendering, or exclude the timer line from that assertion).

---

### Task 11: Markdown Unicode tables, heading hierarchy, themed task markers

**Files:**
- Modify: `crates/mehh-tui/src/components/markdown.rs:56-125`, `:200-224`, `:384-390`, `:441-452`
- Test: `cargo test -p mehh-tui --lib components::markdown::tests`

**Interfaces:**
- Consumes: `theme.success`, `theme.dim`, `theme.md_heading`, `theme.text`, `theme.surface_elevated`, `theme.border_muted`.
- Produces: no new public API; `format_markdown_table` output changes from ASCII pipes to Unicode box drawing.

- [ ] **Step 1: Write failing tests in `markdown.rs::tests`**

```rust
#[test] fn table_uses_unicode_box_drawing() {
    let md = "| A | B |\n|---|---|\n| 1 | 2 |";
    let lines = super::MarkdownView::new(md).with_theme(mehh_tui::Theme::dark()).render(40);
    let joined = lines.join("\n");
    assert!(joined.contains('┌') && joined.contains('┬') && joined.contains('┐'));
    assert!(joined.contains('├') && joined.contains('┼') && joined.contains('┤'));
    assert!(joined.contains('└') && joined.contains('┴') && joined.contains('┘'));
    assert!(!joined.contains('|'), "{joined}");
}
#[test] fn task_markers_come_from_theme() {
    let t = mehh_tui::Theme::dark();
    let lines = super::MarkdownView::new("- [x] done\n- [ ] todo").with_theme(t.clone()).render(40);
    let joined = lines.join("\n");
    assert!(joined.contains(&t.success), "{joined}");
    assert!(!joined.contains("\x1b[32m"), "{joined}");
}
#[test] fn headings_have_distinct_hierarchy() {
    let lines = super::MarkdownView::new("# One\n\n### Three").with_theme(mehh_tui::Theme::dark()).render(40);
    let h1 = lines.iter().find(|l| l.contains("One")).unwrap();
    let h3 = lines.iter().find(|l| l.contains("Three")).unwrap();
    assert_ne!(h1, h3);
    assert!(h1.contains('─'));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-tui --lib components::markdown::tests`
Expected: FAIL.

- [ ] **Step 3: Rewrite `format_markdown_table`**

Emit top `┌─┬─┐`, header separator `├─┼─┤`, body `│ cell │ cell │`, bottom `└─┴─┘`, with `─` repeated to each column width and `┬`/`┼`/`┴` at column junctions. Keep the existing width-shrinking loop (70-94) and `border_color`. Use `theme.success` / `theme.dim` nowhere here; keep header cells bold as today.

- [ ] **Step 4: Differentiate headings**

At 200-224 choose a style per level: H1 = `theme.md_heading` plus a following underline row of `─` in `theme.border_muted`; H2 = `theme.md_heading`; H3+ = `\x1b[1m` + `theme.text`. Keep the `#` prefixes and the blank line after each heading. Remember to close with `\x1b[0m` and re-apply the selected style after emphasis/strong/strikethrough resets.

- [ ] **Step 5: Theme the task markers and inline code**

At 384-390 use `theme.success` for `[x]` and `theme.dim` for `[ ]`. At 441-452 wrap inline code with `theme.surface_elevated` + `theme.md_code` (and keep the heading-style restoration). Remove the `\x1b[33m` fallback for code; keep the `md_code` theme fallback only if the field is empty.

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p mehh-tui --lib components::markdown::tests`
Expected: PASS.

---

### Task 12: Collapsible thinking and tool-call duration/diff metadata

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:43-69`, `:144-197`, `:2029-2099`, `:2345-2350`, `:2366-2421`
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `AgentEvent::ToolExecutionEnd { duration_ms: Option<u64>, .. }` (`mehh-agent/types.rs:307-308`), `state.turn_started_at` / `turn_completed_at` (Task 10).
- Produces: `TranscriptItem::ToolCall { duration_ms: Option<u64>, .. }`; `InteractiveState.thinking_expanded: bool`; pure helpers `tool_duration_label(d: Option<u64>) -> String`, `diff_visible_lines(diff: &str) -> (Vec<&str>, usize)`, `thinking_preview(thinking: &str, expanded: bool, busy: bool, elapsed_ms: Option<u64>) -> String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn tool_duration_label_formats_ms() {
    assert_eq!(super::tool_duration_label(None), "");
    assert_eq!(super::tool_duration_label(Some(18)), " 18ms");
}
#[test] fn diff_truncation_reports_hidden_lines() {
    let diff = (0..30).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n");
    let (shown, hidden) = super::diff_visible_lines(&diff);
    assert_eq!(shown.len(), 24);
    assert_eq!(hidden, 6);
}
#[test] fn thinking_preview_shows_last_three_lines_when_busy() {
    let text = (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let s = super::thinking_preview(&text, false, true, None);
    assert!(s.contains("line 8") && s.contains("line 6"));
    assert!(!s.contains("line 1"));
    assert!(s.contains("Thinking (8 lines)"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: FAIL.

- [ ] **Step 3: Add metadata fields and helpers**

Add `duration_ms: Option<u64>` to `TranscriptItem::ToolCall` and `pub thinking_expanded: bool` (init false) to `InteractiveState`. Implement the three pure helpers. `thinking_preview` when busy and not expanded returns the last 3 lines prefixed with `⠋ Thinking (N lines)...`; when not busy and not expanded returns `▸ Thought for {secs}s (N lines · Ctrl+T to expand)` using `elapsed_ms`; when expanded returns the full text.

- [ ] **Step 4: Record duration and update all constructors**

At 2029-2035 add `duration_ms` to the `ToolExecutionEnd` destructuring (type `Option<u64>`, copy it). In the matching `ToolCall` arm at 2060-2067 add `duration_ms: dur` to the pattern and assign `*dur = duration_ms` when `Some`. Grep every `TranscriptItem::ToolCall {` construction in the file and add `duration_ms: None` (the `..` in match patterns needs no change).

- [ ] **Step 5: Render duration and diff overflow**

At 2366-2421, append `tool_duration_label(*duration_ms)` to the tool summary line. Replace the `.take(24)` loop with `diff_visible_lines(diff)`; after the shown lines, if `hidden > 0`, push `    … (+{hidden} more lines)` in `theme.dim`.

- [ ] **Step 6: Render collapsible thinking**

At 2345-2350, when `!thinking.is_empty() && !hide_thinking`, compute `elapsed_ms` from `turn_started_at` / `turn_completed_at` and call `thinking_preview(thinking, state.thinking_expanded, state.is_busy, elapsed_ms)`, then wrap with `wrap_text_with_ansi` in `theme.thinking_text`. In the key loop, add a `ctrl+t` arm that toggles `state.thinking_expanded` and calls `render_now` (use `keybindings.matches` or a direct `key_str == "ctrl+t"` check — direct match is acceptable here because no existing action owns `ctrl+t`).

- [ ] **Step 7: Run to verify pass**

Run: `cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS, including the existing transcript tests (update fixtures that construct `TranscriptItem::ToolCall` literally).

---

## Final verification (after all 12 tasks)

- [ ] **Run both crate suites and checks**

Run: `cargo test -p mehh-tui --lib && cargo test -p mehh-coding-agent --lib && cargo check -p mehh-tui --tests && cargo check -p mehh-coding-agent --tests`
Expected: 0 failures, clean checks.

- [ ] **Confirm no hardcoded SGR colors remain in render paths**

Run: `git grep -n '\\x1b\[3[0-9]m\\\|\\x1b\[9[0-9]m' -- crates/mehh-tui/src crates/mehh-coding-agent/src/interactive.rs`
Expected: no matches in production code (tests may keep literal theme values).

- [ ] **Confirm no stray full-redraw sequences**

Run: `git grep -n 'renderer.reset()\|\\x1b\[H\|\\x1b\[J' -- crates/mehh-coding-agent/src/interactive.rs`
Expected: only the Resize-branch `renderer.reset()` and the `tui.rs` internal erasure `\x1b[J`.

- [ ] **Confirm no accidental commit**

Run: `git status --short`
Expected: only the task files above are modified; no commit was created unless explicitly requested.
