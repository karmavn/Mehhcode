# Wave 2 — Sharper Tools, BM25 Tool Search & Subagent Delegation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate silent command hangs via real-time `BashTool` streaming, recover from harmless formatting/smart-character mismatches in `EditTool` Tier-2, keep system prompts lean under large MCP server toolkits via Okapi BM25 `tool_search`, and isolate long codebase research or scoped edits inside a depth-bounded `subagent` tool.

**Architecture:** `mehh-tools` upgrades `BashTool` and `EditTool` in place using existing `tokio` + `similar` crates. `mehh-agent` extends `AgentLoopConfig` with `dynamic_tools: Option<Arc<RwLock<Vec<Arc<dyn AgentTool>>>>>` (reread every LLM turn) and `max_turns: Option<usize>`. `mehh-coding-agent` adds `tool_search.rs` (BM25 over MCP tools + shared active tool mutation) and `subagent.rs` (`ToolExecutionMode::Sequential`, in-memory child loop without `subagent`, linked cancellation).

**Tech Stack:** Rust workspace crates (`tokio`, `tokio-util`, `similar`, `serde`, `serde_json`, `anyhow`, `async-trait` — zero new external crates).

**Spec:** [2026-11-13-strongest-rust-agent-design.md](../../specs/2026-11-13-strongest-rust-agent-design.md)

**Global Constraints:**
- Never add new crates to `Cargo.toml` (`EditTool` Tier-2 normalizer explicitly uses a deterministic character mapping without `unicode-normalization` / `NFKC`).
- `SubagentTool` must use `ToolExecutionMode::Sequential` and must never expose `subagent` inside child tool sets (`depth == 1` hard cap).
- Mid-loop `tool_search` unlocks must become visible to the LLM on the very next turn inside the same `run_agent_loop` call.
- After every wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must exit `0`.

**Review Focus:**
- **Incremental `bash` streaming:** Verified by a test asserting >= 2 distinct `on_update` ticks arrive before process exit, plus partial output retained after timeout kill.
- **Ambiguity vs missing-match error in `edit`:** Verified by tests where Tier-2 matches 2 locations (fails with ambiguity error) and 0 locations (reports closest line snippet via `similar`).
- **Live tool unlock inside single loop:** Verified by `FauxProvider` calling `tool_search` on turn 1 and invoking the newly unlocked MCP mock tool cleanly on turn 2.
- **`subagent` isolation & cancellation:** Verified by tests confirming `"research"` omits `write`/`edit`/`bash`, both modes omit `subagent`, `max_turns` stops boundedly, and parent cancel aborts child loop.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Modify | `crates/mehh-tools/src/bash.rs` | Concurrent 4KB `stdout`/`stderr` reader tasks, 256KB rolling cap, 100ms throttled `on_update`, head+tail 30k snapshot truncation, timeout/cancel kill + partial output |
| Modify | `crates/mehh-tools/src/edit.rs` | Tier-2 `normalize_for_fuzzy_match`, line-window mapper to original byte offsets, `similar` closest-line error hint |
| Modify | `crates/mehh-agent/src/agent_loop.rs` | `AgentLoopConfig::dynamic_tools` snapshot per turn + `AgentLoopConfig::max_turns` guard |
| Create | `crates/mehh-coding-agent/src/tool_search.rs` | `DeferredToolMetadata`, `DeferredToolCatalog` (Okapi BM25 `k1=1.2, b=0.75`), `ToolSearchTool` |
| Modify | `crates/mehh-coding-agent/src/settings.rs` | Add `pub defer_mcp_tools: Option<bool>` + `effective_defer_mcp_tools(mcp_count)` |
| Create | `crates/mehh-coding-agent/src/subagent.rs` | `SubagentMode`, `SubagentTool` (`Sequential`, `research`/`coding` tool sets, `on_update`, structured `details`) |
| Modify | `crates/mehh-coding-agent/src/lib.rs` | Export `tool_search` and `subagent` |
| Modify | `crates/mehh-coding-agent/src/agent_session.rs` | Manage `shared_tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>`, optional `DeferredToolCatalog`, auto-defer MCP tools (`> 8` or `defer_mcp_tools`), wire `dynamic_tools`, register `SubagentTool` |
| Modify | `crates/mehh-coding-agent/src/interactive.rs` | Show active + deferred tool counts cleanly in `/tools` |

---

### Task 1: Streaming `BashTool` (`crates/mehh-tools/src/bash.rs`)

**Files:**
- Modify: `crates/mehh-tools/src/bash.rs`

**Interfaces:**
- **Consumes:** `AgentTool::execute(..., cancel, on_update)` from `mehh_agent`, `tokio::process::Command`
- **Produces:** Incremental `AgentToolResult::text` updates every `>= 100ms` plus bounded head+tail final output

- [ ] **Step 1: Write failing tests in `crates/mehh-tools/src/bash.rs`**

Add cross-platform tests (Unix `sh` / Windows `cmd` using the same OS command selection as existing `bash.rs`):
1. `test_bash_streams_incremental_updates`: run a command printing line 1, sleeping ~220ms, printing line 2, sleeping ~150ms, printing line 3; capture every `on_update` call in `Arc<Mutex<Vec<String>>>`. Assert `updates.len() >= 2`, early update contains line 1 without line 3, and final output contains all 3 lines.
2. `test_bash_timeout_preserves_partial_output`: run a command printing `"partial_before_sleep"` then sleeping 5s with `timeout = Some(1)` (1s); assert tool returns `Err` (or error result matching existing `BashTool` contract) whose message includes `"partial_before_sleep"` and `"timed out"`.

- [ ] **Step 2: Run targeted tests to verify RED**

Run: `cargo test -p mehh-tools test_bash_`
Expected: FAIL.

- [ ] **Step 3: Implement concurrent chunk reading + 100ms throttled updates**

Inside `BashTool::execute`:
- Spawn child with `Stdio::piped()` for `stdout` and `stderr`, `kill_on_drop(true)`.
- Maintain `RollingOutputBuffer` guarded by `Arc<std::sync::Mutex<RollingOutputBuffer>>` with a `256 * 1024` byte cap (trimming front half on overflow + marking truncated) and `snapshot_tail(MAX_OUTPUT_CHARS = 30_000) -> String`.
- Spawn two lightweight tokio tasks reading `stdout` and `stderr` in `4096`-byte chunks via `AsyncReadExt::read`, pushing UTF-8 lossy text into the shared rolling buffer and keeping a `DirtyFlag` (`AtomicBool`) or `Notify`.
- Run a driver loop selecting over:
  - `child.wait()` completion,
  - `cancel.cancelled()` -> kill child + await readers briefly -> return cancelled error with partial output,
  - `timeout_sleep` -> kill child + await readers -> return timeout error with partial output,
  - `flush_interval` (`100ms`) -> if output changed since last emission and `on_update` is `Some`, emit `AgentToolResult::text(buffer.snapshot())`.
- On completion, await both reader tasks so no trailing bytes are lost, emit one final update if flushed text changed, then format final output cleanly (`"(Command produced no output)"` / head+tail truncation matching existing `BashTool` tests).

- [ ] **Step 4: Run `mehh-tools` tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-tools`
`cargo clippy -p mehh-tools --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 2: Tolerant `EditTool` Tier-2 Fuzzy Matching (`crates/mehh-tools/src/edit.rs`)

**Files:**
- Modify: `crates/mehh-tools/src/edit.rs`

**Interfaces:**
- **Consumes:** `similar::TextDiff` (or line similarity ratio), normalized UTF-8 file text
- **Produces:** `normalize_for_fuzzy_match(s: &str) -> String`, two-tier match finder mapping back to byte offsets in `normalized_content`

- [ ] **Step 1: Write failing tests in `crates/mehh-tools/src/edit.rs`**

Add unit tests:
1. `test_edit_tier2_trailing_whitespace_and_smart_quotes`: file has `fn greet() {   \n    println!("hello—world\u{00A0}");\n}\r\n`; tool call passes ASCII quotes, `-` hyphen, regular space, no trailing spaces; Tier-2 applies replacement while preserving CRLF line endings!
2. `test_edit_tier2_rejects_ambiguous_fuzzy_matches`: file has two identical blocks differing only by trailing whitespace; fuzzy fallback sees `2` matches and returns an ambiguity error without modifying the file.
3. `test_edit_zero_match_reports_closest_line_hint`: target snippet has a typo on one token; error message contains `"Closest match"` and the candidate line numbers from the file.

- [ ] **Step 2: Run targeted tests to verify RED**

Run: `cargo test -p mehh-tools test_edit_tier2 test_edit_zero_match`
Expected: FAIL.

- [ ] **Step 3: Implement Tier-2 line-window matcher and closest-snippet hint**

Implement in `crates/mehh-tools/src/edit.rs`:
- Keep Tier-1 exact `normalized_content.match_indices(&old_norm)` untouched as the fast path.
- `pub fn normalize_for_fuzzy_match(input: &str) -> String`:
  - Map per char: `‘’‚‛` -> `'`, `“”„‟` -> `"`, `\u{2010}..\u{2015} | \u{2212}` -> `-`, `\u{00A0} | \u{2002}..\u{200A} | \u{202F} | \u{205F} | \u{3000}` -> `' '`.
  - Split lines and `trim_end_matches(|c| c == ' ' || c == '\t')` on each line (preserving leading indentation!).
- Track exact `(line_start_byte, line_end_byte)` spans in `normalized_content` (excluding `\n`). Compare sliding windows of `old_lines.len()` normalized lines against normalized `old_norm` lines. (Handle trailing `\n` consistently if `old_norm` ends with `\n`.)
- If `matches.len() == 1`, substitute exact byte range `(start_byte, end_byte)` cleanly before running the existing `apply_edits` / unified diff builder.
- If `matches.len() > 1`, fail with ambiguous match count (`matches.len()`).
- If `matches.is_empty()`, scan sliding windows of size `old_lines.len().max(1)` with `similar::utils::diff_slices` / line-level `TextDiff::from_lines` ratio, pick best candidate above threshold (`>= 0.45`), and append `Closest match near line N:\n...` to the error string.

- [ ] **Step 4: Run `mehh-tools` tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-tools`
`cargo clippy -p mehh-tools --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 3: Dynamic Mid-Loop Tools & Okapi BM25 `tool_search`

**Files:**
- Modify: `crates/mehh-agent/src/agent_loop.rs`
- Modify: `crates/mehh-coding-agent/src/settings.rs`
- Create: `crates/mehh-coding-agent/src/tool_search.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Consumes:** `AgentLoopConfig` in `mehh-agent`, MCP adapter tools from `McpManager` (`create_mcp_tool_name` namespace prefix `mcp__<server>__<tool>`), `AgentTool::parameters()` schema
- **Produces:** `AgentLoopConfig::dynamic_tools`, `Settings::defer_mcp_tools`, `DeferredToolCatalog`, `ToolSearchTool`

- [ ] **Step 1: Write failing tests across `mehh-agent` and `mehh-coding-agent`**

1. In `mehh-agent/src/agent_loop.rs`:
   - `test_agent_loop_sees_dynamic_tools_added_mid_loop`: configure `dynamic_tools = Some(Arc::new(RwLock::new(vec![tool_a])))` where `tool_a` inserts `tool_b` into `dynamic_tools`; `FauxProvider` inspects `context.tools` on turn 1 (sees only `tool_a`), calls `tool_a`, then on turn 2 inspects `context.tools` (sees `tool_a` + `tool_b`), calls `tool_b`, and receives `tool_b`'s result cleanly.
2. In `crates/mehh-coding-agent/src/tool_search.rs`:
   - `test_bm25_ranks_exact_and_schema_property_matches_above_noise`: index 12 mock MCP tools (`mcp__github__create_issue`, `mcp__slack__post_message`, `mcp__postgres__query`, etc.); assert `"create github issue"` puts `mcp__github__create_issue` at rank #1, and `"sql_statement"` matches schema property metadata on `mcp__postgres__query`.
   - `test_tool_search_unlocks_tools_into_shared_registry_without_duplicates`: calling `tool_search("github", Some(5))` adds `mcp__github__*` tools into shared `Arc<RwLock<Vec<Arc<dyn AgentTool>>>>`; calling a second time returns status already-active without duplicating entries in `active_tools`.
3. In `crates/mehh-coding-agent/src/agent_session.rs`:
   - `test_defer_mcp_tools_auto_activates_above_8_and_unlocks_mid_loop`: load 10 mock MCP tools onto `AgentSession`; verify initial `session.tools()` keeps built-ins + `tool_search` (and omits raw deferred MCP tools), then run `FauxProvider` calling `tool_search` -> turn 2 calling the unlocked MCP tool inside one `session.prompt` call!

- [ ] **Step 2: Run targeted tests to verify RED**

Run:
`cargo test -p mehh-agent dynamic_tools`
`cargo test -p mehh-coding-agent bm25 defer_mcp_tools`
Expected: FAIL.

- [ ] **Step 3: Implement `AgentLoopConfig::dynamic_tools` (`mehh-agent/src/agent_loop.rs`)**

- Add `pub dynamic_tools: Option<Arc<std::sync::RwLock<Vec<Arc<dyn AgentTool>>>>>` to `AgentLoopConfig` (default `None` in `AgentLoopConfig::new`).
- Helper `fn current_tools(config: &AgentLoopConfig) -> Vec<Arc<dyn AgentTool>>`: returns cloned `Vec` from `dynamic_tools.read().unwrap().clone()` when `Some`, else `config.tools.clone()`.
- At the top of each turn iteration in `run_agent_loop`, resolve `let turn_tools = current_tools(&config);` -> build `llm_context.tools` from `turn_tools`, and pass `&turn_tools` (or re-read before tool execution) into `execute_tool_calls`. Wait: if two calls share a batch, reading `current_tools(&config)` right before dispatching tool calls guarantees consistency with that assistant turn's declared tools AND handles late mutation cleanly.

- [ ] **Step 4: Implement `tool_search.rs`, `Settings::defer_mcp_tools`, and `AgentSession` wiring**

1. `crates/mehh-coding-agent/src/settings.rs`: add `pub defer_mcp_tools: Option<bool>` (`#[serde(default)]`) and `pub fn effective_defer_mcp_tools(&self, mcp_count: usize) -> bool { self.defer_mcp_tools.unwrap_or(mcp_count > 8) }`.
2. `crates/mehh-coding-agent/src/tool_search.rs`:
   - Parse MCP tool naming convention cleanly (`mcp__<server>__<local_name>`, falling back gracefully for resource tools like `list_mcp_resources`).
   - Extract index tokens (`lowercase`, split on non-alphanumeric + underscore/camelCase boundaries):
     - `name_tokens` (weight `3.0`)
     - `local_name_tokens` (weight `2.5`)
     - `desc_and_schema_tokens` (`tool.description()` + schema `properties` keys & nested `description` strings, weight `1.0`)
     - `server_tokens` (weight `0.5`)
   - Compute IDF + Okapi BM25 (`k1 = 1.2`, `b = 0.75`) plus exact/suffix match bonus (`+5.0` exact tool/local name, `+2.0` prefix match).
   - `ToolSearchTool`: holds `catalog: Arc<DeferredToolCatalog>`, `active_tools: Arc<std::sync::RwLock<Vec<Arc<dyn AgentTool>>>>`, `already_unlocked: Arc<Mutex<HashSet<String>>>`. Returns JSON `{ unlocked: [...], matches: [...] }` + human-readable names/descriptions (`limit` default `8`, clamped `1..=20`).
3. `crates/mehh-coding-agent/src/agent_session.rs`:
   - Add `pub shared_tools: Arc<std::sync::RwLock<Vec<Arc<dyn AgentTool>>>>` and `pub deferred_catalog: Option<Arc<DeferredToolCatalog>>` kept synchronized in `set_tools`, `enable_mcp_and_codemode`, and `prepare_prompt` (`config.dynamic_tools = Some(self.shared_tools.clone())`).
   - In `enable_mcp_and_codemode`:
     - Build `codemode_all_tools` first (if `enable_codemode`, `CodemodeTool::new` gets built-in tools + ALL `mcp_tools` so scripts see every MCP tool directly).
     - Decide deferral via `self.settings.effective_defer_mcp_tools(mcp_tools.len())`.
     - If deferred: separate MCP resource tools (`list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` stay active or deferred consistently — spec keeps deferred MCP tools in `DeferredToolCatalog`), push `ToolSearchTool` into `self.tools`, and sync `self.shared_tools`. If not deferred: extend `self.tools` directly as before.
     - Update `/tools` in `crates/mehh-coding-agent/src/interactive.rs` (or `session.deferred_catalog`) to list active tools plus count of deferred catalog tools.

- [ ] **Step 5: Run targeted tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-agent`
`cargo test -p mehh-coding-agent`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 4: Isolated `subagent` Tool & `max_turns` Loop Bound

**Files:**
- Modify: `crates/mehh-agent/src/agent_loop.rs`
- Create: `crates/mehh-coding-agent/src/subagent.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-cli/src/main.rs`

**Interfaces:**
- **Consumes:** `AgentLoopConfig::max_turns`, `mehh_tools::create_coding_tools`, `SessionManager::in_memory`
- **Produces:** `SubagentMode`, `SubagentTool`, `AgentLoopConfig::max_turns`

- [ ] **Step 1: Write failing tests in `mehh-agent` and `crates/mehh-coding-agent/src/subagent.rs`**

Add unit tests asserting:
1. `test_agent_loop_enforces_max_turns`: `FauxProvider` emits endless tool calls; with `max_turns = Some(2)`, `run_agent_loop` terminates after `2` assistant turns without hanging.
2. `test_subagent_research_mode_only_exposes_readonly_tools`: inspect child `context.tools` passed to `FauxProvider`; assert names are exactly `[read, grep, find, ls]` (`write`, `edit`, `bash`, and `subagent` are absent).
3. `test_subagent_coding_mode_exposes_coding_tools_without_subagent`: assert child tool names include `[read, write, edit, bash, grep, find, ls]` and do NOT include `subagent`.
4. `test_subagent_clamps_max_turns_and_propagates_parent_cancel`: `max_turns = 999` is clamped to `30`, `0` -> `1`; cancelling parent `CancellationToken` aborts running child loop immediately and sets `details["aborted"] == true`.
5. `test_subagent_emits_progress_updates_and_returns_structured_details`: child executes 1 `read` tool call then answers `"Summary OK"`; `on_update` captures `"[subagent] turn 1: read"`, final text is `"Summary OK"`, `details` has `mode`, `turns_used`, `total_tokens`, `cost`, `aborted`.

- [ ] **Step 2: Run targeted tests to verify RED**

Run:
`cargo test -p mehh-agent max_turns`
`cargo test -p mehh-coding-agent test_subagent_`
Expected: FAIL.

- [ ] **Step 3: Implement `AgentLoopConfig::max_turns` and `crates/mehh-coding-agent/src/subagent.rs`**

1. In `crates/mehh-agent/src/agent_loop.rs`:
   - Add `pub max_turns: Option<usize>` to `AgentLoopConfig` (default `None` in `AgentLoopConfig::new`).
   - Increment `turns_completed` per assistant turn; stop loop cleanly (`break`) when `config.max_turns.is_some_and(|cap| turns_completed >= cap)` even if the last assistant message had tool calls or follow-ups queued.
2. Create `crates/mehh-coding-agent/src/subagent.rs`:
   - `SubagentConfig { pub cwd: PathBuf, pub model: Model, pub provider: Arc<dyn LlmProvider>, pub api_key: Option<String>, pub thinking_level: Option<ThinkingLevel>, pub temperature: Option<f32>, pub max_tokens: Option<u32> }` — or `Arc<RwLock<SubagentRuntime>>` updated whenever `AgentSession::set_model` / `set_api_key` / `set_thinking_level` changes! Updating a shared runtime snapshot ensures switching `/model` in the parent session automatically updates what `subagent` uses too!
   - `#[async_trait] impl AgentTool for SubagentTool`:
     - `name(&self) -> "subagent"`
     - `execution_mode(&self) -> ToolExecutionMode::Sequential`
     - `parameters()`: schema with required `task` (`string`), optional `mode` (`enum: ["research", "coding"]`), `context` (`string`), `max_turns` (`integer`).
     - Select tools by filtering `mehh_tools::create_coding_tools(self.cwd.clone())`:
       - `"research"` -> `["read", "grep", "find", "ls"]`
       - `"coding"` -> `["read", "write", "edit", "bash", "grep", "find", "ls"]`
     - Build concise child system prompt via `build_system_prompt_with_skills` (excluding interactive bloat; instructing scoped focused return).
     - Derive `let child_cancel = cancel.child_token();`.
     - Attach `on_event` callback counting turns + forwarding `"[subagent] turn {turn}: {tool_name}"` via `on_update`, and accumulating `usage.input + usage.output` & `usage.cost.total`.
     - Run `run_agent_loop(vec![user_msg], vec![], child_config, Some(on_event)).await`.
     - Extract final assistant text (or aborted status error), return `AgentToolResult` with `details: Some(json!({ "mode": mode_str, "turns_used": turns_used, "total_tokens": total_tokens, "cost": total_cost, "aborted": aborted }))`.
3. Register `SubagentTool` cleanly inside `AgentSession` (synced on `set_model`, `set_api_key`, `set_thinking_level` via `Arc<RwLock<SubagentRuntime>>`) whenever default coding tools are active (preserving `--no-tools` in `main.rs` which clears tools). Note: keep `SubagentTool` excluded from `CodemodeTool` unless desired, OR include it cleanly outside `subagent`'s own child loop.

- [ ] **Step 4: Run full workspace tests & clippy**

Run:
`cargo test --workspace`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-11-13-wave-2-tools-search-and-subagent.md`. Two execution options:
1. **Subagent-Driven (`superpowers:subagent-driven-development`)** — dispatch one fresh coding subagent per task + two-stage spec/code review between tasks.
2. **Inline (`superpowers:executing-plans`)** — implement task-by-task directly in the current session with verification after each task.
