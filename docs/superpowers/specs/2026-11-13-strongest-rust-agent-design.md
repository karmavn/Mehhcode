# Strongest Rust Coding Agent — 3-Wave Design Specification

**Date:** 2026-11-13
**Target:** `crates/*` (`mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-tui`, `mehh-mcp`, `mehh-cli`)
**Baseline:** `cargo test --workspace` (203 passed, 0 failed), `cargo clippy --workspace --all-targets -- -D warnings` (0 warnings)

---

## 1. Intent and Goals

Turn the `crates/` Rust workspace from a feature-complete baseline into a durable, context-efficient, safe daily coding agent across headless (`text`/`json`/`rpc`) and `interactive` TUI modes.

### Goals
- **Wave 1 (Loop Resilience & Dynamic Models):** Survive transient provider/network failures (`429`/`5xx`/timeout), prevent context-limit crashes via proportional automatic compaction and safe overflow recovery, preserve CLI system prompt flags across tool refreshes, and support local/custom OpenAI-compatible endpoints via `models.json`.
- **Wave 2 (Sharper Tools, Bounded Context & Delegation):** Stream incremental output from `bash`, recover from minor whitespace/smart-character mismatches in `edit`, avoid bloating prompts with large MCP tool lists via BM25 `tool_search`, and isolate heavy codebase research/coding subtasks via an in-process `subagent` tool.
- **Wave 3 (Workspace Trust & TUI Ergonomics):** Gate workspace `.mehh/` overrides behind persisted user trust decisions resolved before connecting workspace MCP servers, add transcript scrollback plus live diff/output rendering in interactive mode, and add substring-capable `@file` path completion.

### Non-Goals
- Porting TypeScript `packages/telemetry` (OpenTelemetry), `packages/schema`, `export-html`, third-party `package-manager` (`npm`/`git` installs), or an external JS/TS extension host.
- Executing arbitrary shell commands (`"!cmd"`) inside `models.json` `apiKey` fields in Wave 1 (only literal strings and `"$ENV_VAR"` interpolation are supported to keep model loading safe outside trust workflows).

### Anti-Goals
- Infinite retry or infinite compact-and-retry loops that ignore `CancellationToken`, retry permanent `4xx` errors (`400`/`401`/`403`/`404`/`413`/`422`), or replay side-effecting mid-turn tool calls from scratch.
- Silently swallowing JSON syntax/schema errors when loading `models.json` or `trust.json` (only `ENOENT` falls back to empty defaults; malformed files must return explicit errors).
- Allowing recursive `subagent -> subagent` spawning or silently loading workspace `.mehh/*` overrides/MCP commands in an untrusted repository.

---

## 2. Approaches Considered

1. **Core Runtime & Tool Resilience Only:** Harden `mehh-ai` retry, `auto_compact`, `models.json`, `BashTool` streaming, `EditTool` fuzzy fallback, and `tool_search`, skipping `subagent`, `project-trust`, and TUI upgrades. *Rejected:* leaves long-session context pollution unaddressed and keeps TUI unscrollable without diff visibility.
2. **Multi-Agent, Security & TUI Ergonomics Only:** Build `subagent`, `project-trust`, transcript scroll/diff rendering, and `@file` completion first. *Rejected:* both main agent and child subagents remain fragile under rate limits (`429`) and context limits without Wave 1.
3. **Balanced 3-Wave Stack (Chosen):** Ship Wave 1 (durability) -> Wave 2 (tools + bounded context + `subagent`) -> Wave 3 (trust + TUI), intentionally deferring low-leverage TS parity. Each wave compiles, passes full workspace tests/clippy, and delivers standalone value.

---

## 3. Global Constraints

- **Workspace dependencies only:** Implement all 3 waves using existing dependencies declared in root `Cargo.toml` (`tokio`, `tokio-util`, `reqwest`, `serde`, `serde_json`, `regex`, `ignore`, `globset`, `similar`, `uuid`, `sha2`, `crossterm`, `unicode-width`, `unicode-segmentation`). Do not add new external crates without explicit approval.
- **Verification gate per wave:** After each wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must exit `0` with zero failures and zero warnings.
- **File & path normalization:** Normalize project paths via `fs::canonicalize` (falling back to lexical normalization) and lowercase on Windows (`cfg!(windows)`) before keying `trust.json` entries. Strip UTF-8 BOM when reading JSON configs and write UTF-8 without BOM.
- **Keybinding discipline:** Register all new interactive key actions through `TuiAction` and `Keybindings` in `crates/mehh-tui/src/keys.rs`; do not hardcode raw key checks in `interactive.rs`.

---

## 4. Wave 1 — Loop Resilience, Context Recovery & Dynamic Models

### 4.1 Unified HTTP Retry & Backoff (`crates/mehh-ai/src/retry.rs`)
Currently none of the 7 `reqwest`-based providers (`anthropic`, `openai`, `openai_responses`, `google`, `mistral`, `azure_openai`, `bedrock`) retry transient errors.

- **Design:**
  - Add `RetryPolicy` (`max_retries: u32 = 3`, `initial_backoff: Duration = 500ms`, `max_backoff: Duration = 10s`, `max_retry_after: Duration = 30s`) and one shared async helper:
    `pub async fn send_with_retry<F>(build_req: F, policy: &RetryPolicy, cancel: &CancellationToken) -> Result<reqwest::Response, RetryFailure>`
    where `build_req: FnMut() -> reqwest::RequestBuilder`.
  - **Retryable failures:**
    - Transport errors where `err.is_timeout() || err.is_connect() || err.is_request()`.
    - HTTP status codes `408 | 429 | 500 | 502 | 503 | 504 | 529`.
  - **Backoff calculation:** Honor numeric `Retry-After` response header (seconds) capped at `max_retry_after`; otherwise `min(max_backoff, initial_backoff * 2^attempt)` plus small jitter derived from `std::time::SystemTime` / `Uuid::now_v7()`.
  - **Cancellation safety:** Wrap both `req.send()` and `tokio::time::sleep(delay)` in `tokio::select! { _ = cancel.cancelled() => return Err(RetryFailure::Aborted), ... }` so user abort exits immediately.
  - **Fail-fast statuses:** `400`, `401`, `403`, `404`, `413`, `422` and mid-stream SSE errors after HTTP `200` are never retried inside `send_with_retry`; raw status + body text surface unchanged in `AssistantMessage.error_message` for overflow detection.
  - Update all 7 `reqwest` providers in `crates/mehh-ai/src/` to invoke `send_with_retry`.

### 4.2 Context Overflow Detection & Auto-Compaction
Today `Settings::auto_compact` can be toggled via `/settings`, but `AgentSession::prompt` never reads it, and interactive `start_turn` does not trigger compaction or recover from context overflow.

- **Overflow classifier (`crates/mehh-ai/src/overflow.rs`):**
  - Port `is_context_overflow(msg: &AssistantMessage, context_window: u64) -> bool` using `NON_OVERFLOW_PATTERNS` exclusion (`rate limit`, `too many requests`, `^(Throttling error|Service unavailable):`) + `OVERFLOW_PATTERNS` regexes covering Anthropic (`prompt is too long`, `request_too_large`), OpenAI (`exceeds the context window`, `maximum context length`), Google (`input token count.*exceeds`), Bedrock (`input is too long for requested model`), Mistral, Groq, OpenRouter, xAI, `llama.cpp` (`exceeds the available context size`), Ollama, LM Studio, and HTTP `413`.
  - Also detect silent overflow (`StopReason::Stop` with `usage.input + usage.cache_read > context_window`) and saturated length stop (`StopReason::Length` with `usage.output == 0` and `usage.input + usage.cache_read >= context_window * 99 / 100`).
- **Proportional compaction thresholds (`crates/mehh-coding-agent/src/compaction.rs`):**
  - Local models configured via Wave 1.3 commonly use `4k`–`32k` windows where fixed `16_384` reserve or `20_000` keep-recent budgets would either trigger every turn or fail to shrink context.
  - Add `pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;` and proportional budget helpers:
    - `effective_reserve_tokens(context_window: u64) -> usize`: `DEFAULT_RESERVE_TOKENS.min((context_window as usize) / 4)` (minimum `512` when `context_window >= 2048`).
    - `effective_keep_recent_tokens(context_window: u64) -> usize`: `DEFAULT_KEEP_RECENT_TOKENS.min((context_window as usize) / 2)`.
    - `should_auto_compact(entries: &[SessionEntry], context_window: u64) -> bool`: returns `true` when active estimated tokens `>= (context_window as usize).saturating_sub(effective_reserve_tokens(context_window))` and `prepare_compaction(..., effective_keep_recent_tokens(context_window))` finds compactable history.
  - Update `AgentSession::compact` to use `effective_keep_recent_tokens(self.model.context_window)`.
- **Session integration (`crates/mehh-coding-agent/src/agent_session.rs`):**
  - **Prompt preservation fix:** Add `custom_prompt: Option<String>` and `append_prompt: Option<String>` fields (plus `set_prompt_overrides`) on `AgentSession`. Update `enable_mcp_and_codemode` and a shared `rebuild_system_prompt()` method to pass stored overrides into `build_system_prompt_with_skills` so interactive `/mcp reload` or mid-session tool refreshes never drop CLI `--system-prompt` / `--append-system-prompt`.
  - **Effective `auto_compact` default (`settings.rs` / `agent_session.rs`):** Add `Settings::effective_auto_compact(&self) -> bool` returning `self.auto_compact.unwrap_or(true)` (enabled by default unless explicitly set `false`), and align `/settings` label in `interactive.rs`.
  - **Safe loop lifecycle in `AgentSession::prompt` (headless/print/RPC):**
    1. **Proactive pre-turn check:** Before running `run_agent_loop`, if `settings.effective_auto_compact()` and `should_auto_compact` is true, run `self.compact(None).await`. If summarization fails transiently during pre-turn compaction, emit a warning and proceed unless overflow hits.
    2. **Reactive overflow recovery (bounded to at most 1 retry):** Inspect `new_messages.last()`. When it matches `is_context_overflow(..., self.model.context_window)` and `settings.effective_auto_compact()`:
       - **Case A (Turn-0 overflow before any tool call ran):** Only `[UserPrompt, AssistantOverflowError]` exist in `new_messages`. Run `self.compact(None).await`. If `Ok(Some(_))`, discard the failed overflow pair and replay `UserPrompt` once. If `Ok(None)` (single prompt itself exceeds window), fail immediately without retrying.
       - **Case B (Mid-turn overflow after 1+ tool calls already executed):** `new_messages` holds `[UserPrompt, AssistantToolCall, ToolResult, ..., AssistantOverflowError]`. Do **not** replay `UserPrompt` (avoiding duplicate side-effecting `bash`/`write`/`edit` execution). Instead, persist all prefix messages before the trailing `AssistantOverflowError`, run `self.compact(None).await`, and if `Ok(Some(_))`, continue generation cleanly via `run_agent_loop(vec![], compacted_history, config, on_event)`.
  - **Interactive TUI parity (`crates/mehh-coding-agent/src/interactive.rs`):**
    - Run proactive `should_auto_compact` before `start_turn` (displaying `"Auto-compacting context..."` status).
    - Track per-turn `overflow_retried: bool` in `InteractiveState`: on `AgentSignal::Finished(messages)` matching `is_context_overflow`, execute the same Case A / Case B guard (`finish_prompt` prefix up to error, `compact(None).await`) and resume once (`start_turn` replay for Case A without duplicate `TranscriptItem::User`, or `run_agent_loop(vec![], ...)` continuation for Case B).

### 4.3 Custom Model & Endpoint Registry (`crates/mehh-coding-agent/src/models_config.rs`)
Currently `catalog::builtin_models()` and `AgentSession::default_provider_for_model` only handle hardcoded catalogs/endpoints, preventing zero-code setup of Ollama, `llama.cpp`, vLLM, or custom proxies.

- **Locations & precedence:**
  - Global: `~/.mehh/agent/models.json`
  - Project: `<cwd>/.mehh/models.json` (loaded only when workspace trust permits in Wave 3; global + project merged otherwise).
- **Schema (`ModelsConfigFile`, matching TS `model-config.ts`):**
  ```json
  {
    "providers": {
      "ollama": {
        "baseUrl": "http://127.0.0.1:11434/v1",
        "api": "openai-completions",
        "apiKey": "$OLLAMA_API_KEY",
        "headers": { "X-Custom": "value" },
        "models": [
          {
            "id": "qwen2.5-coder:32b",
            "name": "Qwen 2.5 Coder 32B",
            "reasoning": false,
            "contextWindow": 65536,
            "maxTokens": 8192
          }
        ]
      },
      "anthropic": {
        "modelOverrides": {
          "claude-sonnet-4-5": {
            "baseUrl": "https://proxy.example.com",
            "headers": { "X-Proxy-Auth": "$PROXY_TOKEN" }
          }
        }
      }
    }
  }
  ```
  - `providers.<name>.modelOverrides.<modelId>` supports overrides on `name`, `baseUrl`, `api`, `apiKey`, `reasoning`, `input`, `cost`, `contextWindow`, `maxTokens`, and `headers`, keyed under the provider so model IDs containing `/` (e.g., `openrouter/anthropic/claude-sonnet-4-5`) resolve unambiguously.
- **Env interpolation, key precedence & model switching:**
  - Strip BOM; `ENOENT` -> empty config; invalid JSON -> return `Err` with file path context.
  - Interpolate `"$ENV_VAR"` at lookup time (never persisted to disk): unset `"$ENV_VAR"` in `headers` omits that header; unset `"$ENV_VAR"` in `apiKey` resolves to `None` (so local Ollama/vLLM endpoints omit the `Authorization` header cleanly).
  - Add `explicit_cli_api_key: Option<String>` and `model_registry: ModelRegistry` onto `AgentSession`. Key precedence everywhere (startup, `prompt`, `/model`, `rpc set_model`):
    1. Explicit CLI `--api-key`
    2. `~/.mehh/agent/auth.json` + provider env vars (`resolve_api_key_async`)
    3. `ModelRegistry` (`models.json`) `providers.<name>.apiKey` / per-model override `apiKey`
  - Updating the active model via `set_model` (interactive `/model` picker, `rpc set_model`) clears any previously cached provider key and immediately re-resolves from `explicit_cli_api_key` -> `auth.json`/env -> `ModelRegistry`.

---

## 5. Wave 2 — Sharper Tools, BM25 Tool Search & Subagent Delegation

### 5.1 Streaming `BashTool` (`crates/mehh-tools/src/bash.rs`)
- Read `stdout` and `stderr` concurrently via `4KB` async chunk loops merged into a bounded rolling buffer (`256KB` memory cap, final + update snapshots truncated head+tail at `30,000` chars).
- Throttle `on_update` emissions to at most once every `100ms` (plus final flush) with `AgentToolResult::text(...)`.
- Preserve `cancel.cancelled()` and timeout kills (`kill_on_drop(true)` + explicit `child.kill().await`), reporting partial output captured prior to timeout.

### 5.2 Tolerant `EditTool` (`crates/mehh-tools/src/edit.rs`)
- Keep **Tier 1 (exact match)** via `normalized_content.match_indices(&old_norm)` as fast path.
- Add **Tier 2 (fuzzy fallback)** invoked only when Tier 1 yields 0 matches for `edits[i]`:
  - `normalize_for_fuzzy_match`: trim trailing horizontal whitespace per line, smart single/double quotes (`‘’‚‛` -> `'`, `“”„‟` -> `"`), Unicode dashes (`\u{2010}..\u{2015},\u{2212}` -> `-`), and Unicode spaces (`\u{00A0},\u{2002}..\u{200A},\u{202F},\u{205F},\u{3000}` -> `' '`), keeping zero external crate additions (no general `NFKC`).
  - Match full line windows (preserving leading indentation constraints) and map matched lines back to exact byte offsets in `normalized_content`.
  - Enforce uniqueness: `> 1` fuzzy match still returns an ambiguity error.
  - On 0 matches even after Tier 2, locate closest-matching line window via `similar` and include a concise 1-snippet line hint in the error message.

### 5.3 Dynamic Active Tools & BM25 `tool_search` (`crates/mehh-agent`, `crates/mehh-coding-agent`)
- **Mid-loop dynamic tool support (`crates/mehh-agent/src/agent_loop.rs`):**
  - Add `pub dynamic_tools: Option<Arc<std::sync::RwLock<Vec<Arc<dyn AgentTool>>>>>` to `AgentLoopConfig` (default `None`). When `Some`, `run_agent_loop` reads the active `Vec<Arc<dyn AgentTool>>` at the start of every LLM iteration and dispatches tool executions against it so `tool_search` unlocks take effect on the very next model call inside the same loop.
- **BM25 ranker & `ToolSearchTool` (`crates/mehh-coding-agent/src/tool_search.rs`):**
  - Core built-in tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, plus `subagent` and `codemode` if active) stay eagerly loaded.
  - Add `Settings::defer_mcp_tools: Option<bool>` (auto-defer when MCP tool count `> 8` unless set explicitly). Deferred MCP tools live in `DeferredToolCatalog` indexed by Okapi BM25 (`k1 = 1.2`, `b = 0.75`) over tool `name` (`weight = 3.0`), `local_name`, `description` + JSON schema property names/descriptions (`weight = 1.0`), and server namespace (`weight = 0.5`), plus exact/suffix name boost.
  - Calling `tool_search(query, limit)` unlocks up to `limit` (default `8`, max `20`) matching tools into the session's shared `RwLock<Vec<Arc<dyn AgentTool>>>` (deduplicated by name) and returns structured JSON + names/descriptions.
  - With `--codemode`, `CodemodeTool` continues seeing all MCP tools inside sandboxed scripts while `tool_search` exposes native tool schemas incrementally.

### 5.4 Isolated `subagent` Tool (`crates/mehh-coding-agent/src/subagent.rs`)
- Tool parameters:
  - `task` (`string`, required): Self-contained objective, target paths, and expected output format.
  - `mode` (`"research" | "coding"`, optional, default `"research"`):
    - `"research"`: `[read, grep, find, ls]` (read-only).
    - `"coding"`: `[read, write, edit, bash, grep, find, ls]`.
    - Neither mode includes `subagent` (hard nesting depth cap = 1).
  - `context` (`string`, optional): Extra context prepended to child prompt.
  - `max_turns` (`integer`, optional, default `12`, clamped `1..=30`): Enforced via new `pub max_turns: Option<usize>` on `mehh_agent::AgentLoopConfig`.
- Declared `ToolExecutionMode::Sequential` (safe default because `"coding"` mode mutates workspace files).
- Shares parent `Model`, `Arc<dyn LlmProvider>`, `api_key`, `cwd`, `settings` (`temperature`/`max_tokens`), and derives `cancel.child_token()`.
- Runs `SessionManager::in_memory(&cwd)` + `run_agent_loop`, forwards concise progress updates (`"[subagent] turn N: <tool>"`) to parent `on_update`, and returns final assistant text plus `details` (`{ mode, turns_used, total_tokens, cost, aborted }`).

---

## 6. Wave 3 — Project Trust & Daily TUI Ergonomics

### 6.1 Project Trust Gate (`crates/mehh-coding-agent/src/trust.rs`)
- **Trust Store (`~/.mehh/agent/trust.json`):**
  - Stores `{ entries: HashMap<String, TrustDecision> }` (`trusted: bool`, `decided_at: i64`), keyed by `normalize_trust_key(cwd)` (`fs::canonicalize` fallback lexical -> forward slashes -> lowercase on Windows).
  - Strip BOM; `ENOENT` -> empty store; malformed JSON -> explicit `Err` (fail closed).
- **Workspace resources requiring trust:**
  - Any existing `<cwd>/.mehh/{mcp.json, settings.json, models.json, skills/, prompts/}` triggers trust resolution before session resources are initialized (`AGENTS.md` / `CLAUDE.md` outside `.mehh/` remain loaded like TS).
  - Add trust-aware constructors/loaders (`SettingsManager::load_with_trust`, `ModelRegistry::load_with_trust`, `McpManager::connect_from_configs_with_trust`, `load_skills_with_trust`, `load_prompt_templates_with_trust`, `AgentSession::new_with_trust`).
- **Startup boundary (`crates/mehh-cli/src/main.rs` -> `run_interactive_mode`):**
  - Add `--trust` CLI flag (`CliArgs.trust: bool`), giving ephemeral session trust (`trusted = true` without modifying `trust.json`) in **both** headless and interactive modes.
  - Resolution runs **before** constructing `Settings`, `ModelRegistry`, `McpManager`, and `AgentSession`:
    - If no gated `.mehh/*` file exists or `trust.json` has a saved decision -> apply immediately without prompting.
    - If undecided + headless (`--print`/`json`/`rpc` without `--trust`) -> fail closed (`trusted = false`, skip `.mehh/*`, warn on stderr).
    - If undecided + interactive (without `--trust`) -> launch standalone pre-session TUI modal (`prompt_interactive_trust_decision(&cwd)` using `CrosstermTerminal` + `SelectorKind::Trust`: `Trust this workspace` / `Do not trust`) **before** initializing `Settings`, `ModelRegistry`, `AgentSession`, or `McpManager`, persist decision to `trust.json`, and then proceed with normal CLI flag precedence (`--model`, `--provider`, `--api-key`, `--thinking`) layered cleanly over the resulting workspace configuration.

### 6.2 TUI Transcript Scrollback & Rich Tool Diff/Output (`crates/mehh-tui`, `crates/mehh-coding-agent/src/interactive.rs`)
- **Keybindings (`crates/mehh-tui/src/keys.rs`):**
  - Register `TuiAction::ScrollUp` (`pageup`), `TuiAction::ScrollDown` (`pagedown`), and `TuiAction::ScrollBottom` (`ctrl+end`) in `Keybindings`.
  - Support mouse wheel scroll (`Event::Mouse(MouseEventKind::ScrollUp | ScrollDown)`), pairing `EnableMouseCapture` / `DisableMouseCapture` cleanly in `CrosstermTerminal` setup & `Drop`.
- **Scrollback state (`InteractiveState`):**
  - Add `scroll_offset: usize` (`0` = bottom auto-follow; `> 0` = frozen reading offset from bottom, clamped during `render_interactive_frame`, resetting to `0` on user submit or `ScrollBottom`).
  - Show indicator `[scrolled -N lines • PgDn/Ctrl+End]` in footer/status when `scroll_offset > 0`.
- **Rich `TranscriptItem::ToolCall`:**
  - Extend with `args_summary: String`, `output_preview: String`, `diff: Option<String>`.
  - Capture concise `args_summary` on `ToolExecutionStart`, update rolling tail (`max 10 lines`) on `ToolExecutionUpdate` (surfacing live `BashTool` streaming), and render `details["diff"]` on `ToolExecutionEnd` with theme `+`/`-` colors (capped at `24` lines per edit card).

### 6.3 `@file` Path Autocomplete (`crates/mehh-tui`, `crates/mehh-coding-agent/src/file_complete.rs`)
- **Editor prefix-replacement (`crates/mehh-tui/src/components/editor.rs`):**
  - Add `pub autocomplete_replace_chars: usize` on `Editor` and `Editor::set_autocomplete_replacing(&mut self, items: Vec<SelectItem>, replace_chars: usize)` (`set_autocomplete` sets `replace_chars = 0`; `clear_autocomplete` resets `0`).
  - When accepting an item in `Editor::handle_editor_key`, delete `autocomplete_replace_chars` chars immediately before `cursor_col` on the current line and insert `item.label`.
- **Workspace indexer & token detector (`file_complete.rs` + `interactive.rs`):**
  - Detect `@<partial>` ending at cursor (bounded by line start or whitespace before `@`), alongside existing `/` command autocomplete.
  - Index workspace paths with `ignore::WalkBuilder` (respecting `.gitignore`, skipping `.git`/`target`/`node_modules`, max `5,000` files, `5s` TTL refresh on demand).
  - Rank `starts_with` > substring/basename matches (top `12` items), passing `replace_chars = partial.chars().count()` (preserving `@` or replacing `@partial` consistently: `replace_chars = 1 + partial.chars().count()`, `item.label = format!("@{path} ")`) so selecting `@auth` anywhere in a multi-word prompt cleanly becomes `@crates/mehh-coding-agent/src/auth.rs ` without disturbing surrounding text.

---

## 7. Testing & Verification Strategy

- **Wave 1 tests:**
  - `mehh-ai`: `tokio::net::TcpListener` mock HTTP server testing `429`/`503` + `Retry-After` backoff, immediate failure on `400`/`401`/`413`, abort mid-sleep, and full `is_context_overflow` suite (including silent & length-stop overflow).
  - `mehh-coding-agent`: `FauxProvider` tests for proportional `should_auto_compact` on small (`8k`/`16k`) and large (`200k`) windows, Case A turn-0 replay vs. Case B mid-turn `run_agent_loop(vec![], ...)` continuation without re-running completed tool calls, `custom_prompt`/`append_prompt` survival across `enable_mcp_and_codemode`, and `ModelRegistry` BOM/syntax failure/env interpolation/`modelOverrides` precedence across `set_model`.
- **Wave 2 tests:**
  - `mehh-tools`: `BashTool` incremental `on_update` emission test; `EditTool` Tier-2 trailing-whitespace + smart-quote + dash + CRLF preservation + ambiguity error tests.
  - `mehh-agent` & `mehh-coding-agent`: `tool_search` BM25 ranking + live mid-loop dynamic tool activation via `FauxProvider`; `subagent` research vs. coding tool scopes, depth-1 restriction, `max_turns` enforcement, and parent-to-child cancel propagation.
- **Wave 3 tests:**
  - `mehh-coding-agent` & `mehh-tui`: `trust.json` normalization + fail-closed JSON/IO + headless/interactive trust gate isolating `.mehh/{mcp.json,settings.json,models.json,skills,prompts}`; `Editor` `autocomplete_replace_chars` mid-line `@file` replacement; `interactive.rs` `scroll_offset` clamping + rich `ToolCall` diff/stream rendering; `WorkspaceFileIndex` matching/ranking.

---

## 8. Open Calls Made in This Spec

1. **`auto_compact` effective default (`Settings::effective_auto_compact`):** Proposed `unwrap_or(true)` so long runs compact automatically out of the box rather than failing on context limits until manually enabled.
2. **Proportional small-context compaction (`effective_reserve_tokens` / `effective_keep_recent_tokens`):** Scaled reserve (`min(16_384, window / 4)`) and keep-recent (`min(20_000, window / 2)`) budgets so local `4k`–`32k` models compact cleanly without looping.
3. **Wave 1 `models.json` `apiKey` scope:** Supports literal strings and `"$ENV_VAR"` (missing env var = `None` for key, omit for header), omitting `"!shell_command"` execution.
4. **Workspace trust scope:** Gates `.mehh/{mcp.json, settings.json, models.json, skills/, prompts/}` via a lightweight pre-session TUI modal before initializing workspace config/MCP, while keeping `AGENTS.md`/`CLAUDE.md` outside `.mehh/` loaded as instructions and `--trust` ephemeral across both headless and interactive modes.
5. **`subagent` execution mode & `EditTool` normalizer:** Single `subagent` tool uses `ToolExecutionMode::Sequential`; `EditTool` Tier-2 skips general `NFKC` in favor of the explicit Unicode character table to keep zero new external crate dependencies.
