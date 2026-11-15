# Wave 5 — `mehh-agent`, `mehh-tools`, `mehh-coding-agent` & `mehh-cli` Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close behavioral gaps across the agent loop, tools, session tree, branch summarization, interactive slash commands, headless RPC, and CLI flags/file expansion while keeping `AgentSession` calling `run_agent_loop` directly and keeping `PowerShellTool` opt-in on Windows.

**Architecture:** `mehh-agent` adds `duration_ms`, async tool hooks with `AgentContext`/`structured_content`/`usage`, turn lifecycle hooks, and reactive `AgentState`. `mehh-tools` adds `FileMutationQueue` around `EditTool`/`WriteTool`, Windows UTF-8/exit-code fixes in `BashTool`, and opt-in `PowerShellTool`. `mehh-coding-agent` and `mehh-cli` complete the 4 missing `SessionEntryKind` variants, `branch_summarization.rs`, 8 slash commands, 11 RPC commands, missing CLI flags (`--approve`/`--no-approve`, `--export`, `--tools`, etc.), and `@file` expansion.

**Tech Stack:** Rust workspace crates (`tokio`, `tokio-util`, `serde`, `serde_json`, `futures`, `clap`, `anyhow` — zero new external crates; OS clipboard via `clipboard-command` subprocess + OSC 52).

**Spec:** [2026-11-15-ts-parity-waves-4-5-6-design.md](../../specs/2026-11-15-ts-parity-waves-4-5-6-design.md)

## Global Constraints

- Zero new external Cargo crates; platform clipboard uses OS commands (`powershell.exe Set-Clipboard` / `clip`, `pbcopy`, `wl-copy`/`xclip`/`xsel`) + OSC 52 fallback.
- `BashTool` stays mapped to `powershell` on Windows (with TS UTF-8 + exit-code suffix); `PowerShellTool` (`"powershell"`) is opt-in via `--tools powershell` / `create_all_tools(cwd)` and fails cleanly on non-Windows.
- `--trust` aliases `--approve` (ephemeral session trust); `--no-approve` forces `trusted = false` for that run even when `trust.json` holds `Trusted` (neither mutates `trust.json`).
- Keep `AgentSession` calling `run_agent_loop` directly; RPC `fork`/`clone`/`switch_session` swap/create `SessionManager` in place on `&mut AgentSession`.
- Remove unused `thiserror` dependency warning from `crates/mehh-coding-agent/Cargo.toml` so `cargo clippy --workspace --all-targets -- -D warnings` exits `0`.

## Review Focus

- **`before_tool_call` blocks a tool call — `duration_ms` must be `None`:** Covered by `agent_loop::tests::blocked_tool_call_omits_duration_ms`.
- **`FileMutationQueue` serializes same-file writes even across mixed Windows casing or relative/canonical paths while running distinct files concurrently:** Covered by `file_mutation_queue::tests::same_file_writes_serialize_different_files_parallel`.
- **`ContextEdit` with `replacement: None` hides target entry from LLM context while keeping JSONL tree intact:** Covered by `session::tests::context_edit_hides_and_replaces_without_mutating_tree`.
- **CLI `--no-approve` overrides a `Trusted` entry in `trust.json` without mutating disk:** Covered by `main::tests::no_approve_overrides_persisted_trust_without_writing_disk`.
- **RPC `get_tree` returns nested recursive children with `Label` bookmarks intact:** Covered by `rpc::tests::get_tree_returns_nested_nodes_with_labels`.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Modify | `crates/mehh-ai/src/types.rs` | Add optional `duration_ms: Option<u64>` and `usage: Option<Usage>` on `ToolResultMessage` |
| Modify | `crates/mehh-agent/src/types.rs` | `AgentState`, richer `AgentToolResult`, async `BeforeToolCallHook`/`AfterToolCallHook`, `AgentContext`, turn lifecycle hook types, `ToolExecutionEnd { duration_ms }` |
| Modify | `crates/mehh-agent/src/agent_loop.rs` | `Instant::now()` `duration_ms`, async tool hook execution + merge rules, `prepare_request`/`prepare_next_turn`/`transform_context`/`finish_turn` hooks |
| Modify | `crates/mehh-agent/src/agent.rs` | Reactive `AgentState` tracking, `Agent::state(&self)`, `Agent::reset(&self)`, async hook builders |
| Create | `crates/mehh-tools/src/file_mutation_queue.rs` | `with_file_mutation_queue` keyed by normalized canonical path |
| Create | `crates/mehh-tools/src/powershell.rs` | Opt-in `PowerShellTool` (`"powershell"`) with UTF-8 preamble + PS 5.1 exit suffix |
| Modify | `crates/mehh-tools/src/bash.rs` | Add TS UTF-8 preamble & PS 5.1 exit-code suffix on Windows |
| Modify | `crates/mehh-tools/src/edit.rs` & `write.rs` | Wrap mutations in `with_file_mutation_queue` |
| Modify | `crates/mehh-tools/src/lib.rs` | Export `file_mutation_queue`, `powershell`, and `create_all_tools` + tool filter builder |
| Modify | `crates/mehh-coding-agent/Cargo.toml` | Remove unused `thiserror` dependency |
| Modify | `crates/mehh-coding-agent/src/session.rs` | Add `Usage`, `CustomMessage`, `ContextEdit`, `Label` entries; `project_context_entry`; `set_session_name`; recursive `RpcSessionTreeNode` |
| Create | `crates/mehh-coding-agent/src/branch_summarization.rs` | `collect_entries_for_branch_summary`, `prepare_branch_entries`, `generate_branch_summary` |
| Modify | `crates/mehh-coding-agent/src/lib.rs` & `agent_session.rs` | Export `branch_summarization`; wire `navigate_tree` with optional auto `BranchSummary` & `SessionManager` helpers |
| Create | `crates/mehh-coding-agent/src/clipboard.rs` | Platform clipboard writer (`powershell.exe`/`clip`, `pbcopy`, `wl-copy`/`xclip`/`xsel`) + OSC 52 fallback |
| Modify | `crates/mehh-coding-agent/src/interactive.rs` | Slash commands `/copy`, `/name`, `/session`, `/hotkeys`, `/reload`, `/scoped-models`, `/export`, `/import`; `/tree` summarize option |
| Modify | `crates/mehh-coding-agent/src/rpc.rs` | 11 RPC commands (`steer`, `follow_up`, `clear_queue`, `cycle_model`, `bash`, `abort_bash`, `fork`, `clone`, `switch_session`, `get_tree`, `set_session_name`) |
| Modify | `crates/mehh-coding-agent/src/trust.rs` | Support `TrustOverride::{None, Approve, NoApprove}` in `resolve_project_trust_with_override` |
| Modify | `crates/mehh-cli/src/main.rs` | Missing `CliArgs` flags, `--approve`/`--no-approve`/`--trust`, `--export <input> [output]`, `@file` positional expansion |

---

### Task 1: `mehh-agent` `duration_ms` Timing & Reactive `AgentState`

**Files:**
- Modify: `crates/mehh-ai/src/types.rs:285-296`
- Modify: `crates/mehh-agent/src/types.rs`
- Modify: `crates/mehh-agent/src/agent_loop.rs`
- Modify: `crates/mehh-agent/src/agent.rs`

**Interfaces:**
- **Consumes:** `mehh_ai::types::{ToolResultMessage, Usage}`
- **Produces:**
  - `ToolResultMessage.{duration_ms, usage}`
  - `AgentEvent::ToolExecutionEnd { ..., duration_ms: Option<u64> }`
  - `pub struct AgentState { pub system_prompt: String, pub model: Model, pub thinking_level: Option<ThinkingLevel>, pub tools: Vec<Arc<dyn AgentTool>>, pub messages: Vec<Message>, pub is_streaming: bool, pub streaming_message: Option<Message>, pub pending_tool_calls: HashSet<String>, pub error_message: Option<String> }`
  - `Agent::state(&self) -> AgentState`, `Agent::reset(&self)`

- [ ] **Step 1: Write failing tests in `crates/mehh-agent/src/agent_loop.rs` and `agent.rs`**

Assert:
1. `agent_loop::tests::executed_tool_call_records_duration_ms`: running a tool populates `Some(ms)` on both `AgentEvent::ToolExecutionEnd.duration_ms` and persisted `ToolResultMessage.duration_ms`.
2. `agent_loop::tests::blocked_tool_call_omits_duration_ms`: when `before_tool_call` blocks execution, `duration_ms` is `None`.
3. `agent::tests::agent_state_tracks_streaming_pending_tools_and_reset`: verifies `is_streaming`, `pending_tool_calls`, `error_message`, and `reset()` restoring initial idle state.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-agent duration_ms agent_state`
Expected: FAIL.

- [ ] **Step 3: Implement `duration_ms` and `AgentState`**

Add `duration_ms`/`usage` fields to `ToolResultMessage` and `ToolExecutionEnd`; record `Instant::now()` inside `execute_single_tool` after `before_tool_call` allows execution; track `AgentState` via interior `Arc<std::sync::RwLock<AgentState>>` updated from emitted `AgentEvent`s in `Agent`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-agent`
Expected: PASS.

---

### Task 2: Async Tool Hooks (`structured_content`, `usage`, `AgentContext`) & Turn Lifecycle Hooks

**Files:**
- Modify: `crates/mehh-agent/src/types.rs`
- Modify: `crates/mehh-agent/src/agent_loop.rs`
- Modify: `crates/mehh-agent/src/agent.rs`

**Interfaces:**
- **Consumes:** `futures::future::BoxFuture`, `tokio_util::sync::CancellationToken`
- **Produces:**
  - `AgentToolResult` + `AfterToolCallResult` fields: `structured_content: Option<Value>`, `usage: Option<Usage>`, `is_error: Option<bool>`, `terminate: Option<bool>`
  - Async `BeforeToolCallHook`, `AfterToolCallHook`, `FinishTurnHook`, `PrepareRequestHook`, `PrepareNextTurnHook`, `TransformContextHook` on `AgentLoopConfig`

- [ ] **Step 1: Write failing tests in `crates/mehh-agent/src/agent_loop.rs`**

Assert:
1. `async_tool_hooks_receive_context_and_override_structured_content_usage`: `before_tool_call` / `after_tool_call` run asynchronously with `context: Vec<Message>`, override `structured_content` and `usage`, and clear `structured_content` when `content` is replaced without `structured_content` (TS `finalizeExecutedToolCall` rule).
2. `turn_lifecycle_hooks_control_request_context_and_finish_decision`: `transform_context` prunes history, `prepare_request` swaps model/context before provider call, `finish_turn` forces continuation on `Continue` or exits on `End`, and `prepare_next_turn` injects turn updates before the next iteration.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-agent async_tool_hooks turn_lifecycle_hooks`
Expected: FAIL.

- [ ] **Step 3: Implement async tool hooks & turn lifecycle hooks**

Update `BeforeToolCallContext` / `AfterToolCallContext` / `AgentToolResult` / `AfterToolCallResult` / `AgentLoopConfig`, wire them at the exact TS turn boundaries in `run_agent_loop`, and update any internal callers/tests in `mehh-agent`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-agent`
Expected: PASS.

---

### Task 3: `FileMutationQueue`, Windows `BashTool` Hardening & Opt-in `PowerShellTool`

**Files:**
- Create: `crates/mehh-tools/src/file_mutation_queue.rs`
- Create: `crates/mehh-tools/src/powershell.rs`
- Modify: `crates/mehh-tools/src/bash.rs`
- Modify: `crates/mehh-tools/src/edit.rs`
- Modify: `crates/mehh-tools/src/write.rs`
- Modify: `crates/mehh-tools/src/lib.rs`

**Interfaces:**
- **Consumes:** `tokio::sync::Mutex`, `mehh_agent::AgentTool`
- **Produces:**
  - `mehh_tools::file_mutation_queue::with_file_mutation_queue`
  - `mehh_tools::powershell::PowerShellTool`
  - `mehh_tools::create_all_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>>` and `select_coding_tools(cwd: PathBuf, include: Option<&[String]>, exclude: &[String]) -> anyhow::Result<Vec<Arc<dyn AgentTool>>>`

- [ ] **Step 1: Write failing tests in `crates/mehh-tools/src/`**

Assert:
1. `file_mutation_queue::tests::same_file_writes_serialize_different_files_parallel`: two concurrent tasks mutating the same file (via `./foo.txt` and canonical path) execute strictly serially without interleaving, while two tasks on distinct files run concurrently.
2. `edit::tests::concurrent_edits_on_same_file_do_not_clobber`: two parallel `EditTool` calls on different sections of the same file both succeed cleanly.
3. `powershell::tests::powershell_tool_executes_on_windows_and_handles_native_stderr`: on Windows, preserves UTF-8 output and succeeds when a native command writes to stderr with `$LASTEXITCODE == 0`; on non-Windows, returns explicit unsupported-platform `Err`.
4. `lib::tests::create_coding_tools_excludes_powershell_while_create_all_tools_includes_it`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-tools`
Expected: FAIL.

- [ ] **Step 3: Implement `file_mutation_queue.rs`, `powershell.rs`, `bash.rs` Windows fix, and `lib.rs` helpers**

Normalize queue keys (`canonicalize` fallback lexical normalization, lowercased under `cfg!(windows)`), wrap `EditTool::execute` and `WriteTool::execute` in `with_file_mutation_queue`, apply the TS UTF-8 preamble + PS 5.1 exit-code suffix to both `PowerShellTool` and Windows `BashTool`, and keep `create_coding_tools` returning the 7 default tools while `create_all_tools`/`select_coding_tools` expose `powershell`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-tools`
Expected: PASS.

---

### Task 4: Missing 4 `SessionEntryKind` Variants & Projection (`session.rs`)

**Files:**
- Modify: `crates/mehh-coding-agent/Cargo.toml:22` (remove unused `thiserror`)
- Modify: `crates/mehh-coding-agent/src/session.rs`

**Interfaces:**
- **Consumes:** `mehh_ai::types::{Message, Usage, TextContent, ImageContent, UserContent}`
- **Produces:**
  - `SessionEntryKind::{Usage, CustomMessage, ContextEdit, Label}`
  - `SessionManager::{append_usage, append_custom_message, append_context_edit, append_label, set_session_name, build_rpc_tree}`
  - `pub struct RpcSessionTreeNode`

- [ ] **Step 1: Write failing tests in `crates/mehh-coding-agent/src/session.rs`**

Assert:
1. `usage_custom_message_context_edit_and_label_roundtrip`: serializes/deserializes all 4 variants in JSONL v3 format matching TS camelCase field names.
2. `context_edit_hides_and_replaces_without_mutating_tree`: `ContextEdit { target_id, replacement: None }` omits target entry from `build_context_messages`; `replacement: Some(..)` replaces visible content; `CustomMessage { display, .. }` projects into `build_context_messages`.
3. `label_entries_track_latest_bookmark_per_target`: setting and clearing (`None`) labels updates `build_rpc_tree()` node labels cleanly.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-coding-agent session::tests`
Expected: FAIL.

- [ ] **Step 3: Implement the 4 variants, projection, and recursive `RpcSessionTreeNode`**

Update `SessionEntryKind`, `SessionManager` append helpers, `build_context_messages` projection, `set_session_name`, and `build_rpc_tree()`, and remove the unused `thiserror` line from `crates/mehh-coding-agent/Cargo.toml`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-coding-agent session::tests`
Expected: PASS.

---

### Task 5: Automatic `BranchSummary` Generation (`branch_summarization.rs`)

**Files:**
- Create: `crates/mehh-coding-agent/src/branch_summarization.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**
- **Consumes:** `SessionManager`, `SessionEntry`, `mehh_ai::LlmProvider`, `compaction::estimate_context_tokens`
- **Produces:**
  - `branch_summarization::{collect_entries_for_branch_summary, prepare_branch_entries, generate_branch_summary, BRANCH_SUMMARY_PROMPT, BRANCH_SUMMARY_PREAMBLE}`
  - `AgentSession::navigate_tree(&mut self, target_id: &str, summarize: bool, custom_instructions: Option<&str>) -> anyhow::Result<Option<String>>`

- [ ] **Step 1: Write failing tests in `branch_summarization.rs` and `agent_session.rs`**

Assert:
1. `collect_entries_for_branch_summary_stops_at_common_ancestor`: collects only abandoned branch entries between `old_leaf_id` and deepest common ancestor with `target_id`.
2. `prepare_branch_entries_trims_to_budget_and_extracts_file_ops`: fits within `context_window - reserve_tokens` and preserves read/edited paths.
3. `navigate_tree_with_summarize_generates_and_appends_branch_summary`: using `FauxProvider`, switching branches with `summarize = true` appends `SessionEntryKind::BranchSummary { from_id, summary }` and switches leaf to target branch.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-coding-agent branch_summarization navigate_tree_with_summarize`
Expected: FAIL.

- [ ] **Step 3: Implement `branch_summarization.rs` and wire `AgentSession::navigate_tree` + interactive `/tree`**

Port TS prompts, entry collection, budget trimming, and LLM call; replace direct `session.session_mut().branch_from(entry_id)` calls inside `/tree` (`interactive.rs:1008-1049`) with `AgentSession::navigate_tree` supporting both explicit `--summarize` and selector branch-switch confirmation when abandoning entries.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-coding-agent branch_summarization navigate_tree`
Expected: PASS.

---

### Task 6: Missing 8 Slash Commands & 11 RPC Commands

**Files:**
- Create: `crates/mehh-coding-agent/src/clipboard.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs:685-950`
- Modify: `crates/mehh-coding-agent/src/rpc.rs:179-460`

**Interfaces:**
- **Consumes:** `SessionManager::{fork_at, clone_at_leaf, build_rpc_tree, set_session_name}`, `AgentSession::{steer, follow_up, clear_queues, set_model, rebuild_system_prompt}`, `Keybindings`, `BashTool`
- **Produces:**
  - `clipboard::copy_to_clipboard(text: &str) -> anyhow::Result<()>` (platform command + OSC 52 fallback)
  - Slash handlers `/copy`, `/name`, `/session`, `/hotkeys`, `/reload`, `/scoped-models`, `/export`, `/import`
  - RPC handlers `steer`, `follow_up`, `clear_queue`, `cycle_model`, `bash`, `abort_bash`, `fork`, `clone`, `switch_session`, `get_tree`, `set_session_name`

- [ ] **Step 1: Write failing tests in `interactive.rs` and `rpc.rs`**

Assert:
1. `interactive::tests::slash_name_session_hotkeys_scoped_models_export_import`: `/name Demo` persists `SessionInfo`; `/session` reports counts/tokens/cost; `/hotkeys` renders `Keybindings` rows; `/scoped-models` updates cycling list; `/export <tmp>` + `/import <tmp>` roundtrips JSONL cleanly; `/reload` reloads settings/skills respecting trust; `/copy` formats/dispatches clipboard helper cleanly without panicking on headless test.
2. `rpc::tests::rpc_queue_session_tree_and_bash_commands`: tests `steer`, `follow_up`, `clear_queue`, `cycle_model`, `bash` + `abort_bash`, `fork`, `clone`, `switch_session`, `get_tree` (nested `RpcSessionTreeNode` with `Label`s), and `set_session_name`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-coding-agent slash_name_session rpc_queue_session_tree`
Expected: FAIL.

- [ ] **Step 3: Implement `clipboard.rs`, slash command handlers, and RPC command arms**

Implement platform clipboard runner (without blocking Tokio runtime), JSONL export/import helpers shared between `/export` and CLI `--export`, update `builtin_slash_commands()` in `interactive.rs:144` with the 8 new commands, hold resolved scoped models cleanly so `/scoped-models`, RPC `cycle_model`, and CLI `--models` share the same cycling list, update `AgentSession` in-place on RPC `fork`/`clone`/`switch_session` (`session.session = ...`), and implement all 8 slash + 11 RPC handlers.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-coding-agent`
Expected: PASS.

---

### Task 7: CLI Missing Flags, Trust Override & `@file` Expansion + Wave 5 Gate

**Files:**
- Modify: `crates/mehh-coding-agent/src/trust.rs`
- Modify: `crates/mehh-cli/src/main.rs`

**Interfaces:**
- **Consumes:** `select_coding_tools`, `SessionManager::{fork_at, export_jsonl}`, `UserContentBlock`
- **Produces:**
  - `TrustOverride::{None, Approve, NoApprove}` + `resolve_project_trust_with_override`
  - `CliArgs` flags (`--name`, `--session-id`, `--session-dir`, `--fork`, `--models`, `--tools`, `--exclude-tools`, `--no-builtin-tools`, `--export`, `--skills`, `--no-skills`, `--prompt-templates`, `--no-prompt-templates`, `--use-theme`, `--no-themes`, `--no-context-files`, `--verbose`, `--approve`, `--no-approve`, `--trust` alias)
  - `process_file_arguments(cwd: &Path, args: &[String]) -> anyhow::Result<ProcessedPromptArgs>`

- [ ] **Step 1: Write failing tests in `trust.rs` and `crates/mehh-cli/src/main.rs`**

Assert:
1. `trust::tests::no_approve_overrides_persisted_trust_without_writing_disk`: saved `Trusted` repo resolves `trusted = false` under `TrustOverride::NoApprove` while `trust.json` remains `Trusted`; `--approve` / `--trust` resolves `trusted = true` ephemerally.
2. `main::tests::cli_rejects_conflicting_approve_and_no_approve` and `cli_parses_export_input_and_optional_output`.
3. `main::tests::process_file_arguments_expands_at_text_with_bom_strip_and_at_images`: `@notes.txt` with UTF-8 BOM becomes `<file name="...">...</file>` text and `@img.png` becomes `UserContentBlock::Image` while non-`@` words remain prompt text.
4. `main::tests::tool_selection_flags_respect_tools_exclude_tools_and_no_builtin_tools` (including `--tools powershell`).

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-coding-agent no_approve_overrides && cargo test -p mehh-cli`
Expected: FAIL.

- [ ] **Step 3: Implement trust overrides, `CliArgs` flags, `--export`, tool/resource filtering, and `@file` expansion**

Wire `--export <input> [output]` before session creation, wire `--name`/`--session-id`/`--session-dir`/`--fork`/`--models`/`--tools`/`--exclude-tools`/`--no-builtin-tools`/skills/prompt-templates/theme/context-file/verbose flags, and parse `@file` positional arguments cleanly.

- [ ] **Step 4: Run Wave 5 tests & full workspace verification gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (exit code 0, zero warnings).
