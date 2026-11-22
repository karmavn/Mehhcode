# Wave 3 — Project Trust & Daily TUI Ergonomics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Isolate untrusted repository overrides (`.mehh/{mcp.json,settings.json,models.json,skills/,prompts/}`) behind a persisted fail-closed `trust.json` gate resolved pre-session, add scrollable TUI history + mouse wheel + live tool output/diff cards, and support mid-line `@file` substring completion in `Editor`.

**Architecture:** `mehh-coding-agent` adds `trust.rs` (`TrustStore`, `normalize_trust_key`, `has_workspace_overrides`), trust-aware loaders (`SettingsManager::load_with_trust`, `ModelRegistry::load_with_trust`, `load_skills_with_trust`, `load_prompt_templates_with_trust`, `McpManager::connect_from_configs_with_trust`), and pre-session `prompt_interactive_trust_decision`. `mehh-tui` adds `ScrollUp`/`ScrollDown`/`ScrollBottom` actions, `CrosstermTerminal` mouse capture management, and `autocomplete_replace_chars` prefix replacement on `Editor`. `interactive.rs` + `file_complete.rs` wire scrollback, `ToolCall` streaming preview + colored diff rendering, and `@file` autocomplete.

**Tech Stack:** Rust workspace crates (`crossterm`, `ignore`, `serde`, `serde_json`, `tokio`, `unicode-width`, `anyhow` — zero new external crates; enable workspace `ignore = { workspace = true }` inside `crates/mehh-coding-agent/Cargo.toml`).

**Spec:** [2026-11-13-strongest-rust-agent-design.md](../../specs/2026-11-13-strongest-rust-agent-design.md)

**Global Constraints:**
- Never connect workspace MCP servers, load workspace `.mehh/models.json`, `.mehh/settings.json`, `.mehh/skills/`, or `.mehh/prompts/` before trust resolves cleanly (`pre-session` gate in `mehh-cli/src/main.rs`).
- `AGENTS.md` / `CLAUDE.md` outside `.mehh/` continue loading as repository instructions.
- Strip UTF-8 BOM and return explicit `Err` on malformed `trust.json` (`ENOENT` -> empty store, fail closed on syntax/I/O error).
- Normalize `trust.json` map keys (`fs::canonicalize` fallback lexical -> `/` separators -> lowercase on Windows) so different casing/relative paths never bypass or duplicate decisions.
- Register new key actions in `TuiAction` (`crates/mehh-tui/src/keys.rs`), never hardcoded raw string checks in `interactive.rs`.
- After every wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must exit `0`.

**Review Focus:**
- **Fail-closed untrusted headless startup:** Verified by integration tests ensuring untrusted `.mehh/{mcp.json,settings.json,models.json,skills,prompts}` are completely skipped in headless mode while `--trust` enables them ephemerally without writing `trust.json`.
- **Pre-session trust resolution order:** Verified in `main.rs` so `SettingsManager::load_with_trust`, `ModelRegistry::load_with_trust`, `McpManager::connect_from_configs_with_trust`, and `AgentSession::new_with_trust` run strictly after trust is decided.
- **Scrollback clamping & auto-follow reset:** Verified by tests for `scroll_offset` clamping on resize/short transcript and reset on user submit / `ScrollBottom`.
- **Mid-line `@file` replacement:** Verified in `editor.rs` tests confirming accepting `@auth` in `"inspect @auth please"` produces `"inspect @crates/mehh-coding-agent/src/auth.rs please"` without losing surrounding text.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Create | `crates/mehh-coding-agent/src/trust.rs` | `TrustDecision`, `TrustStore`, `normalize_trust_key`, `has_workspace_overrides`, fail-closed JSON loader/saver |
| Modify | `crates/mehh-mcp/src/config.rs` | Add `load_mcp_configs_with_trust(cwd, trusted: bool)` |
| Modify | `crates/mehh-coding-agent/src/mcp_manager.rs` | Add `McpManager::connect_from_configs_with_trust(cwd, trusted: bool)` |
| Modify | `crates/mehh-coding-agent/src/{settings.rs, models_config.rs, skills.rs, prompt_templates.rs, agent_session.rs}` | Add `*_with_trust(..., trusted: bool)` variants respecting trust gate for `<cwd>/.mehh/*` |
| Modify | `crates/mehh-coding-agent/src/interactive.rs` | Add `SelectorKind::Trust`, `prompt_interactive_trust_decision(cwd)`, `scroll_offset`, mouse scroll handling, rich `TranscriptItem::ToolCall` diff/stream rendering, `@file` autocomplete wiring |
| Modify | `crates/mehh-tui/src/{keys.rs, tui.rs}` | `TuiAction::{ScrollUp, ScrollDown, ScrollBottom}`, `CrosstermTerminal::enable_mouse_capture` + cleanup in `Drop` |
| Modify | `crates/mehh-tui/src/components/editor.rs` | `cursor()`/`lines()` accessors, `autocomplete_replace_chars`, `set_autocomplete_replacing`, char-range prefix replacement on accept |
| Modify | `crates/mehh-coding-agent/Cargo.toml` | Enable existing workspace dep `ignore = { workspace = true }` |
| Create | `crates/mehh-coding-agent/src/file_complete.rs` | `WorkspaceFileIndex` (`ignore::WalkBuilder`, max `5_000` files, `5s` TTL) + `extract_at_query` + substring ranking |
| Modify | `crates/mehh-coding-agent/src/lib.rs` | Export `trust` and `file_complete` |
| Modify | `crates/mehh-cli/src/main.rs` | `--trust` CLI flag, pre-session trust resolution (`prompt_interactive_trust_decision` vs fail-closed headless), pass `trusted` to all `*_with_trust` loaders |

---

### Task 1: Project Trust Store (`trust.json`) & Pre-Session Startup Gate

**Files:**
- Create: `crates/mehh-coding-agent/src/trust.rs`
- Modify: `crates/mehh-mcp/src/config.rs`
- Modify: `crates/mehh-coding-agent/src/{mcp_manager.rs, settings.rs, models_config.rs, skills.rs, prompt_templates.rs, agent_session.rs, interactive.rs, lib.rs}`
- Modify: `crates/mehh-cli/src/main.rs`

**Interfaces:**
- **Consumes:** `std::fs::canonicalize`, `CrosstermTerminal`, `SelectList`
- **Produces:** `trust::{TrustDecision, TrustStore, normalize_trust_key, has_workspace_overrides}`, `--trust` CLI flag, `*_with_trust` constructors, `prompt_interactive_trust_decision`

- [ ] **Step 1: Write failing tests in `trust.rs` and trust-gated loaders**

1. `test_normalize_trust_key_normalizes_separators_and_case`: equivalent casing/relative paths resolve to identical key on Windows (and clean canonical slash paths cross-platform).
2. `test_trust_store_enoent_bom_and_fail_closed_invalid_json`: missing file yields empty store; BOM-prefixed JSON loads; malformed JSON returns `Err` (never silent empty store).
3. `test_has_workspace_overrides_detects_dot_mehh_only`: directory with only `AGENTS.md` returns `false`; adding any of `.mehh/{mcp.json, settings.json, models.json, skills/, prompts/}` returns `true`.
4. `test_untrusted_loaders_skip_workspace_overrides`: populate workspace `.mehh/{mcp.json, settings.json, models.json, skills/local/SKILL.md, prompts/local.md}`; assert all `*_with_trust(&cwd, false)` calls ignore local entries while `*_with_trust(&cwd, true)` include them.

- [ ] **Step 2: Run targeted tests to verify RED**

Run: `cargo test -p mehh-coding-agent trust`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/mehh-coding-agent/src/trust.rs` & trust-aware loaders**

1. `crates/mehh-coding-agent/src/trust.rs`:
   - `TrustDecision { pub trusted: bool, pub decided_at: i64 }`
   - `TrustStore { pub version: u32, pub entries: HashMap<String, TrustDecision> }` + `default_path()` (`~/.mehh/agent/trust.json`), `load_file(&Path) -> Result<Self>`, `save_file(&Path) -> Result<()>` (write UTF-8 without BOM), `get(&Path) -> Option<bool>`, `set(&Path, bool)`.
   - `normalize_trust_key(cwd: &Path) -> String`: `fs::canonicalize(cwd)` fallback lexical normalization -> strip `\\?\` Windows verbatim prefix if present -> replace `\` with `/` -> `.to_lowercase()` when `cfg!(windows)`.
   - `has_workspace_overrides(cwd: &Path) -> bool`: checks `.mehh/{mcp.json, settings.json, models.json, skills, prompts}`.
2. Add `*_with_trust(..., trusted: bool)` across:
   - `crates/mehh-mcp/src/config.rs`: `load_mcp_configs_with_trust(cwd, trusted)` (keeps `load_mcp_configs` delegating `trusted = true` so unit tests stay concise, or explicit callers choose).
   - `crates/mehh-coding-agent/src/mcp_manager.rs`: `McpManager::connect_from_configs_with_trust(cwd, trusted)`.
   - `SettingsManager::load_with_trust(cwd, trusted)` in `settings.rs`.
   - `ModelRegistry::load_with_trust(cwd, trusted)` in `models_config.rs`.
   - `load_skills_with_trust(cwd, trusted)` in `skills.rs`.
   - `load_prompt_templates_with_trust(cwd, trusted)` in `prompt_templates.rs`.
   - `AgentSession::new_with_trust(cwd, model, provider, session, settings, trusted)` in `agent_session.rs`.
3. Interactive pre-session modal (`crates/mehh-coding-agent/src/interactive.rs`):
   - Add `SelectorKind::Trust` and `pub fn prompt_interactive_trust_decision(cwd: &Path) -> anyhow::Result<bool>` rendering a standalone bordered prompt (`Trust workspace <cwd>?`, items `"trust"` -> `"Yes, trust this workspace"` / `"deny"` -> `"No, run without workspace .mehh/ overrides"`) over `CrosstermTerminal`, returning `true`/`false` on Enter or `false` on Esc/`Ctrl+C`.
4. Pre-session CLI gate (`crates/mehh-cli/src/main.rs`):
   - Add `#[arg(long)] pub trust: bool` on `CliArgs`.
   - After `--login` / `--logout`, determine `interactive = should_run_interactive(&args, parse_cli_mode(&args.mode)?, io::stdin().is_terminal())`.
   - Compute `trusted: bool`:
     - If `!has_workspace_overrides(&cwd)` -> `false` (nothing gated to load).
     - Else if `args.trust` -> `true` (ephemeral, do NOT mutate `trust.json`).
     - Else load `TrustStore::load_file(&TrustStore::default_path())?`:
       - If `Some(decision)` saved -> `decision`.
       - Else if `!interactive` -> emit warning on stderr (`"Untrusted workspace .mehh/ overrides skipped; pass --trust to allow"`) and resolve `false`.
       - Else (`interactive`) -> `let accepted = prompt_interactive_trust_decision(&cwd)?; store.set(&cwd, accepted); store.save_file(&TrustStore::default_path())?; accepted`.
   - Pass `trusted` into `SettingsManager::load_with_trust`, `ModelRegistry::load_with_trust`, `McpManager::connect_from_configs_with_trust`, and `AgentSession::new_with_trust`, preserving existing CLI flag overrides (`--model`, `--provider`, `--api-key`, `--thinking`) cleanly on top.
   - Note for `--list-models`: load `ModelRegistry::load_with_trust(&cwd, trusted_if_saved_or_flag)` without launching interactive prompt.

- [ ] **Step 4: Run targeted tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-mcp`
`cargo test -p mehh-coding-agent trust`
`cargo test -p mehh-cli`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 2: TUI Transcript Scrollback, Mouse Wheel & Rich Tool Output/Diff Cards

**Files:**
- Modify: `crates/mehh-tui/src/keys.rs`
- Modify: `crates/mehh-tui/src/tui.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Consumes:** `TuiAction`, `CrosstermTerminal`, `AgentEvent::{ToolExecutionStart, ToolExecutionUpdate, ToolExecutionEnd}`, `Theme::{tool_pending, tool_success, tool_error, dim, text}`
- **Produces:** `TuiAction::{ScrollUp, ScrollDown, ScrollBottom}`, `CrosstermTerminal::enable_mouse_capture`, `InteractiveState::scroll_offset`, enriched `TranscriptItem::ToolCall`

- [ ] **Step 1: Write failing tests in `keys.rs` and `interactive.rs`**

1. In `crates/mehh-tui/src/keys.rs`:
   - `test_scroll_keybindings_registered`: assert default `Keybindings` matches `"pageup"` -> `ScrollUp`, `"pagedown"` -> `ScrollDown`, `"ctrl+end"` -> `ScrollBottom`.
2. In `crates/mehh-coding-agent/src/interactive.rs`:
   - `test_scroll_offset_freezes_and_clamps_transcript_viewport`: populate 60 lines of transcript items; at `scroll_offset = 0`, frame shows bottom tail; at `scroll_offset = 10`, frame freezes 10 lines up and footer displays `"[scrolled -10 lines • PgDn/Ctrl+End]"`; at `scroll_offset = 9999`, frame clamps safely to top without panicking.
   - `test_rich_tool_call_tracks_args_streaming_tail_and_diff`: send `ToolExecutionStart` (`args = {"path": "src/lib.rs"}`), two `ToolExecutionUpdate` events (`25` lines total -> keeps tail `10`), and `ToolExecutionEnd` (`details = {"diff": "+added\n-removed"}`); assert rendered frame includes args summary, streaming tail, and colorized `+added`/`-removed` diff capped at `24` lines.

- [ ] **Step 2: Run targeted tests to verify RED**

Run:
`cargo test -p mehh-tui test_scroll_keybindings`
`cargo test -p mehh-coding-agent test_scroll_offset test_rich_tool_call`
Expected: FAIL.

- [ ] **Step 3: Implement `TuiAction` scroll bindings & mouse capture (`mehh-tui`)**

1. `crates/mehh-tui/src/keys.rs`: add `ScrollUp`, `ScrollDown`, `ScrollBottom` variants to `TuiAction`; bind `"pageup"`, `"pagedown"`, `"ctrl+end"` in `Keybindings::default_bindings()`. Also verify `normalize_crossterm_key` in `interactive.rs` maps `KeyCode::PageUp`, `KeyCode::PageDown`, `KeyCode::End` (with modifiers -> `"ctrl+end"`).
2. `crates/mehh-tui/src/tui.rs`: add `mouse_capture_enabled: bool` (`false` in `new()`), `pub fn enable_mouse_capture(&mut self) -> std::io::Result<()>` issuing `crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)`, and emit `DisableMouseCapture` in `Drop` when enabled before `disable_raw_mode()`.

- [ ] **Step 4: Implement scrollback & rich `TranscriptItem::ToolCall` rendering (`interactive.rs`)**

1. Add `pub scroll_offset: usize` (`0` default) to `InteractiveState`.
2. Extend `TranscriptItem::ToolCall`:
   `ToolCall { id: String, name: String, args_summary: String, status: String, output_preview: String, diff: Option<String>, is_error: bool }`
   Update `message_to_transcript_item` and all existing unit tests in `interactive.rs`.
3. Add concise formatting helpers:
   - `summarize_tool_args(name: &str, args: &serde_json::Map<String, serde_json::Value>) -> String` (prefers `"path"`, `"command"`, `"query"`, `"pattern"`, falling back cleanly truncated to 60 chars).
   - `tail_lines(text: &str, max_lines: usize) -> String` (retains last `max_lines` non-empty tail cleanly).
   - In `apply_agent_event`: populate `args_summary` on `ToolExecutionStart`, update `output_preview = tail_lines(..., 10)` on `ToolExecutionUpdate`, populate `details["diff"]` (capped cleanly when rendering at 24 lines) and final `output_preview` when appropriate on `ToolExecutionEnd`.
4. In `render_interactive_frame`:
   - Color `+` lines with `theme.tool_success`, `-` lines with `theme.tool_error`, hunk `@@` headers with `theme.accent` (up to 24 diff lines).
   - Slice `transcript_lines` using `let max_offset = transcript_lines.len().saturating_sub(available_height); let offset = state.scroll_offset.min(max_offset); let end = transcript_lines.len() - offset; let start = end.saturating_sub(available_height);`
   - When `offset > 0`, show `[scrolled -{offset} lines • PgDn/Ctrl+End]` in footer!
5. In `run_interactive_mode`:
   - Enable mouse capture cleanly via `terminal.enable_mouse_capture()`.
   - Handle `Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp | ScrollDown, .. })` (`+/- 3` lines on `state.scroll_offset`).
   - Handle `TuiAction::ScrollUp` (`+ half-page`), `ScrollDown` (`saturating_sub(half-page)`), `ScrollBottom` (`= 0`) when no modal selector intercepts them; reset `state.scroll_offset = 0` on `EditorOutput::Submit`.

- [ ] **Step 5: Run tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-tui`
`cargo test -p mehh-coding-agent`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 3: `@file` Substring Path Completion & Editor Prefix Replacement

**Files:**
- Modify: `crates/mehh-tui/src/components/editor.rs`
- Modify: `crates/mehh-coding-agent/Cargo.toml`
- Create: `crates/mehh-coding-agent/src/file_complete.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Consumes:** `ignore::WalkBuilder`, `Editor::{lines, cursor_row, cursor_col}`
- **Produces:** `Editor::set_autocomplete_replacing`, `file_complete::{WorkspaceFileIndex, extract_at_query}`

- [ ] **Step 1: Write failing tests in `editor.rs` and `file_complete.rs`**

1. In `crates/mehh-tui/src/components/editor.rs`:
   - `test_editor_autocomplete_replaces_at_token_without_touching_surrounding_text`: type `"check @auth please"`, position cursor right after `"@auth"`, call `editor.set_autocomplete_replacing(vec![SelectItem::new("src/auth.rs", "@src/auth.rs")], 5)`, send `"tab"` -> `editor.text() == "check @src/auth.rs please"`, AND existing slash-command `set_autocomplete` (`replace_chars = 0`) behavior stays identical.
2. In `crates/mehh-coding-agent/src/file_complete.rs`:
   - `test_extract_at_query_respects_boundaries`: `"@foo"` -> `Some((4, "foo"))`; `"inspect @src/lib please"` with cursor after `@src/lib` -> `Some((8, "src/lib"))`; `"email@domain.com"` (non-whitespace before `@`) -> `None`.
   - `test_workspace_file_index_ignores_git_target_and_ranks_prefix_above_substring`: create temp dir with `.gitignore` ignoring `ignored.txt`, plus `target/foo.rs`, `src/auth.rs`, `tests/my_auth_test.rs`; assert `WorkspaceFileIndex` excludes `.gitignore`/`.git`/`target`/`node_modules` and ranks `src/auth.rs` / `auth` match cleanly.

- [ ] **Step 2: Run targeted tests to verify RED**

Run:
`cargo test -p mehh-tui test_editor_autocomplete_replaces`
`cargo test -p mehh-coding-agent file_complete`
Expected: FAIL.

- [ ] **Step 3: Implement `Editor::set_autocomplete_replacing` (`editor.rs`)**

1. Add `pub autocomplete_replace_chars: usize` (`0` in `Editor::new()`).
2. Add read-only getters `pub fn cursor_position(&self) -> (usize, usize) { (self.cursor_row, self.cursor_col) }` and `pub fn current_line(&self) -> &str`.
3. Add `pub fn set_autocomplete_replacing(&mut self, items: Vec<SelectItem>, replace_chars: usize)`; keep `set_autocomplete` calling `set_autocomplete_replacing(items, 0)`, and reset `self.autocomplete_replace_chars = 0` inside `clear_autocomplete`.
4. On `"enter" | "tab"` accept in `handle_editor_key`:
   Save `let remove_chars = self.autocomplete_replace_chars;`, call `self.clear_autocomplete();`, delete `remove_chars` chars immediately before `self.cursor_col` on `self.lines[self.cursor_row]` (respecting UTF-8 char boundaries and shifting `self.cursor_col` left cleanly), then `self.insert_text(&item.label)`.

- [ ] **Step 4: Implement `file_complete.rs` & wire into `interactive.rs`**

1. Enable `ignore = { workspace = true }` in `crates/mehh-coding-agent/Cargo.toml`.
2. Implement `crates/mehh-coding-agent/src/file_complete.rs`:
   - `extract_at_query(line: &str, cursor_col: usize) -> Option<(usize, String)>`: takes char prefix up to `cursor_col`, finds last `@` after line start or whitespace with no whitespace between `@` and cursor, returns `(1 + partial.chars().count(), partial.to_string())`.
   - `WorkspaceFileIndex`: caches `Vec<String>` normalized forward-slash paths up to `5_000` files using `ignore::WalkBuilder`, filtering out `.git`, `target`, `node_modules`, with `5s` TTL refresh.
   - `complete(&mut self, partial: &str, limit: usize) -> Vec<SelectItem>`: scores exact basename/path prefix higher than basename substring higher than path substring, limits to top `12`, formatting `SelectItem::new(path.clone(), format!("@{path}")).with_description(path)`.
3. In `crates/mehh-coding-agent/src/interactive.rs`: add `pub file_index: WorkspaceFileIndex` on `InteractiveState` (seeded with `session.cwd.clone()` when initialized inside `run_interactive_mode`), and update `update_autocomplete` to inspect `(line, cursor_col)`:
   - Slash command `/` at column start uses `editor.set_autocomplete(matches)`.
   - Otherwise check `extract_at_query(line, cursor_col)` and pass top matches via `editor.set_autocomplete_replacing(items, replace_chars)`.

- [ ] **Step 5: Run full workspace tests & clippy**

Run:
`cargo test --workspace`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-11-13-wave-3-trust-and-tui.md`. Two execution options:
1. **Subagent-Driven (`superpowers:subagent-driven-development`)** — dispatch one fresh coding subagent per task + two-stage spec/code review between tasks.
2. **Inline (`superpowers:executing-plans`)** — implement task-by-task directly in the current session with verification after each task.
