# TypeScript-to-Rust Parity — Waves 4, 5, and 6 Design Specification

**Date:** 2026-11-15
**Reference:** `karmavn/Mehhcode@ts-reference` (`packages/*`)
**Target:** `crates/*` (`mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-mcp`, `mehh-tui`, `mehh-codemode`, `mehh-cli`)
**Baseline:** `cargo check --workspace` passes cleanly (`rustc 1.100.0`, 1 unused-dependency warning on `thiserror` in `crates/mehh-coding-agent/Cargo.toml`)

---

## 1. Intent and Goals

Close the high-leverage behavioral, protocol, and UX gaps between the Pi TypeScript reference (`packages/*`) and the Rust workspace (`crates/mehh-*`) on top of the Wave 1–3 foundation (`2026-11-13-strongest-rust-agent-design.md`).

### Goals
- **Wave 4 (`mehh-ai` Provider, Streaming & Auth Parity):** Prevent cross-model 400 API errors, handle image/thinking/tool-call compatibility across all 6 providers (`transform_messages`, `split_error_tool_result_images`), support Anthropic prompt caching (`cache_control`), adaptive thinking/effort budgets, and OAuth credential mimicry/locking, plus incremental tool JSON parsing and unpaired surrogate sanitization.
- **Wave 5 (`mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-cli` Parity):** Add tool timing (`duration_ms`), async tool/turn lifecycle hooks, reactive `AgentState`, serialized file writes (`with_file_mutation_queue`), `PowerShellTool`, full session tree entry kinds (`Usage`, `CustomMessage`, `ContextEdit`, `Label`), automatic `BranchSummary` generation, and essential CLI flags (`@file`, `--fork`, `--tools`, `--models`), slash commands (`/copy`, `/name`, `/session`, `/hotkeys`, `/reload`, `/scoped-models`, `/export`, `/import`), and RPC commands (`steer`, `follow_up`, `bash`, `fork`, `clone`, `switch_session`, `get_tree`, etc.).
- **Wave 6 (`mehh-mcp` & `mehh-tui` Ergonomics Parity):** Add MCP cancellation (`notifications/cancelled`), progress reporting + timeout renewal (`notifications/progress`), disk persistence for oversized outputs and binary blobs (`McpOutputSaver`), dynamic `tools/list_changed` refresh, plus TUI `fuzzy_match`/`fuzzy_filter`, multi-entry `KillRing`, punctuation-aware word navigation, `WheelScrollAccelerator`, and 2-column `SettingsList`.

### Non-Goals
- Porting TS-only infrastructure already excluded in `2026-11-13-strongest-rust-agent-design.md`: `packages/telemetry` (OpenTelemetry), `packages/schema` (TypeBox type-level builder), standalone HTML export bundle (`export-html` vendor JS/CSS), third-party `package-manager` (`npm`/`git` installs), in-process JS/TS extension runtime (`core/extensions/`), and `extensions/llama`.
- Porting JS/edge-specific runtimes (`bun/`, `cloudflare.ts`, `quickjs-wasi`, `node-http-proxy.ts`) or proprietary `radius` backend integrations (`radius.ts`, `radius-mcp.ts`, `bug-report-upload.ts`).
- Full 1,800-line `TuiAltScreen` dual-screen engine rewrite (mouse drag text selection and `alt-screen-search`) or `mehh-codemode` `.d.ts` generator / deterministic VM replay (kept deferred unless requested separately).
- Importing the 40+ provider `models.generated.ts` catalog into `catalog.rs` (Wave 1's `models.json` `ModelRegistry` already handles custom/external models; additional provider presets can be added incrementally).

### Anti-Goals
- Leaving newly ported helpers unwired from their Rust callers or duplicating existing normalizers (`normalize_mistral_tool_call_id`, `openai_responses.rs` inline `split('|')`) alongside parallel implementations.
- Swallowing internal serialization, filesystem, or lock errors via `.ok()` or `unwrap_or_default()` in violation of `AGENTS.md`.
- Adding new external Cargo crates for file locking or clipboard access when `rustc 1.100.0` `std::fs::File::lock` and OS clipboard commands cover both cleanly.

---

## 2. Approaches Considered

1. **`mehh-ai` Only (`transform_messages`, `cache_control`, `oauth` lock):** Fixes provider compatibility and Anthropic cost first, leaving `AgentState`, `file-mutation-queue`, `BranchSummary`, CLI/RPC commands, and MCP cancellation/progress unaddressed. *Rejected as standalone scope* because the user requested porting the remaining necessary TS features across the workspace.
2. **`mehh-coding-agent` + `mehh-tools` Only (Session, CLI, Tools):** Improves interactive ergonomics first, leaving cross-model conversation corruption and missing Anthropic prompt caching in `mehh-ai`. *Rejected as standalone scope.*
3. **Sequential 3-Wave Parity Plan in One Spec (Chosen):** Research and specify **Wave 4 (`mehh-ai`) → Wave 5 (`mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-cli`) → Wave 6 (`mehh-mcp`, `mehh-tui`)** in one unified spec, then execute and verify each wave independently via `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`. Matches the `2026-11-13-strongest-rust-agent-design.md` workflow and avoids repeated subagent research overhead.

---

## 3. Global Constraints

- **Zero new external Cargo dependencies:** Implement all three waves using existing `Cargo.toml` workspace dependencies plus Rust `1.100.0` `std::fs::File::lock` / `try_lock` and platform OS utilities (e.g., `clip.exe`/`powershell.exe`, `pbcopy`, `wl-copy`/`xclip`/`xsel`).
- **Existing helper reuse & cleanup:** Wire new abstractions into every call site and remove superseded inline hacks (`mistral.rs` inline `tc`/`tr` normalization, `openai_responses.rs` inline `split('|').next()`).
- **Backward-compatible Serde evolution:** All new fields on shared wire/config structs (`Model`, `StreamOptions`, `ToolResultMessage`, `AgentEvent`, `AgentToolResult`) use `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing cross-crate callers compile without breaking changes.
- **Fail-closed & error propagation (`AGENTS.md`):** Distinguish `ENOENT` from syntax/schema errors when loading JSON; strip UTF-8 BOM on read and write UTF-8 without BOM; never swallow internal errors.
- **Verification gate per wave:** After each wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with 0 failures and 0 warnings (cleaning up the existing `unused dependency thiserror` warning in `crates/mehh-coding-agent/Cargo.toml` in Wave 5).

---

## 4. Wave 4 — `mehh-ai` Provider, Streaming & Auth Parity

### 4.1 `transform_messages` & `split_error_tool_result_images` (`crates/mehh-ai/src/transform_messages.rs`)
- **Two-pass transformation (`transform_messages(messages, model, normalize_tool_call_id)`):**
  - **Image downgrade (`downgrade_unsupported_images`):** When `!model.input.iter().any(|m| m == "image")`, replace `Image` blocks in `UserContent::Blocks` (`"(image omitted: model does not support images)"`) and `ToolResultMessage.content` (`"(tool image omitted: model does not support images)"`), collapsing consecutive images and skipping duplicate placeholders if the preceding text block already equals the placeholder.
  - **Pass 1 (Model mismatch & ID mapping):** Compute `is_same_model = msg.provider == model.provider && msg.api == model.api && msg.model == model.id`.
    - `Thinking`: Drop when `!is_same_model`. When `is_same_model`, retain if `redacted == Some(true) || thinking_signature.is_some()`, drop if `thinking.trim().is_empty()`.
    - `Text`: Clear `text_signature = None` when `!is_same_model`.
    - `ToolCall`: Clear `thought_signature = None` when `!is_same_model`. If `normalize_tool_call_id` is `Some` and `!is_same_model`, record `old_id -> new_id` in `tool_call_id_map` and update `tc.id`.
    - `ToolResult`: Rewrite `tr.tool_call_id` if present in `tool_call_id_map`.
  - **Pass 2 (Turn repair & orphaned calls):**
    - Skip `AssistantMessage` turns with `StopReason::Error | StopReason::Aborted`.
    - Track `pending_tool_calls` and `existing_tool_result_ids`. Buffer `Message::System` arriving while tool calls are pending into `held_system_messages` and emit after tool results resolve.
    - Before a subsequent `Assistant` or `User` message, or at conversation end, insert synthetic error `ToolResultMessage` (`content: [Text("No result provided")]`, `is_error: true`, `timestamp: now_ms()`) for any unanswered `pending_tool_calls`.
- **`split_error_tool_result_images(msg: &ToolResultMessage) -> Option<SplitErrorToolResult>`:**
  - Active only when `msg.is_error` and at least one `ImageContent` exists.
  - Replaces image runs with `"(image attached after the tool results)"` and outputs `caption: format!("Images from the error result of tool call {} ({}):", msg.tool_call_id, msg.tool_name)` plus `images` to append after all consecutive tool results in the user turn.
- **Caller wiring across all 6 provider builders:**
  - `crates/mehh-ai/src/anthropic.rs` (`build_anthropic_request`): normalize IDs (`[^a-zA-Z0-9_-]` -> `_`, max 64 chars) + `split_error_tool_result_images`.
  - `crates/mehh-ai/src/bedrock.rs` (`build_bedrock_request`): normalize IDs (max 64 chars) + `split_error_tool_result_images` when `model.id.contains("claude")`.
  - `crates/mehh-ai/src/google.rs` (`build_google_request`): normalize IDs when `requires_tool_call_id(&model.id)` (`claude-*`, `gpt-oss-*`, or Gemini major `>= 3`).
  - `crates/mehh-ai/src/mistral.rs` (`build_mistral_request`): upgrade existing `normalize_mistral_tool_call_id` into collision-resistant stateful `MistralToolCallIdNormalizer` (`id_map`/`reverse_map`, 9-char alphanumeric `shortHash` fallback) passed to `transform_messages`, and remove the separate inline `tc`/`tr` normalizations at lines 133 and 152.
  - `crates/mehh-ai/src/openai.rs` (`build_openai_request`): normalize pipe-separated Responses IDs (`call_id|item_id` -> `call_item` or `prefix_hash` <= 40 chars; truncate `openai` IDs > 40 chars).
  - `crates/mehh-ai/src/openai_responses.rs` (`build_openai_responses_request`, shared with `azure_openai.rs`): normalize `{call_id}|fc_{hash}` for foreign calls, removing the ad-hoc `tc.id.split('|').next()` hacks at lines 256 and 269.

### 4.2 Anthropic Prompt Caching, Thinking & OAuth Mimicry (`crates/mehh-ai/src/anthropic.rs`, `types.rs`)
- **Struct extensions in `types.rs`:**
  - Add `ModelCompat` (`supports_long_cache_retention`, `supports_cache_control_on_tools`, `force_adaptive_thinking`, `supports_mid_convo_effort`, `supports_temperature`, `allow_empty_signature`, `supports_strict_tools`) and `compat: Option<ModelCompat>`, `thinking_level_map: Option<HashMap<String, Option<String>>>` to `Model`.
  - Add `CacheRetention` (`None | Short | Long`), `AnthropicEffort` (`Low | Medium | High | Xhigh | Max`), `AnthropicThinkingDisplay` (`Summarized | Omitted`), `cache_retention`, `thinking_budget_tokens`, `effort`, `thinking_display`, `interleaved_thinking` to `StreamOptions`.
- **`anthropic.rs` & `bedrock.rs` caching + headers:**
  - Resolve `cache_retention` (`options.cache_retention` -> `PI_CACHE_RETENTION` env -> `Short`); attach `cache_control: { "type": "ephemeral", ... }` (`ttl: "1h"` when `Long` and compat permits) to system prompt blocks, last tool definition (when compat permits), and the last block of the final user/system message.
  - Aggregate `anthropic-beta` headers dynamically (merging `model.headers` / `options.headers` with OAuth betas `claude-code-20250219,oauth-2025-04-20`, `fine-grained-tool-streaming-2025-05-14`, `interleaved-thinking-2025-05-14`, `mid-conversation-output-config-2026-07-01`, `thinking-binding-controls-2026-08-01`).
  - Support `ThinkingBudgets` + `clamp_thinking_budget_to_answer_room` (`MIN_ANSWER_TOKENS = 1024`), adaptive thinking (`type: "adaptive"`, `display`), `output_config.effort`, and explicit `type: "disabled"` when reasoning is off.
  - Detect `is_oauth_token(api_key)` (`api_key.contains("sk-ant-oat")`): switch from `x-api-key` to `Authorization: Bearer`, inject Claude Code CLI identity headers (`anthropic-dangerous-direct-browser-access`, `user-agent`, `x-app`) and system prompt prefix, and map canonical tool names (`to_claude_code_name` / `from_claude_code_name`).

### 4.3 Incremental Tool JSON, Surrogate/Escaped-Surrogate Safety & Locked Auth Store
- **JSON repair & streaming (`crates/mehh-ai/src/streaming_json.rs`):**
  - Port `repair_json` (escapes raw `0x00..=0x1F` inside strings and doubles invalid backslashes) and `parse_tool_call_arguments` (enforcing JSON object root with `"Tool arguments must be a JSON object\n\nReceived arguments:\n..."` on failure).
  - Port `StreamingJsonParser` / `ToolCallArgumentsStream` (tolerant incremental parser for unterminated strings/objects/arrays with amortized copy credits) and wire into `anthropic.rs`, `openai.rs`, `openai_responses.rs`, and `bedrock.rs` `ToolCallDelta` + `ToolCallEnd`.
- **Surrogate & boundary text sanitization (`crates/mehh-ai/src/unicode.rs`):**
  - Port `sanitize_surrogates` for JSON text containing unpaired UTF-16 escape sequences (`\uD800`..`\uDFFF`) or `U+FFFD` / stray NUL bytes before outgoing provider JSON serialization (`anthropic`, `bedrock`, `google`, `mistral`, `openai`).
- **Thread/process-safe credential storage (`crates/mehh-ai/src/oauth.rs`, `crates/mehh-coding-agent/src/auth.rs`, `crates/mehh-mcp/src/oauth.rs`):**
  - Define async `CredentialStore` trait (`read`, `list`, `modify`, `delete`) + `InMemoryCredentialStore` (per-provider `tokio::sync::Mutex`) in `mehh-ai`.
  - Migrate `crates/mehh-coding-agent/src/auth.rs` onto `FileCredentialStore: CredentialStore`, holding cross-process advisory file locks (`tokio::task::spawn_blocking` + `std::fs::File::lock` on `<auth_path>.lock`) across the entire read -> expiry re-check -> HTTP refresh -> write sequence so concurrent OAuth refreshes cannot burn rotated single-use refresh tokens.
  - Apply the same `<path>.lock` advisory locking to `McpAuthStore` (`crates/mehh-mcp/src/oauth.rs`, `~/.mehh/agent/mcp-auth.json`).

---

## 5. Wave 5 — `mehh-agent`, `mehh-tools`, `mehh-coding-agent` & `mehh-cli` Parity

### 5.1 Agent Runtime Timing, Async Hooks, Turn Hooks & `AgentState` (`crates/mehh-agent`)
- **`duration_ms` timing:**
  - Add `#[serde(default, skip_serializing_if = "Option::is_none", rename = "durationMs", alias = "duration_ms")] pub duration_ms: Option<u64>` and `usage: Option<Usage>` to `mehh_ai::ToolResultMessage` and `AgentEvent::ToolExecutionEnd`.
  - Measure monotonic elapsed ms via `std::time::Instant::now()` in `execute_single_tool` (`agent_loop.rs`), omitting `duration_ms` when `before_tool_call` blocks execution.
- **Async tool hooks & richer `AgentToolResult`:**
  - Extend `AgentToolResult` and `AfterToolCallResult` with `structured_content: Option<serde_json::Value>`, `usage: Option<Usage>`, `is_error: Option<bool>`, and `terminate: Option<bool>`.
  - Convert `BeforeToolCallHook` and `AfterToolCallHook` to async callbacks (`BoxFuture<'static, ...>`) receiving `CancellationToken` and `context: Vec<Message>` alongside message/tool_call/args. Apply TS merging rules (`if content replaced without structured_content, clear structured_content`).
- **Turn lifecycle hooks (`AgentLoopConfig`):**
  - Add `prepare_request`, `prepare_next_turn`, `transform_context`, and `finish_turn` async hooks to `AgentLoopConfig` and invoke them in `run_agent_loop` (`agent_loop.rs`) at the exact TS turn boundaries.
- **Reactive `AgentState` (`crates/mehh-agent/src/agent.rs`, `types.rs`):**
  - Add `AgentState { system_prompt, model, thinking_level, tools, messages, is_streaming, streaming_message, pending_tool_calls, error_message }` updated from loop events, plus `Agent::state(&self) -> AgentState` and `Agent::reset(&self)`.
  - Keep `AgentSession` (`crates/mehh-coding-agent/src/agent_session.rs`) calling `run_agent_loop` directly without changing `AgentSession`'s public fields or mode callers.

### 5.2 Tools Safety & Windows PowerShell Parity (`crates/mehh-tools`)
- **`FileMutationQueue` (`crates/mehh-tools/src/file_mutation_queue.rs`):**
  - Key per-file `Arc<tokio::sync::Mutex<()>>` locks by canonicalized path (falling back to normalized lexical path, lowercased on Windows via `cfg!(windows)`), serializing same-file mutations while allowing different files to run concurrently.
  - Wrap `EditTool::execute` and `WriteTool::execute` inside `with_file_mutation_queue`.
- **Windows `BashTool` hardening & opt-in `PowerShellTool` (`crates/mehh-tools/src/bash.rs`, `powershell.rs`, `lib.rs`):**
  - Keep `BashTool` executing via `powershell` on Windows, adding the TS UTF-8 output encoding preamble and Windows PowerShell 5.1 native-command exit-code suffix so `bash` works reliably without requiring Git Bash.
  - Implement `PowerShellTool` (`"powershell"`) in `crates/mehh-tools/src/powershell.rs` (same UTF-8 preamble + exit-code suffix on Windows; fails cleanly with a clear platform error on non-Windows) and keep default `create_coding_tools(cwd)` at 7 tools (`bash`, not duplicate `powershell`), exposing `powershell` opt-in via `--tools powershell` / `create_all_tools(cwd)`.

### 5.3 Session Entry Kinds, Auto Branch Summary, Slash Commands & RPC (`crates/mehh-coding-agent`)
- **4 missing `SessionEntryKind` variants (`session.rs`):**
  - Add `Usage`, `CustomMessage` (`display`, `custom_type`, `content`, `details`), `ContextEdit` (`target_id`, `replacement`), and `Label` (`target_id`, `label`) variants plus `append_usage`, `append_custom_message`, `append_context_edit`, and `append_label`.
  - Update `build_context_messages` to project `CustomMessage` into model context and apply `ContextEdit` replacements/omissions along the active branch path.
- **Automatic `BranchSummary` (`crates/mehh-coding-agent/src/branch_summarization.rs`):**
  - Port `collect_entries_for_branch_summary`, `prepare_branch_entries` (chronological file-op extraction + budget trim), and `BRANCH_SUMMARY_PROMPT` / `BRANCH_SUMMARY_PREAMBLE`.
  - Wire branch-switch tree navigation in `interactive.rs` (`/tree`) and `AgentSession` to optionally generate and append `SessionEntryKind::BranchSummary` when leaving an active branch.
- **8 missing slash commands (`interactive.rs`):**
  - `/copy`: copy last assistant text via platform clipboard command (`powershell.exe Set-Clipboard` / `clip` on Windows, `pbcopy` on macOS, `wl-copy`/`xclip`/`xsel` on Linux) with OSC 52 fallback.
  - `/name [title]`: append `SessionEntryKind::SessionInfo { name }`.
  - `/session`: print session path, entry count, active leaf, message count, token usage, and total cost.
  - `/hotkeys`: render keybindings table derived dynamically from `Keybindings`.
  - `/reload`: reload `Settings`, `ModelRegistry`, `Skills`, `PromptTemplates`, and `Theme` (respecting workspace trust state).
  - `/scoped-models`: configure/view scoped model list for quick cycling.
  - `/export [path]` & `/import <path>`: export active session to JSONL and import external JSONL sessions into the session store.
- **11 missing RPC commands (`rpc.rs`):**
  - Implement `steer`, `follow_up`, `clear_queue`, `cycle_model`, `bash`, `abort_bash`, `fork`, `clone`, `switch_session`, `get_tree` (returning recursive `RpcSessionTreeNode` with labels), and `set_session_name`.
  - `fork`, `clone`, and `switch_session` swap/create `SessionManager` in place on `&mut AgentSession` (matching interactive `/resume` and `SessionManager::fork_at`/`clone_at_leaf`), keeping `run_rpc_mode`'s signature unchanged without adding a `runtimeHost` rebind abstraction.

### 5.4 CLI Flags & `@file` Ingestion (`crates/mehh-cli/src/main.rs`)
- Add missing `CliArgs` flags: `--name`, `--session-id`, `--session-dir`, `--fork`, `--models`, `--tools`, `--exclude-tools`, `--no-builtin-tools`, `--export`, `--skills`, `--no-skills`, `--prompt-templates`, `--no-prompt-templates`, `--use-theme`, `--no-themes`, `--no-context-files`, `--verbose`, and `--approve` / `--no-approve`.
- **Trust flag precedence:** Keep `--trust` as an alias for `--approve` (ephemeral single-run `trusted = true` without mutating `trust.json`). `--no-approve` forces `trusted = false` for that run even when `trust.json` has a saved `Trusted` entry (without mutating `trust.json`); passing both `--approve`/`--trust` and `--no-approve` fails CLI parsing.
- **`--export` syntax:** Follow TS (`--export <input-session-file> [output-file]`), consuming an optional first positional argument as the JSONL destination path and short-circuiting before session startup.
- Implement `@file` positional argument expansion (`process_file_arguments`): strip UTF-8 BOM from text files into `<file name="..."><content></file>` blocks and attach supported images (`png`, `jpeg`, `gif`, `webp`) as `UserContentBlock::Image`.

---

## 6. Wave 6 — `mehh-mcp` & `mehh-tui` Ergonomics Parity

### 6.1 `mehh-mcp` Protocol Lifecycle & Output Persistence (`crates/mehh-mcp`, `crates/mehh-coding-agent/src/mcp_manager.rs`)
- **`notifications/cancelled`:**
  - Send `{ method: "notifications/cancelled", params: { requestId, reason } }` whenever an in-flight non-`initialize` request times out or its `CancellationToken` fires (`client.rs`, `tool_adapter.rs`).
  - Handle incoming `notifications/cancelled` from servers to abort client-side request handling (`Cancel` token map).
- **`notifications/progress` & timeout renewal:**
  - Support `progress_token` (`_meta.progressToken = request_id`) in `McpClient::request` / `call_tool`, reset the request timeout timer whenever a matching `notifications/progress` arrives, and forward progress updates through `McpAgentTool::execute`'s `on_update` callback.
- **`McpOutputSaver` disk persistence (`tool_adapter.rs`):**
  - When combined text exceeds `MCP_OUTPUT_MAX_BYTES` (20 KiB), persist full content to `~/.mehh/tmp/mehh-mcp/` (or injected `McpOutputSaver` callback) and append `\n\n[Full output: <path> (read it with offset/limit)]` after middle truncation.
  - Decode and save non-image binary resource blobs to temp files (`[Binary resource <uri> saved to <path>]`) instead of dropping them as `"[binary resource omitted]"`.
- **Dynamic handlers & auto-refresh:**
  - Add `on_notification`, `on_error`, `on_close`, and `set_request_handler` registries to `McpClient`.
  - Subscribe to `notifications/tools/list_changed` in `mcp_manager.rs` (scoped to interactive mode where `McpManager` runs alongside `AgentSession`), wrapping `DeferredToolCatalog` and `CodemodeTool` (`crates/mehh-codemode/src/tool.rs`) tool lists in shared `Arc<RwLock<...>>` so between-turn `AgentSession` refreshes update active tools, deferred search index, codemode bindings, and `rebuild_system_prompt()` automatically without manual `/mcp reconnect`.

### 6.2 `mehh-tui` Matching, Editing, Scroll & Settings Ergonomics (`crates/mehh-tui`, `interactive.rs`)
- **Fuzzy matching (`crates/mehh-tui/src/fuzzy.rs`):**
  - Port `fuzzy_match` and tokenized `fuzzy_filter` (word-boundary/consecutive bonuses, gap penalties, digit-letter transposition) and wire into `SelectList` (`basic.rs`) for `/model`, `/resume`, `/tree`, and autocomplete filtering.
- **Emacs `KillRing` & Word Navigation (`crates/mehh-tui/src/components/editor.rs`):**
  - Replace single `kill_ring: String` with bounded multi-entry `KillRing` supporting forward/backward kill accumulation and `yank_pop` (`TuiAction::YankPop` registered in `keys.rs`, default `alt+y`).
  - Replace whitespace-only `move_word_left`/`move_word_right` with punctuation-run and atomic-token (`[paste #N ...]`) aware word navigation.
- **`WheelScrollAccelerator` (`crates/mehh-tui` / `interactive.rs`):**
  - Port `WheelScrollAccelerator` velocity scaling (`1`–`6` lines based on inter-event gap) replacing hardcoded `3`-line mouse scroll steps in `interactive.rs`.
- **2-Column `SettingsList` (`crates/mehh-tui/src/components/basic.rs` & `interactive.rs`):**
  - Add 2-column label/value + description pane rendering and value-cycling support for `/settings`.

---

## 7. Parity Classification Summary

| Feature Group | Classification | Rationale |
|---|---|---|
| `transform_messages`, `split_error_tool_result_images`, 6 provider ID normalizers | **Port 1:1** | Directly prevents API 400 rejections and cross-model thinking leaks |
| Anthropic `cache_control`, `anthropic-beta`, `ThinkingBudgets`, `is_oauth_token` | **Port 1:1** | Essential for token cost reduction and Anthropic OAuth parity |
| `repair_json`, `StreamingJsonParser`, `sanitize_surrogates` | **Port 1:1** | Prevents truncated/malformed JSON crashes in streaming tool calls |
| `CredentialStore` + `auth.json` file lock | **Adapt to Rust** | Uses `std::fs::File::lock` (`rustc 1.100.0`) + `tokio::sync::Mutex` |
| `mehh-agent` `duration_ms`, async hooks, turn hooks, `AgentState` | **Port 1:1** | Uses `BoxFuture` + `Instant::now()` for Rust async safety |
| `FileMutationQueue`, `PowerShellTool` | **Port 1:1** | Prevents parallel file corruption and restores Windows `powershell` tool |
| 4 `SessionEntryKind`, `BranchSummary`, CLI flags/`@file`, slash & RPC cmds | **Port 1:1** | Closes headless/RPC/interactive session workflow gaps |
| `/copy` clipboard & `/export` JSONL | **Adapt to Rust** | Platform CLI clipboard (`clipboard-command.ts` pattern) + JSONL export |
| `mehh-mcp` cancel, progress+timeout reset, `McpOutputSaver`, `list_changed` | **Port 1:1** | Prevents runaway MCP server processes and truncated large output loss |
| `mehh-mcp` `resources/subscribe` | **Parity Matched** | Confirmed absent in TS `coding-agent` tools (`resources.ts`) as well |
| `mehh-tui` fuzzy filter, `KillRing`, word nav, wheel accel, `SettingsList` | **Port 1:1** | High-value UX improvements without destabilizing `TuiRenderer` |
| `telemetry`, `schema`, `export-html`, TS extensions, `TuiAltScreen`, `models.generated.ts` | **Deliberately Omitted** | Explicit non-goals / JS-specific mechanics |

---

## 8. Testing & Verification Strategy

- **TDD discipline:** Write failing unit/integration tests (`RED`) for each ported behavior before modifying implementation code, porting TS regression scenarios (`#10995`, `#10794`, etc.) where applicable.
- **Wave 4 tests (`mehh-ai`, `mehh-coding-agent`):**
  - Cross-model thinking strip, encrypted reasoning retention, orphaned tool-call synthesis, held system messages, non-vision image downgrade, `split_error_tool_result_images`, and provider request payload snapshots for all 6 providers.
  - Anthropic `cache_control` breakpoints (`short` vs `long`), `anthropic-beta` header set, adaptive/disabled thinking, `is_oauth_token` headers + Claude Code tool casing.
  - `repair_json`, `StreamingJsonParser` partial object snapshots, `sanitize_surrogates`, and concurrent `auth.json` lock serialization tests.
- **Wave 5 tests (`mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-cli`):**
  - `duration_ms` present on executed tool calls and absent when blocked; async `before/after_tool_call` `structured_content`/`usage` overrides; `prepare_request`/`finish_turn` loop decisions; `AgentState` transitions.
  - `FileMutationQueue` concurrency serialization test (same path serialized, distinct paths concurrent); `PowerShellTool` execution/exit-suffix test.
  - `Usage`/`CustomMessage`/`ContextEdit`/`Label` roundtrip + context projection; `FauxProvider`-backed `BranchSummary` generation on tree switch; `@file` CLI expansion; slash & RPC command handler tests.
- **Wave 6 tests (`mehh-mcp`, `mehh-tui`):**
  - `InMemoryTransport` tests for outbound/inbound `notifications/cancelled`, `notifications/progress` timeout extension + `on_update`, `McpOutputSaver` file creation + binary blob persistence, and `notifications/tools/list_changed` refresh.
  - `fuzzy_match` / `fuzzy_filter` ranking, `KillRing` accumulation + `yank_pop`, punctuation/paste-token word navigation, `WheelScrollAccelerator` timing tiers, and `SettingsList` rendering/cycling.
- **Final gate per wave:**
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`

---

## 9. Decisions and Open Calls Made in This Spec

### Confirmed User Decisions (from Builder-Check)
1. **Windows `BashTool` vs opt-in `PowerShellTool`:** `BashTool` stays mapped to `powershell` on Windows (gaining the UTF-8 preamble and PS 5.1 exit-code suffix), while `"powershell"` (`PowerShellTool`) is added opt-in via `--tools powershell` / `create_all_tools` so default `create_coding_tools` avoids exposing two PowerShell tools.
2. **Trust CLI precedence (`--approve` / `--trust` / `--no-approve`):** `--trust` acts as an alias for `--approve` (ephemeral session trust), while `--no-approve` forces `trusted = false` for the current run even if `trust.json` holds `Trusted` (neither flag mutates `trust.json`).

### Builder-Check Calls Settled Here
3. **Zero new external crates (`std::fs::File::lock` + OS clipboard commands):** Relies on `rustc 1.100.0` stdlib file locking and platform CLI clipboard commands (`powershell.exe`/`clip`, `pbcopy`, `wl-copy`/`xclip`/`xsel` + OSC 52) rather than adding `fs2` or `arboard`.
4. **Keep `AgentSession` calling `run_agent_loop` directly:** Add `AgentState` and `reset(&self)` on standalone `mehh_agent::Agent` without refactoring `AgentSession`'s public fields or mode callers.
5. **In-place session swap for RPC `fork`/`clone`/`switch_session`:** Reuse `SessionManager` directly on `&mut AgentSession` (matching interactive `/resume`) rather than adding a JS-style `runtimeHost` rebind layer.
6. **Lock across full OAuth refresh & cover `McpAuthStore`:** `FileCredentialStore` holds `<auth_path>.lock` (`spawn_blocking` + `File::lock`) across read -> expiry re-check -> HTTP refresh -> write to avoid single-use token double-refresh, and `McpAuthStore` (`mcp-auth.json`) receives the same advisory lock.
7. **Interactive-scoped `notifications/tools/list_changed` + shared `Arc<RwLock<...>>` tool lists:** Propagate live MCP catalog updates in interactive mode across `AgentSession`, `DeferredToolCatalog`, and `CodemodeTool` between turns, leaving `run_rpc_mode` / print mode unchanged.
8. **`--export <input-session-file> [output-file]` + `/export` JSONL:** Match TS CLI `--export` positional syntax (JSONL output, no `export-html` vendor bundle).
9. **Keep `TuiRenderer` architecture (skip 1,800-line `TuiAltScreen` dual-engine):** Ports high-leverage `mehh-tui` UX (`fuzzy`, `KillRing`, `word_navigation`, `WheelScrollAccelerator`, `SettingsList`) without rewriting the terminal renderer around mouse text-selection / alt-screen search.
10. **Defer `models.generated.ts` 40-provider dump & niche OAuth flows (`radius`, `meta`, `kimi-coding`, `xai`, `openrouter`):** Keeps Wave 4 focused on `CredentialStore` locking + Anthropic OAuth parity alongside Wave 1's `models.json` registry; extra OAuth presets can be added cleanly later on top of `CredentialStore`.
