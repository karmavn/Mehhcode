# Wave 6 — `mehh-mcp`, `mehh-tui` & `mehh-codemode` Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add full MCP request cancellation (`notifications/cancelled`), progress reporting with timeout renewal (`notifications/progress`), disk persistence for oversized text and binary blobs (`McpOutputSaver`), live `notifications/tools/list_changed` refresh in interactive mode across `AgentSession`, `DeferredToolCatalog`, and `CodemodeTool`, plus TUI `fuzzy_match`/`fuzzy_filter`, Emacs multi-entry `KillRing`, punctuation/paste-aware word navigation, `WheelScrollAccelerator`, and 2-column `SettingsList`.

**Architecture:** `mehh-mcp` upgrades `McpClient` with per-request cancellation/progress dispatch, deadline renewal, and dynamic handler registries, while `tool_adapter.rs` persists large/binary outputs using `0o600` temp files and forwards `on_update`. `mehh-coding-agent` and `mehh-codemode` share `Arc<RwLock<Vec<Arc<dyn AgentTool>>>>` tool lists so `notifications/tools/list_changed` updates interactive tools cleanly between turns. `mehh-tui` adds `fuzzy.rs`, `KillRing` + word navigation in `editor.rs`, `WheelScrollAccelerator`, and `SettingsList`.

**Tech Stack:** Rust workspace crates (`tokio`, `tokio-util`, `serde`, `serde_json`, `unicode-width`, `unicode-segmentation`, `crossterm`, `anyhow` — zero new external crates).

**Spec:** [2026-11-15-ts-parity-waves-4-5-6-design.md](../../specs/2026-11-15-ts-parity-waves-4-5-6-design.md)

## Global Constraints

- Zero new external Cargo crates.
- Never send `notifications/cancelled` for `"initialize"` requests (MCP spec prohibition).
- Persist oversized/binary MCP outputs into temp files with private `0o600` permissions on Unix and `create_new(true)` (`O_EXCL`-style symlink safety).
- Register new TUI actions (`TuiAction::YankPop` = `alt+y`) in `crates/mehh-tui/src/keys.rs` rather than hardcoding key matches in `editor.rs` or `interactive.rs`.
- Live `notifications/tools/list_changed` propagation is scoped to interactive mode (`AgentSession` + `DeferredToolCatalog` + `CodemodeTool`), keeping `run_rpc_mode` / print mode signatures unchanged.
- After Wave 6 completes, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with exit code `0`.

## Review Focus

- **Never cancel `initialize` handshake:** Covered by `client::tests::timeout_on_initialize_does_not_send_cancelled_notification`.
- **`notifications/progress` renews deadline so slow long-running tools do not time out mid-progress:** Covered by `client::tests::progress_notification_renews_timeout_and_invokes_callback`.
- **Binary MCP resource blob saved to disk instead of dropped as `"[binary resource omitted]"`:** Covered by `tool_adapter::tests::convert_mcp_result_saves_binary_blob_and_oversized_text`.
- **`KillRing` direction-aware accumulation + `yank_pop` wrapping and `[paste #N ...]` atomic word navigation:** Covered by `editor::tests::kill_ring_accumulates_rotates_and_word_nav_skips_paste_token`.
- **Live `notifications/tools/list_changed` updates `DeferredToolCatalog` and `CodemodeTool` without manual `/mcp reconnect`:** Covered by `mcp_manager::tests::tools_list_changed_refreshes_catalog_and_codemode`.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Modify | `crates/mehh-mcp/src/client.rs` | `notifications/cancelled` (send + receive), `notifications/progress` (`_meta.progressToken`, timeout renewal, callback), `on_notification`, `on_error`, `on_close`, `set_request_handler` |
| Modify | `crates/mehh-mcp/src/tool_adapter.rs` | `McpOutputSaver` callback + temp-file `0o600` writer for `>20KB` text and binary resource blobs; wire `on_update` + cancellation in `McpAgentTool::execute` |
| Modify | `crates/mehh-codemode/src/tool.rs` | Store `tools` behind `Arc<RwLock<Vec<Arc<dyn AgentTool>>>>` + `replace_tools(&self, ...)` |
| Modify | `crates/mehh-coding-agent/src/tool_search.rs` | Wrap `deferred` behind `Arc<RwLock<Vec<Arc<dyn AgentTool>>>>` + `replace_deferred(&self, ...)` |
| Modify | `crates/mehh-coding-agent/src/mcp_manager.rs` & `interactive.rs` & `agent_session.rs` | Listen for `notifications/tools/list_changed`, refresh server tools, and sync interactive `AgentSession`, `DeferredToolCatalog`, `CodemodeTool`, and `rebuild_system_prompt()` between turns |
| Create | `crates/mehh-tui/src/fuzzy.rs` | `fuzzy_match` and tokenized `fuzzy_filter` |
| Modify | `crates/mehh-tui/src/lib.rs` & `components/basic.rs` | Export `fuzzy`; wire `fuzzy_filter` into `SelectList`; add 2-column `SettingsList` component |
| Modify | `crates/mehh-tui/src/keys.rs` & `components/editor.rs` | Register `TuiAction::YankPop` (`alt+y`); multi-entry `KillRing` with accumulation/rotation; punctuation & `[paste #N ...]`-aware `word_navigation` |
| Create | `crates/mehh-tui/src/wheel_scroll.rs` | `WheelScrollAccelerator` (`1`–`6` line velocity scaling) |
| Modify | `crates/mehh-coding-agent/src/interactive.rs` | Wire `WheelScrollAccelerator` and 2-column `SettingsList` into interactive loop & `/settings` |

---

### Task 1: `mehh-mcp` Cancellation, Progress & Dynamic Handlers (`client.rs`)

**Files:**
- Modify: `crates/mehh-mcp/src/client.rs`

**Interfaces:**
- **Consumes:** `JsonRpcNotification`, `CancellationToken`, `McpTransport`
- **Produces:**
  - `pub struct McpProgressNotification { pub progress_token: serde_json::Value, pub progress: f64, pub total: Option<f64>, pub message: Option<String> }`
  - `pub type ProgressCallback = Arc<dyn Fn(McpProgressNotification) + Send + Sync>`
  - `McpClient::{request_with_options, call_tool_with_options, on_notification, on_error, on_close, set_request_handler}`

- [ ] **Step 1: Write failing tests in `crates/mehh-mcp/src/client.rs` using `InMemoryTransport`**

Assert:
1. `cancelled_request_sends_notifications_cancelled`: cancelling `CancellationToken` mid-request sends JSON-RPC `notifications/cancelled` with matching `requestId` and optional `reason`.
2. `timeout_on_initialize_does_not_send_cancelled_notification`: timing out on `"initialize"` omits `notifications/cancelled`.
3. `incoming_notifications_cancelled_triggers_server_request_token`: server-sent `notifications/cancelled` cancels the registered client request handler's `CancellationToken`.
4. `progress_notification_renews_timeout_and_invokes_callback`: with `timeout = 80ms`, sending 2 `notifications/progress` frames at `50ms` and `100ms` lets the request finish cleanly at `140ms` while delivering both `McpProgressNotification` values to `on_progress`.
5. `dynamic_notification_and_request_handler_registries_fire`: tests `on_notification`, `set_request_handler`, `on_error`, and `on_close`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-mcp client::tests`
Expected: FAIL.

- [ ] **Step 3: Implement cancellation, progress + deadline loop, and dynamic registries in `client.rs`**

In `McpClient::request_with_options`: inject `_meta.progressToken` when `on_progress` is `Some`; race response against `tokio::time::sleep_until(deadline)` (extended on matching `progress` notifications) and `cancel.cancelled()`; emit `notifications/cancelled` whenever `method != "initialize"` aborts or times out. Route server requests/notifications/errors/EOF through registered callbacks before default handling.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-mcp client::tests`
Expected: PASS.

---

### Task 2: `McpOutputSaver` Persistence, Progress `on_update`, and Live `tools/list_changed` Refresh

**Files:**
- Modify: `crates/mehh-mcp/src/tool_adapter.rs`
- Modify: `crates/mehh-codemode/src/tool.rs`
- Modify: `crates/mehh-coding-agent/src/tool_search.rs`
- Modify: `crates/mehh-coding-agent/src/mcp_manager.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Consumes:** `McpClient::{call_tool_with_options, on_notification}`, `std::env::temp_dir()`
- **Produces:**
  - `pub type McpOutputSaver = Arc<dyn Fn(Vec<u8>, String) -> BoxFuture<'static, anyhow::Result<PathBuf>> + Send + Sync>`
  - `tool_adapter::save_mcp_output_file(data: &[u8], ext: &str) -> anyhow::Result<PathBuf>` (`create_new(true)`, `0o600` on Unix)
  - `CodemodeTool::replace_tools(&self, tools: Vec<Arc<dyn AgentTool>>)`
  - `DeferredToolCatalog::replace_deferred(&self, deferred: Vec<Arc<dyn AgentTool>>)`
  - `McpManager::take_tools_dirty(&self) -> bool` / live refresh handler on `notifications/tools/list_changed`

- [ ] **Step 1: Write failing tests across `tool_adapter.rs`, `tool.rs`, `tool_search.rs`, and `mcp_manager.rs`**

Assert:
1. `tool_adapter::tests::convert_mcp_result_saves_binary_blob_and_oversized_text`: `>20KB` text truncates middle and appends `[Full output: <path> (read it with offset/limit)]` pointing to a readable file containing the full payload; binary resource blob saves decoded bytes to disk and appends `[Binary resource <uri> saved to <path>]`.
2. `tool_adapter::tests::mcp_agent_tool_forwards_progress_to_on_update_and_cancels_server`: `McpAgentTool::execute` streams progress via `on_update` and propagates `cancel` to `McpClient`.
3. `mcp_manager::tests::tools_list_changed_refreshes_catalog_and_codemode`: simulated `notifications/tools/list_changed` notification updates `McpManager`, `DeferredToolCatalog`, and `CodemodeTool` so newly added MCP tools are immediately discoverable without `/mcp reconnect`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-mcp tool_adapter::tests && cargo test -p mehh-coding-agent tools_list_changed`
Expected: FAIL.

- [ ] **Step 3: Implement `McpOutputSaver`, `on_update` forwarding, and interactive `tools/list_changed` refresh**

Make `convert_mcp_result` async (`convert_mcp_result_with_saver`), add `save_mcp_output_file` (`create_new(true)` + Unix `0o600`), update `McpAgentTool::execute` (`crates/mehh-mcp/src/tool_adapter.rs:349-388`) to call `McpClient::call_tool_with_options` (forwarding progress notifications via `on_update` and `CancellationToken`) with `call_tool` delegating to it, switch `CodemodeTool.tools` and `DeferredToolCatalog.deferred` to `Arc<RwLock<Vec<Arc<dyn AgentTool>>>>`, subscribe `McpManager` to `notifications/tools/list_changed`, and sync between interactive turns in `interactive.rs` + `AgentSession`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-mcp -p mehh-codemode -p mehh-coding-agent`
Expected: PASS.

---

### Task 3: `mehh-tui` `fuzzy_match` / `fuzzy_filter` & `SelectList` Integration

**Files:**
- Create: `crates/mehh-tui/src/fuzzy.rs`
- Modify: `crates/mehh-tui/src/lib.rs`
- Modify: `crates/mehh-tui/src/components/basic.rs:211-285`

**Interfaces:**
- **Produces:**
  - `pub struct FuzzyMatch { pub matches: bool, pub score: f64 }`
  - `pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch`
  - `pub fn fuzzy_filter<T>(items: &[T], query: &str, get_text: impl Fn(&T) -> &str) -> Vec<(usize, &T)>` (sorted best-first)

- [ ] **Step 1: Write failing tests in `crates/mehh-tui/src/fuzzy.rs` and `basic.rs`**

Assert:
1. `fuzzy_match_scores_word_boundaries_higher_than_scattered_subsequence` and `supports_digit_letter_swap` (`"gpt4"` matches `"claude-4gpt"` / `"gpt-4o"`).
2. `fuzzy_filter_tokenized_requires_all_space_separated_tokens`: `"sonnet 4"` matches `"claude-sonnet-4-5"` and rejects `"claude-opus-4"`.
3. `select_list_uses_fuzzy_filter_ordering`: `"cson"` ranks `"claude-sonnet"` ahead of substring-non-contiguous misses.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-tui fuzzy select_list_uses_fuzzy`
Expected: FAIL.

- [ ] **Step 3: Implement `fuzzy.rs` and wire into `SelectList::filtered_indices`**

Port TS scoring rules (word boundary, consecutive run bonus, gap/position penalties, alphanumeric swap) and update `SelectList` filtering/ordering in `basic.rs`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-tui`
Expected: PASS.

---

### Task 4: Emacs `KillRing` & Punctuation/Paste-Aware `word_navigation`

**Files:**
- Modify: `crates/mehh-tui/src/keys.rs`
- Modify: `crates/mehh-tui/src/components/editor.rs`

**Interfaces:**
- **Produces:**
  - `TuiAction::YankPop` (default `"alt+y"`)
  - `pub struct KillRing` (`push`, `peek`, `rotate`, accumulation mode)
  - `editor.rs` `move_word_left`, `move_word_right`, `delete_word_backward`, `delete_word_forward`, `yank`, `yank_pop`

- [ ] **Step 1: Write failing tests in `crates/mehh-tui/src/components/editor.rs`**

Assert:
1. `kill_ring_accumulates_rotates_and_word_nav_skips_paste_token`:
   - Consecutive backward kills accumulate into one entry; an intervening non-kill key starts a new ring entry; `ctrl+y` followed by `alt+y` (`YankPop`) replaces yanked text with the prior ring entry and wraps cleanly.
   - `move_word_left` / `move_word_right` stop on punctuation boundaries (`foo.bar/baz`) and step atomically across `[paste #1 +3 lines]`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-tui kill_ring_accumulates`
Expected: FAIL.

- [ ] **Step 3: Implement `KillRing`, `TuiAction::YankPop`, and `word_navigation` in `keys.rs` + `editor.rs`**

Replace `kill_ring: String` with `KillRing` (bounded capacity, e.g., 60 entries) and track `last_yank_range` + `last_action_was_kill` for accumulation and `yank_pop`; port TS punctuation/word/atomic-paste segment scanning.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-tui`
Expected: PASS.

---

### Task 5: `WheelScrollAccelerator`, 2-Column `SettingsList` & Wave 6 Gate

**Files:**
- Create: `crates/mehh-tui/src/wheel_scroll.rs`
- Modify: `crates/mehh-tui/src/lib.rs`
- Modify: `crates/mehh-tui/src/components/basic.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Produces:**
  - `pub struct WheelScrollAccelerator` (`record_step(&mut self, direction: i8, now: Instant) -> usize`)
  - `pub struct SettingItem { pub id: String, pub label: String, pub current_value: String, pub values: Vec<String>, pub description: String }`
  - `pub struct SettingsList` implementing `Component` (2-column label/value + description pane + `cycle_current` + filter)

- [ ] **Step 1: Write failing tests in `wheel_scroll.rs`, `basic.rs`, and `interactive.rs`**

Assert:
1. `wheel_scroll_accelerates_on_rapid_bursts_and_resets_on_gap_or_reversal`: single isolated tick returns `1`, rapid ticks within gesture window scale up to `MAX_AUTO_LINES = 6`, reversing direction or pausing `> 200ms` resets to `1`.
2. `settings_list_renders_two_columns_description_and_cycles_values`: verifies formatted 2-column alignment, selected description footer, and value cycling.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-tui wheel_scroll settings_list`
Expected: FAIL.

- [ ] **Step 3: Implement `wheel_scroll.rs`, `SettingsList`, and wire into `interactive.rs`**

Replace hardcoded `3`-line mouse scroll offsets in `interactive.rs` with `WheelScrollAccelerator` on `InteractiveState`, and upgrade `/settings` selector rendering in `interactive.rs` to use `SettingsList`.

- [ ] **Step 4: Run Wave 6 tests & full workspace verification gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (exit code 0, zero warnings).
