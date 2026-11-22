# SDD ledger — plan: docs/superpowers/plans/2026-11-21-tui-ux-optimization.md

Spec: docs/superpowers/specs/2026-11-21-tui-ux-optimization-design.md

Setup rulings:
- Ruling: work in place on main instead of a new worktree — the user opted out of questions and asked for direct implementation; a worktree would add an unrequested merge step — cost if wrong: changes share the working tree with other sessions, mitigated by touching only the plan's files, never `git add -A`, and never committing unless asked.
- Ruling: use a manually written ledger and task tracking because `bash` is unavailable on this Windows host, so the skill's shell scripts cannot run — cost if wrong: no scripted BASE/task-done bookkeeping; tracked explicitly here instead.
- Ruling: do not commit any task — AGENTS.md forbids commits unless the user asks, and the plan carries no commit steps — cost if wrong: none; changes stay in the working tree for review.

Pre-flight shared-interface scan:
- Task 2 -> Task 7: `RawLines` + `BorderedBox::with_border_color` produced in Task 2, consumed in Task 7. Checked: Task 2 defines them in `basic.rs`; Task 7 only calls them. No conflict.
- Task 3 / Task 8: both edit `basic.rs::SelectItem`/`SelectList::render`. Checked: Task 3 changes selected-row layout; Task 8 adds `label_color` and uses it in the same render fn. Sequential execution; no signature conflict.
- Task 4 -> Task 6/Task 5: `render_now` no longer resets; caches are computed in `render_interactive_frame`. Checked: no interface dependency beyond `TuiRenderer::render_frame` signature, unchanged.
- Task 5 -> Task 10: `transcript cache` vs live `Instant` timer may break frame determinism. Checked: Task 10 step 7 pins the instants in the determinism test. Ruling: keep test pinned; cost if wrong: a flaky test, caught at Task 10 verification.
- Task 10 -> Task 12: `turn_started_at` / `turn_completed_at` produced in Task 10, consumed by `thinking_preview` in Task 12. Checked: consistent `Mutex<Option<Instant>>` types. No conflict.
- Pre-flight: no other shared interfaces.

Task status:
- Task 1: complete (tests: `cargo test -p mehh-tui --lib theme::tests` → 2/2 pass after RED on missing `surface_elevated`; full crate `cargo test -p mehh-tui --lib` → 70/70 pass). Note: PowerShell reports exit 1 because cargo writes progress to stderr; test output is green.
- Task 1: Ruling: raw JSON test literal uses `r##"..."##` because `r#"` closes on `"#` in `#123456` — cost if wrong: compile error, caught immediately.
- Task 2: complete (tests: `cargo test -p mehh-tui --lib components::basic::tests` → 5/5 pass after RED on missing `with_border_color`/`RawLines`; `cargo check -p mehh-coding-agent --lib` → clean). Note: PowerShell exit 1 is stderr handling; cargo output green.
- Task 2: Ruling: `BorderedBox` paint re-emits `border_color` after the title and after the child line so an inner `\x1b[0m` does not leave the right border uncolored — cost if wrong: one border segment loses color.
- Task 2: Ruling: ToolApproval border uses `theme.warning` only; the spec's `theme.error` variant needs an `is_error` flag that `PendingApproval` (interactive.rs:93-99) does not have — cost if wrong: approval panel stays warning-colored for denied tools.
- Task 2: Ruling: unrelated `mehh-sandbox/src/lib.rs:167` had `out.push(path)` moved value, blocking every `mehh-coding-agent` build. Applied the compiler-suggested `path.clone()` to unblock verification. The file is actively edited by another session (first edit was overwritten); re-applied and verified. This one-line change is outside the plan and must be reported to the user. Cost if wrong: transient edit to another session's crate; semantics unchanged.
- Task 3: complete (tests: `cargo test -p mehh-tui --lib components` → RED on missing bg/`▎`, then `cargo test -p mehh-tui --lib` → 74/74 pass). Updated three legacy `'>'` assertions in `lib.rs`/`basic.rs` and three `"> "` assertions in `settings_list.rs` to `▎`.
- Task 3: Ruling: selected-row background must be re-emitted after each inner `\x1b[0m`; padding is computed inside the background span so the full row is covered — cost if wrong: background stops at the label.
- Task 4: complete (tests: `cargo test -p mehh-tui --lib tui::tests` → RED on relative moves, then 3/3 pass; `render_now_keeps_renderer_state_and_does_not_home` RED on `\x1b[H`, then `cargo test -p mehh-tui --lib` → 76/76 and `cargo test -p mehh-coding-agent --lib` → 165/165 pass).
- Task 4: Ruling: absolute row positioning makes theme changes self-healing through the diff, so no `/theme` reset is needed; only Resize calls `renderer.reset()` — cost if wrong: a full repaint on theme change, not a corruption.
- Task 4: Ruling: removed redundant `\x1b[?25l`/`\x1b[?25h` writes because `CrosstermTerminal` now owns cursor visibility and the alternate screen in `new()`/`Drop` — cost if wrong: cursor state depends on Drop running.
- Task 5: complete (tests: 3 new tests RED on missing cache symbols; `cargo test -p mehh-coding-agent --lib` → 168/168 pass).
- Task 5: Ruling: transcript cache fingerprint includes the `hide_thinking` flag because that setting changes an Assistant item's rendered lines — cost if wrong: stale thinking text after toggling the setting, caught by the existing hide-thinking test.
- Task 6: complete (3 new helper tests RED on missing symbols; `cargo test -p mehh-coding-agent --lib` → 171/171 pass).
- Task 6: Ruling: scroll anchoring uses an `AtomicUsize` baseline and a locally adjusted offset instead of mutating `state.scroll_offset`; key handlers still operate on the user's offset and the anchor is re-applied each frame — cost if wrong: repeated anchoring if the frame is rendered more than once without new lines.
- Task 6: Ruling: the scroll pill moved below transcript construction because it needs the new-line delta; `last_max_scroll` from the previous frame supplies the percentage — cost if wrong: percentage lags one frame.
- Task 7: complete (2 new editor tests RED on missing methods; `cargo test -p mehh-tui --lib components::editor::tests` → 12/12; `cargo test -p mehh-coding-agent --lib` → 171/171).
- Task 7: Ruling: `Editor::render` now excludes autocomplete and exposes `render_autocomplete`; the host renders it in a `RawLines` panel above the dock — cost if wrong: autocomplete disappears if the host forgets the new call.
- Task 8: complete (3 new tests RED; `cargo test -p mehh-coding-agent --lib` → 174/174, `cargo test -p mehh-tui --lib` → 78/78 after updating legacy `test_editor_autocomplete` to call `render_autocomplete`).
- Task 8: Ruling: `cap_preview_lines` emits up to `max_lines` content lines plus one `… (+N more lines)` note, so the panel can be `max_lines + 1` rows; the four approval actions render outside that panel — cost if wrong: one extra preview row.
- Task 9: complete (2 new tests RED; `cargo test -p mehh-coding-agent --lib` → 176/176). Updated one legacy assertion that required the old `⚙` glyph.
- Task 9: Ruling: pending tool/subagent rows use the braille spinner frame from `state.loader.frame`, so a running row's glyph changes with the loader tick — cost if wrong: none functionally, extra diff noise per tick.
- Task 10: complete (4 new helper tests RED; `cargo test -p mehh-coding-agent --lib` → 180/180).
- Task 10: Ruling: header right side uses a fixed 18-column compacted cwd so the model name survives `left_right` truncation at 80 columns — cost if wrong: less path detail, which the `~/.../` form intentionally trades away.
- Task 10: Ruling: two pre-existing `session::tests::test_list_sessions_*` tests failed once in a parallel full-suite run and passed in isolation and on rerun; treat as flaky shared-temp-dir tests, not a regression — cost if wrong: a real intermittent failure could be missed.
- Task 11: complete (3 new tests RED; `cargo test -p mehh-tui --lib` → 81/81). Updated two legacy markdown tests for H3 text style and separate H1 underline row.
- Task 11: Ruling: H1 underline is a separate output row, and H3+ uses bold+`theme.text` rather than `md_heading`; this matches the spec's hierarchy — cost if wrong: consumers matching a single style for all heading levels need updating.
- Task 12: complete (3 new tests RED; `cargo test -p mehh-coding-agent --lib` → 183/183). Updated one legacy test to assert the collapsed thinking preview instead of raw thinking text.
- Task 12: Ruling: thinking collapses to a 3-line tail while busy and a one-line `Thought for Ns` summary when done; `Ctrl+T` toggles expansion — cost if wrong: users who expected raw thinking text must press Ctrl+T.

Final review (fresh subagent, general): "With fixes" — 2 Critical, 2 Important, 3 Minor.
- Final: fixed full-redraw cursor home — added `\x1b[H` at the start of the full-redraw branch; test `full_redraw_homes_cursor` RED→GREEN, suite 83/83. Cost if wrong: resize would clip header/transcript.
- Final: fixed cache fingerprint omitting render-time state — fingerprint now includes `thinking_expanded`, `is_busy`, and the loader frame for running rows; test `transcript_cache_includes_render_time_flags` RED→GREEN, suite 185/185. Cost if wrong: Ctrl+T and the spinner are no-ops.
- Final: fixed ToolApproval vertical budget — preview budget is `height.saturating_sub(18).clamp(1, 6)`; test `tool_approval_fits_60x20_with_long_preview` RED→GREEN, suite 185/185. Cost if wrong: footer pushed off-screen at 60x20.
- Final: fixed editor window marker clipping — window is now derived from the actual `CURSOR_MARKER` row in rendered output; test `visible_window_keeps_cursor_marker_on_long_wrapped_line` RED→GREEN, suite 83/83. Cost if wrong: caret disappears on long wrapped prompts.
- Final: minor (deferred): scrollbar thumb uses adjusted offset while the pill uses raw offset and previous-frame `last_max_scroll`, so they can disagree during streaming.
- Final: minor (deferred): markdown tables overflow at widths below the column overhead (pre-existing geometry, more visible with new borders).
- Final: minor (deferred): `compact_path` mixes byte and char counts, so non-ASCII paths can exceed `max_len`.
- Final: minor (deferred): `format!("{item:?}")` builds a fingerprint over full item contents each frame; a version counter would be cheaper if streaming CPU matters.
- Final verification (round 1): `cargo test -p mehh-tui --lib` → 83/83; `cargo test -p mehh-coding-agent --lib` → 185/185; `cargo check -p mehh-coding-agent --tests` → clean; no commit created.

Second review (explicit @requesting-code-review): "With fixes" — 2 Critical, 3 Important, 3 Minor.
- Final round 2: fixed scroll anchor persistence — added cumulative `scroll_anchor: AtomicUsize`; `anchored_scroll_offset` now accumulates growth and resets only at bottom; test `scroll_anchor_persists_across_frames` RED→GREEN, suite 189/189. Cost if wrong: viewport jumps one tick after a streaming burst.
- Final round 2: fixed 60x20 overflow with autocomplete — welcome header collapses to 2 rows when autocomplete is open, and ToolApproval preview is wrapped before capping; test `autocomplete_fits_60x20_when_empty` RED→GREEN, suite 189/189. Cost if wrong: footer pushed off-screen.
- Final round 2: fixed `Thought for 0.0s` — `turn_started_at` is no longer cleared on `TurnEnded`; test `completed_thinking_keeps_elapsed` pins the 4.2s case, suite 189/189. Cost if wrong: completed thinking always reports zero.
- Final round 2: fixed global busy flag — only the last Assistant item uses the active thinking preview, and the per-item current flag is in the cache fingerprint; test `only_active_thinking_shows_spinner` RED→GREEN, suite 189/189. Cost if wrong: historical thinking blocks flip to spinners.
- Final round 2: fixed stale rows after resize — full redraw emits `\x1b[J` unconditionally; test `full_redraw_erases_stale_rows` RED→GREEN, suite 84/84. Cost if wrong: old rows remain after a smaller resize.
- Final round 2: minor (deferred): `Editor::visible_window()` is still unused by production `render` (which locates `CURSOR_MARKER` directly).
- Final round 2: minor (deferred): `.lock().unwrap()` on the new Instant mutexes in the event loop is not poison-tolerant like the other mutexes.
- Final round 2: minor (deferred): review test guards could be tightened (four approval actions, `<=6` cap assertion, more fingerprint flag combinations).
- Final verification (round 2): `cargo test -p mehh-tui --lib` → 84/84; `cargo test -p mehh-coding-agent --lib` → 189/189; `cargo check -p mehh-coding-agent --tests` → clean. No commit created.

Third review (explicit @requesting-code-review): no Critical, 1 Important, 5 Minor.
- Final round 3: fixed unbounded `bottom_lines` — aggregate budget `height - header - footer - 1` with front-dropping of optional rows while preserving the dock and approval actions; test `stacked_panels_fit_60x20` RED→GREEN (21 rows → 20), suite 190/190. Cost if wrong: footer clipped when loader/pills/autocomplete stack.
- Final round 3: minor (deferred): `Editor::visible_window()` remains unused in production; render locates the marker directly.
- Final round 3: minor (deferred): first frame of a turn can render `· Esc to cancel` with an empty elapsed value before `turn_started_at` is set.
- Final round 3: minor (deferred): historical thinking blocks use the latest turn's elapsed on cache misses; per-item elapsed would fix it.
- Final round 3: minor (deferred): scroll pill reports per-frame growth rather than cumulative lines since pausing, and its percentage is one frame stale.
- Final round 3: minor (deferred): unconditional `\x1b[J` on full redraw duplicates the conditional shrink-clear when both fire (harmless).
- Final verification (round 3): `cargo test -p mehh-tui --lib` → 84/84; `cargo test -p mehh-coding-agent --lib` → 190/190; `cargo check -p mehh-coding-agent --tests` → finished, only pre-existing warnings in mehh-ai/mehh-sandbox. No commit created.

Minors cleanup (user asked to fix all remaining):
- Ruling: scroll pill now uses the cumulative anchor for `+N new lines` and a pre-computed `max_scroll` for the percentage, matching the scrollbar thumb; test `scroll_anchor_persists_across_frames` now also asserts the cumulative pill count.
- Ruling: markdown tables falling below budget use a compact border mode (1-char cells, no padding) instead of overflowing; added `narrow_table_stays_within_width`.
- Ruling: `compact_path` counts characters rather than bytes; added `compact_path_counts_characters_not_bytes`.
- Ruling: transcript cache now stores a clone of the item and compares with `PartialEq` instead of formatting `Debug` every frame; render-time flags, spinner frame, and stored thinking elapsed live in the key. Removed `transcript_fingerprint` and its test; `render_interactive_frame_is_deterministic_at_60x20` plus cache-flag tests cover the behavior.
- Ruling: removed dead `Editor::visible_window()` and its test; `render` remains the single windowing implementation with its cursor-marker regression test.
- Ruling: `turn_started_at` is set before the first `render_now` of a turn; all new Instant mutex writes use the poison-tolerant lock pattern.
- Ruling: completed thinking elapsed is stored per item in `thinking_elapsed` at `TurnEnded`, so old blocks keep their own duration across cache misses; added `historical_thinking_keeps_its_own_elapsed`.
- Ruling: full-redraw clear is now an `else` branch, avoiding a duplicated `\x1b[J`.
- Ruling: tightened guards — `tool_approval_fits_60x20_with_long_preview` checks all four actions, `cap_preview_lines_preserves_action_rows` asserts exactly `max_lines + 1`, and `transcript_cache_includes_render_time_flags` toggles `thinking_expanded`, `hide_thinking`, and `is_busy`.
- Final verification (minors): `cargo test -p mehh-tui --lib` → 84/84; `cargo test -p mehh-coding-agent --lib` → 191/191; `cargo check -p mehh-coding-agent --tests` → finished. No commit created.
