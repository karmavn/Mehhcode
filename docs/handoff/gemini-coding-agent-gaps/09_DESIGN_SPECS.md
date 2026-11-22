# Dump 09 - Design specs (nguyen si)

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| docs/superpowers/specs/2026-11-13-strongest-rust-agent-design.md | 25.4 | 232 | 2609eb297977376ba334c535b3d52f523f7a031cc32433fa8a0e57125243afd1 |
| docs/superpowers/specs/2026-11-15-ts-parity-waves-4-5-6-design.md | 30 | 229 | 3dbf1d6d847378f36d4ef60b7d1e21413770a7aa47fcbb01a6769cc2c59ef4ec |
| docs/superpowers/specs/2026-11-16-goose-parity-core-design.md | 38.1 | 326 | 89214e55df16ae433145de19eef5193bcce0b4604b6217b32e331a5b0c47b39b |
| docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md | 7.3 | 142 | 1443f8450dc2dc088c7c11b621355d1673061f8485f994443329d1b37b5b6ab0 |
| docs/superpowers/specs/2026-11-18-subagent-port-claw-code-design.md | 7.1 | 83 | c97108556ef6053634f682bdc96dea418f1ee6726d67d6980e2519dc81c73594 |
| docs/superpowers/specs/2026-11-20-review-suggestions-design.md | 2.2 | 42 | 471d39a40cc2e1506772ee4d938ec3cc5316fccca91a500767b0524a90e89d5a |
| docs/superpowers/specs/2026-11-21-tui-ux-optimization-design.md | 8.7 | 139 | 9250ba86db4f77d6694cbb2749fd4c14d8a87b7ad03f6a598c239bd15ac111b7 |

## docs/superpowers/specs/2026-11-13-strongest-rust-agent-design.md

`````markdown
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
`````

## docs/superpowers/specs/2026-11-15-ts-parity-waves-4-5-6-design.md

`````markdown
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
`````

## docs/superpowers/specs/2026-11-16-goose-parity-core-design.md

`````markdown
# Goose-Parity Core & Runtime Architecture — 3-Wave Design Specification

**Date:** 2026-11-16
**Target:** `crates/*` (`mehh-agent`, `mehh-mcp`, `mehh-tools`, `mehh-codemode`, `mehh-coding-agent`, `mehh-tui`, `mehh-cli`)
**Approach:** Approach A — Core Runtime Governance, Code Intelligence, Working Memory, and Multi-Mode Lead/Worker Subagents

---

## 1. Intent and Why

Mehhcode already outperforms `aaif-goose/goose` in binary footprint (~16.9 MB standalone binary vs. Electron + daemon), zero-IPC in-process tools (`mehh-tools`), QuickJS code execution (`mehh-codemode`), file mutation serialization (`FileMutationQueue`), and migration-free JSONL v3 tree persistence (`SessionManager`).

However, inspecting the current codebase reveals specific runtime and coding-intelligence gaps compared to Goose:
- **Unwired lifecycle hooks:** `AgentLoopConfig` (`crates/mehh-agent/src/agent_loop.rs`) declares `before_tool_call`, `after_tool_call`, `prepare_request`, `prepare_next_turn`, `transform_context`, and `finish_turn`, yet `AgentSession::prepare_prompt_content` (`crates/mehh-coding-agent/src/agent_session.rs`) leaves all six `None`.
- **No runtime tool governance:** `trust.rs` only gates loading `.mehh/*` at startup. Once inside a session, tools execute with no dangerous-command guard, no `smart_approve`/interactive confirmation, and no infinite-repetition loop breaker. Furthermore, `CodemodeTool` (`crates/mehh-codemode/src/tool.rs`) invokes `tool.execute(...)` directly, bypassing `AgentLoopConfig` hooks, and `McpToolInfo` (`crates/mehh-mcp/src/client.rs`) drops MCP `annotations` (`readOnlyHint`, `destructiveHint`).
- **Blind TUI tool cards:** `TranscriptItem::ToolCall` (`crates/mehh-coding-agent/src/interactive.rs`) stores only `{ id, name, status, is_error }`, discarding live `BashTool` streaming previews and `EditTool` unified diffs.
- **Missing code structure, undo, and working memory:** Agents burn tokens calling `read` on huge files without an AST/symbol outline (`analyze`), cannot undo a bad file mutation without `git`, lack a live `Todo` + MOIM (`Model-Observed Internal Memory`) checklist injected across compactions, and `ToolSearchTool` (`crates/mehh-coding-agent/src/tool_search.rs`) uses naive substring `contains` scoring without indexing JSON schema properties.
- **Constrained subagent & single-model execution:** `SubagentTool` (`crates/mehh-coding-agent/src/subagent.rs`) is locked to 4 read-only tools, drops `on_update`, runs sequentially, and always uses the parent's heavy model instead of Lead/Worker model routing.

Closing these gaps turns Mehhcode into a safer, lower-token, faster terminal coding agent than Goose without sacrificing its single-binary, in-process architecture.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
- **Wave 1 — Runtime Tool Governance, MCP Hints & TUI Tool Visibility:**
  - Introduce a priority-ordered `ToolInspector` pipeline (`SecurityInspector` -> `PermissionInspector` -> `RepetitionInspector`) wired through `AgentSession::prepare_prompt_content` (`before_tool_call`) AND through `CodemodeTool`'s internal sandbox executor.
  - Parse MCP `annotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) in `McpToolInfo` and add `is_read_only(&self) -> Option<bool>` on `AgentTool`.
  - Add `PermissionMode` (`Auto`, `SmartApprove`, `Approve`, `Chat`), persisted tool permission overrides (`~/.mehh/agent/permissions.json`), CLI flags, `/mode` slash command, serialized interactive TUI approval modal with diff/command preview, and rich `TranscriptItem::ToolCall` rendering (live streaming output + colorized `EditTool` diff).
- **Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM, and BM25 `tool_search`:**
  - Add a pure-Rust structural code analyzer tool (`analyze`) supporting `structure`, `symbols`, and `focus` modes for Rust, TypeScript/JavaScript, Python, and Go with zero new external crates.
  - Add session-scoped file snapshot & rollback (`EditUndoStore` + `undo_edit` tool) integrated inside `with_file_mutation_queue` for `EditTool` and `WriteTool`.
  - Add `TodoTool` (`todo`) + MOIM injection via `AgentLoopConfig.prepare_request` (keeping active checklist and pinned instructions visible at the turn boundary without polluting saved JSONL).
  - Upgrade `DeferredToolCatalog` in `tool_search.rs` to Okapi BM25 (`k1 = 1.2`, `b = 0.75`) indexing tool names, descriptions, server namespaces, and JSON schema property keys/descriptions.
- **Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing:**
  - Upgrade `SubagentTool` to support `mode: "research" | "coding" | "review"`, live `on_update` progress forwarding, parent `ToolInspector` enforcement inside child loops, and parallel subagent execution (`ExecutionMode::Parallel` for `research`/`review`; `FileMutationQueue` + serialized approval broker for `coding`).
  - Add Lead/Worker model routing (`LeadWorkerConfig` in `Settings` / CLI `--worker-model` / `--lead-turns`): route `SubagentTool` (default) and post-lead execution turns to a fast/cheap worker model with automatic escalation/fallback back to the lead model on provider/tool-loop failure.

### Non-Goals
- Adding `tree-sitter` C crates or any new external Cargo dependencies (`Cargo.toml` dependencies remain fixed).
- Goose YAML `Recipe` engine (`mehh-recipe`), `ChatRecall` cross-session search, HTTP/WebSocket daemon (`mehh serve`), or IDE `ACP` server (`mehh acp`) — reserved for follow-up tracks (Approaches B and C).
- Using an auxiliary LLM call as a permission classifier inside `SmartApprove` (deterministic rules + `is_read_only` + shell/path classification avoid extra latency, token cost, and network failure modes).
- Converting `mehh-tools` into external MCP child processes (core tools stay in-process).

### Anti-Goals
- Allowing `CodemodeTool` JS scripts or `"coding"` subagents to bypass `SecurityInspector` or `PermissionInspector`.
- Blocking headless CLI/RPC runs (`-p`, `--mode json`, `--mode rpc`) on an interactive approval prompt that has no TTY listener (headless `RequireApproval` must fail closed as `Deny` with an actionable error).
- Persisting ephemeral MOIM injections into `.jsonl` session entries or silently swallowing malformed `permissions.json` syntax errors (`ENOENT` -> default; corrupt JSON -> explicit `Err`).
- Allowing recursive `subagent -> subagent` spawning (`coding`/`research`/`review` child tool lists must exclude `subagent`).

---

## 3. Global Constraints

- **Zero new external crates:** Implement Waves 1–3 strictly with existing workspace dependencies in root `Cargo.toml` (`tokio`, `tokio-util`, `futures`, `serde`, `serde_json`, `regex`, `ignore`, `globset`, `similar`, `uuid`, `sha2`, `crossterm`, `unicode-width`, `unicode-segmentation`, `clap`, `tracing`, `anyhow`, `thiserror`).
- **Path & file hygiene:** Normalize file paths canonically (with lexical fallback and lowercasing on Windows `cfg!(windows)`) before keying security rules, `FileMutationQueue`, `EditUndoStore`, or `permissions.json`. Strip UTF-8 BOM on config reads and write UTF-8 without BOM using `write_file_atomic`.
- **Backward-compatible Serde evolution:** New fields on shared structs (`Model`, `Settings`, `McpToolInfo`, `AgentToolResult`, `AgentEvent`) use `#[serde(default, skip_serializing_if = "Option::is_none")]` (or `#[serde(default)]`) so existing tests and JSONL sessions remain valid.
- **Keybinding discipline:** Register any new TUI interactive actions (approval keys if mapped via actions) in `TuiAction` / `Keybindings` (`crates/mehh-tui/src/keys.rs`) rather than hardcoding raw checks inconsistently.
- **Verification gate per wave:** After each wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with 0 failures and 0 warnings.

---

## 4. Approaches Considered

1. **Approach A — Core Governance, Code Intelligence, Working Memory & Multi-Mode Lead/Worker Subagents (Chosen):** Hardens runtime safety, wires all unused `AgentLoopConfig` hooks, adds pure-Rust AST `analyze` + `undo_edit` + `Todo`/MOIM + BM25 `tool_search`, renders live diffs/approvals in TUI, and upgrades `subagent` + Lead/Worker routing. Delivers immediate daily coding superiority over Goose in the standalone CLI/TUI binary.
2. **Approach B — Declarative Recipes & Multi-Session Recall (`mehh-recipe` + `ChatRecall`):** Focuses on YAML playbook automation and cross-session history search. Deferred until Approach A establishes runtime tool governance (`ToolInspector`), which automated recipes depend on to run safely.
3. **Approach C — Daemon Server & IDE Protocol (`mehh serve` + `mehh acp`):** Focuses on multi-client HTTP/WebSocket serving and Zed/VS Code ACP integration. Deferred until core execution safety, TUI observability, and Lead/Worker subagents are solid.

---

## 5. Wave 1 — Runtime Tool Governance, MCP Annotations & TUI Approval/Diffs

### 5.1 Tool Read-Only Metadata & MCP Annotations (`mehh-agent`, `mehh-mcp`, `mehh-tools`)

1. **`mehh-mcp` (`crates/mehh-mcp/src/client.rs`, `tool_adapter.rs`):**
   - Add `McpToolAnnotations` struct (`read_only_hint: Option<bool>`, `destructive_hint: Option<bool>`, `idempotent_hint: Option<bool>`, `open_world_hint: Option<bool>`, camelCase serde) and `#[serde(default, skip_serializing_if = "Option::is_none")] pub annotations: Option<McpToolAnnotations>` on `McpToolInfo`.
   - Update `McpAgentTool` (`tool_adapter.rs`) to expose `annotations` and implement `AgentTool::is_read_only(&self) -> Option<bool>` returning `self.tool_info.annotations.as_ref().and_then(|a| a.read_only_hint)` and `AgentTool::execution_mode(&self)`: `Parallel` only when `read_only_hint == Some(true)`, otherwise `Sequential` (preventing unannotated mutating MCP tools from firing concurrently without serialization).
   - Update `ListMcpResourcesTool`, `ListMcpResourceTemplatesTool`, `ReadMcpResourceTool` (`tool_adapter.rs`) to return `is_read_only() -> Some(true)`.

2. **`mehh-agent` (`crates/mehh-agent/src/types.rs`):**
   - Add two default methods on `AgentTool`:
     - `fn is_read_only(&self) -> Option<bool> { None }`
     - `fn is_safe_for_auto_approve(&self, _args: &serde_json::Map<String, serde_json::Value>) -> Option<bool> { self.is_read_only() }`
     *(Note: `is_safe_for_auto_approve` allows `BashTool`/`PowerShellTool` to inspect the command string, while pure read tools return `Some(true)`.)*

3. **`mehh-tools` & `mehh-coding-agent` implementations:**
   - `ReadTool`, `GrepTool`, `FindTool`, `LsTool`: `is_read_only() -> Some(true)`.
   - `WriteTool`, `EditTool`: `is_read_only() -> Some(false)`.
   - `BashTool`, `PowerShellTool`: `is_read_only() -> Some(false)`. (Read-only shell classification in `SmartApprove` is handled centrally inside `PermissionInspector` rather than scattered in tools so all rules stay auditable in one place.)
   - `ToolSearchTool` (`tool_search.rs`): `is_read_only() -> Some(true)`.
   - `SubagentTool` (`subagent.rs`): `is_read_only()` returns `Some(true)` only when invoked in `"research"`/`"review"`? Since `is_read_only(&self)` takes `&self` (static metadata), `SubagentTool` returns `None` (neutral container) and its inner child loop runs through the parent's `ToolInspectionManager`, which inspects each child `read`/`edit`/`bash` call individually!
   - `CodemodeTool` (`mehh-codemode/src/tool.rs`): `is_read_only() -> None` at the outer tool boundary (or `Some(false)` unless inspected per inner tool call — see 5.2 below!).

### 5.2 Chain-of-Responsibility `ToolInspectionManager` (`crates/mehh-coding-agent/src/tool_inspection.rs`)

Create `crates/mehh-coding-agent/src/tool_inspection.rs`:

- **Core Types:**
  ```rust
  pub enum InspectionVerdict {
      Allow,
      RequireApproval { reason: String, preview: Option<String> },
      Deny { reason: String },
  }

  #[async_trait::async_trait]
  pub trait ToolInspector: Send + Sync {
      fn name(&self) -> &str;
      async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
  }
  ```
  `ToolInspectionContext` holds `{ tool_call_id, tool_name, args, is_read_only: Option<bool>, cwd }`.
  `ToolInspectionManager` runs inspectors in strict priority order and merges verdicts:
  - First `Deny` stops immediately and blocks the tool call (`BeforeToolCallResult { block: true, reason: Some(reason), terminate: false }`).
  - Any `RequireApproval` escalates to the configured `ApprovalHandler` (even if a later inspector would `Allow`).
  - `Allow` proceeds only if no inspector returned `Deny` or `RequireApproval`.

- **Inspector 1 — `SecurityInspector` (Priority 1, runs in ALL modes including `Auto`):**
  - **Command danger patterns (for `bash` / `powershell`):** Compiled `OnceLock<Vec<(Regex, &'static str)>>` matching catastrophic/exfiltration patterns (e.g., `rm -rf /` or `rm -rf ~`, `mkfs`, `dd if=.* of=/dev/`, fork bombs `:(){ :|:& };:`, `curl|wget .* \| (sh|bash)`, reading/exfiltrating `~/.ssh/id_*`, `~/.mehh/agent/auth.json`, `~/.mehh/agent/mcp-auth.json`, `Invoke-Expression` / `iex` piped from `Invoke-WebRequest`). Returns `RequireApproval` in interactive `Auto` mode (or `Deny` if critical/untrusted) and `Deny` in `Chat`/headless without explicit override.
  - **Sensitive path protection (for `read`, `write`, `edit`, `undo_edit`):** Rejects writes/edits to `~/.mehh/agent/{auth.json,mcp-auth.json,trust.json,permissions.json}` so a prompt-injected model cannot rewrite its own permission rules or steal OAuth tokens. Rejects writes outside normalized `cwd` unless the user explicitly approves (`RequireApproval` in interactive, `Deny` in headless unless `--allow-outside-cwd` / `Auto`).

- **Inspector 2 — `PermissionInspector` (Priority 2):**
  - **Modes (`PermissionMode`):**
    - `Auto`: Executes tools without prompting (except `SecurityInspector` flags).
    - `SmartApprove` (**default in interactive mode**):
      - Checks persisted rules in `~/.mehh/agent/permissions.json` (`AlwaysAllow`, `AskBefore`, `NeverAllow`) and session-scoped `"Allow for this session"` grants first.
      - Otherwise, if `ctx.is_read_only == Some(true)` -> `Allow`.
      - If `tool_name` is `bash` / `powershell` -> checks a conservative read-only command whitelist (`git status`, `git diff`, `git log`, `git show`, `cargo check`, `cargo clippy`, `cargo test`, `ls`, `dir`, `pwd`, `cat`, `rg`, `find`, `which`, `Get-ChildItem`, `Select-String` when free of redirections `>`, `| sh`, `-exec`, or chaining `;`/`&&`/`||` containing non-whitelisted commands). Safe read-only commands -> `Allow`; mutating/unknown commands -> `RequireApproval` with command preview.
      - If `tool_name` is `edit` / `write` / `undo_edit` -> `RequireApproval` with pre-computed unified diff preview (or byte/line summary for `write`).
      - If `tool_name` is `codemode` -> outer `codemode` execution is `Allow` (unless `Chat` mode), while EVERY inner `tools.<name>(args)` call inside `CodemodeTool` runs through `ToolInspectionManager`! That way read-only JS scripts run without prompting, while a JS script calling `tools.edit(...)` or `tools.bash(...)` triggers `SecurityInspector` + `PermissionInspector` cleanly.
    - `Approve`: Prompts (`RequireApproval`) for every tool call not explicitly marked `AlwaysAllow`.
    - `Chat`: Read-only conversational investigation mode. Allows read-only exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, and MCP tools with `annotations.readOnlyHint == Some(true)`). Strictly returns `Deny` for mutating tools (`edit`, `write`, `undo_edit`), shell execution (`bash`, `powershell`), unannotated MCP tools, and `subagent` with `mode: "coding"`.
    - **Rule Scoping & Granularity in `permissions.json`:**
      - Scoped at `~/.mehh/agent/permissions.json` (per-user) and optional trusted `.mehh/permissions.json` (per-workspace, only loaded if workspace is trusted in `trust.rs`).
      - Schema supports:
        - `tool_name: String` (e.g. `"read"`, `"mcp__github__get_issue"`, `"bash"`).
        - `decision: "AlwaysAllow" | "AskBefore" | "NeverAllow"`.
        - `arg_prefix: Option<String>` (e.g., `command_prefix: "cargo test"` or `"git status"` for `bash`/`powershell`).
        - `path_glob: Option<String>` (e.g., `path_glob: "tests/**"` for `edit`/`write`/`undo_edit`).
      - Evaluation precedence: Exact rule (`tool_name` + matching `arg_prefix`/`path_glob`) > Tool-level rule (`tool_name` with no filter) > Default mode policy (`SmartApprove`).
    - **MCP Unannotated Tool Fallback:** MCP tools without `readOnlyHint` (`annotations.read_only_hint == None`) fail closed: treated as mutating (`is_read_only() -> None`), requiring explicit approval in `SmartApprove` and denied in `Chat` mode.

- **Inspector 3 — `RepetitionInspector` (Priority 3):**
  - Tracks consecutive identical `(tool_name, canonical_json_args)` calls per turn/session (`Mutex<RepetitionState>`, default `max_repetitions = 3`).
  - Resets when a different tool or different arguments are called (or on turn reset).
  - When count exceeds `max_repetitions`, returns `Deny { reason: "Repeated identical tool call detected (3+ times). Change strategy or inspect prior output instead of repeating the exact same call." }`.

### 5.3 Interactive Approval Broker, `CodemodeTool` Integration & Headless Policy

- **Wiring `AgentSession` (`crates/mehh-coding-agent/src/agent_session.rs`):**
  - Add `permission_mode: PermissionMode`, `permission_store: PermissionStore`, `approval_handler: Option<Arc<dyn ApprovalHandler>>`, and `inspection_manager: Arc<ToolInspectionManager>` on `AgentSession`.
  - In `prepare_prompt_content`, wire `config.before_tool_call` to invoke `inspection_manager.inspect_and_authorize(...)` (checking `RepetitionInspector`, `SecurityInspector`, `PermissionInspector`, and calling `approval_handler` if `RequireApproval`).
  - Wire `CodemodeTool` (`crates/mehh-codemode/src/tool.rs`): add an optional `ToolCallInterceptor` callback trait/closure on `CodemodeTool` (`with_interceptor(...)`) invoked before `tool.execute(...)` inside the QuickJS executor closure (`tool.rs:140-162`). `AgentSession` attaches the same `ToolInspectionManager` + `approval_handler` to `CodemodeTool`.
- **Serialized Interactive Approval (`interactive.rs`):**
  - Add `AgentSignal::ApprovalRequest { request: ApprovalRequest, reply: tokio::sync::oneshot::Sender<ApprovalDecision> }` and `SelectorKind::ToolApproval` in `interactive.rs`.
  - Wrap `ApprovalHandler` requests behind an async `tokio::sync::Mutex<()>` so parallel tool calls (e.g., two MCP tools requiring approval) queue their modal prompts one-by-one rather than overwriting `state.selector`.
  - When `SelectorKind::ToolApproval` is active, display tool name, target path/command, and up to 18 lines of diff/preview inside a `BorderedBox`, letting the user choose:
    - `Allow once` (`y` / `Enter` on option 1)
    - `Allow tool for session` (`a`)
    - `Always allow tool` (`A`, persisted to `~/.mehh/agent/permissions.json`)
    - `Deny` (`n` / `Esc`)
  - Register `y`/`a`/`n` shortcuts cleanly or navigate with `Up`/`Down` + `Enter` inside `SelectList` so existing keybindings stay uniform.
- **Headless Policy & Security Boundaries (`mehh-cli` / `modes.rs` / `rpc.rs` / `trust.rs`):**
  - Add `--permission-mode <auto|smart_approve|approve|chat>` to `CliArgs`.
  - **Permission Mode Escalation Guard:** Workspace `.mehh/settings.json` is strictly forbidden from loosening `permission_mode` to `Auto`. Any workspace setting specifying `auto` is ignored and clamped to `SmartApprove` unless overridden by global `~/.mehh/agent/settings.json` or explicit `--permission-mode auto` on the CLI.
  - **Protected Metadata Files:** `<cwd>/.mehh/*` files (`settings.json`, `trust.json`, `permissions.json`) are strictly write-protected in `SecurityInspector` against write/edit/undo tools during a session.
  - **Headless Runs (`-p`, `--mode json`):** Default to `PermissionMode::SmartApprove`. `ApprovalHandler` is `HeadlessDenyHandler`: any `RequireApproval` verdict fails closed as `Deny` with an actionable error (`"Tool '<name>' requires approval in headless mode (pass --permission-mode auto or configure permissions.json)"`).
  - **RPC Mode (`--mode rpc`):** During RPC handshake (`initialize` / `get_session`), the client declares its capabilities (`capabilities: { tool_approvals: bool }`). If `tool_approvals: true`, `ApprovalHandler` sends an `approval_request` JSON-RPC notification and awaits an `approve_tool` response. If unannounced or `false`, it fails closed (`Deny`) to protect non-interactive RPC scripts.
- **Slash command `/mode`:**
  - Add `/mode [auto|smart_approve|approve|chat]` in `interactive.rs` (`builtin_slash_commands()` + `handle_slash_command`) and expose `permission_mode` in `/settings` (`build_settings_list`).

### 5.4 Rich `TranscriptItem::ToolCall` Rendering (`interactive.rs`)

- Extend `TranscriptItem::ToolCall`:
  ```rust
  ToolCall {
      id: String,
      name: String,
      status: String,
      is_error: bool,
      args_summary: String,
      output_preview: Vec<String>,
      diff: Option<String>,
  }
  ```
- In `apply_agent_event`:
  - `ToolExecutionStart`: populate concise `args_summary` (e.g., `path` for `read`/`edit`/`write`, truncated `command` for `bash`, `query` for `grep`/`tool_search`, `mode` for `subagent`).
  - `ToolExecutionUpdate`: retain the last 8 non-empty lines of `partial_result` text into `output_preview` (so `BashTool` streaming and `SubagentTool` progress render live in the transcript).
  - `ToolExecutionEnd`: set final status (`completed` / `failed`, plus `duration_ms` if present), extract `result.details["diff"]` into `diff`, and clear transient streaming preview for `read`/`grep`/`find`/`ls` while keeping concise tail (`max 8 lines`) on `bash` errors or `diff` (capped at `24` lines) with theme `+` (`tool_success`) / `-` (`tool_error`) / `@@` (`accent`) colors.

---

## 6. Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM & BM25 `tool_search`

### 6.1 Pure-Rust Structural Code Analyzer (`crates/mehh-tools/src/analyze.rs`)

- **Tool name:** `analyze` (`is_read_only() -> Some(true)`, `ToolExecutionMode::Parallel`).
- **Parameters:**
  - `path` (`string`, required): file or directory path (relative to `cwd` or absolute).
  - `mode` (`"structure" | "symbols" | "focus"`, optional, default `"symbols"` for files, `"structure"` for directories).
  - `focus` (`string`, optional): symbol name (e.g., `AgentSession` or `prepare_prompt_content`) to zoom into and print exact line range + body (capped at `250` lines).
  - `depth` (`integer`, optional, default `2`, clamped `1..=5`): directory recursion depth for `"structure"` mode.
- **Parser Engine (Zero External Dependencies):**
  - Uses `ignore::WalkBuilder` for directory traversal in `structure` mode (skipping `.git`, `target`, `node_modules`).
  - Language detection by extension:
    - **Rust (`.rs`)**: matches `pub(crate|super)? (async )?(unsafe )?(fn|struct|enum|trait|impl|mod|type|const|macro_rules!)` and tracks `{`/`}` brace depth (ignoring strings/comments) to compute accurate `[start_line..end_line]` spans for each item and its methods inside `impl` blocks.
    - **TypeScript / JavaScript (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`)**: matches `export`, `class`, `interface`, `type`, `enum`, `(async )?function`, and arrow/method signatures with `{`/`}` span tracking.
    - **Python (`.py`)**: matches `(async )?def`, `class` and computes `[start_line..end_line]` via indentation tracking.
    - **Go (`.go`)**: matches `func`, `type ... struct`, `type ... interface` with `{`/`}` span tracking.
  - Truncates output cleanly (`MAX_ANALYZE_CHARS = 24_000`) with actionable guidance to call `read` or `analyze(focus=...)`.

### 6.2 File Snapshot & Rollback (`crates/mehh-tools/src/undo_edit.rs`, `edit.rs`, `write.rs`)

- **Shared `EditUndoStore`:**
  - Thread-safe per-session/tool-set stack `Arc<Mutex<HashMap<String, Vec<FileSnapshot>>>>`, keyed by `get_mutation_queue_key(&full_path)` (`file_mutation_queue.rs`).
  - `FileSnapshot`:
    ```rust
    pub enum FileSnapshot {
        Existed { bytes: Vec<u8>, tool_name: String, timestamp_ms: i64 },
        DidNotExist { tool_name: String, timestamp_ms: i64 },
    }
    ```
  - Bounded per-file (`MAX_SNAPSHOTS_PER_FILE = 20`) and total memory (`MAX_UNDO_BYTES = 16 MiB` total across snapshots; skips snapshots for files `> 4 MiB` with a note).
  - `EditTool` (`with_undo_store(...)`) and `WriteTool` (`with_undo_store(...)`) record the pre-mutation bytes (preserving exact UTF-8 BOM and CRLF bytes!) or `DidNotExist` inside `with_file_mutation_queue` immediately before `tokio::fs::write`.
- **`UndoEditTool` (`"undo_edit"`):**
  - `is_read_only() -> Some(false)`, `ToolExecutionMode::Sequential`.
  - Parameters: `path` (`string`, required).
  - Runs inside `with_file_mutation_queue(&full_path, ...)`: pops the latest `FileSnapshot` for that normalized path, restores exact original bytes (or deletes the file if `DidNotExist` and current content matches/exists), and returns unified diff of the revert in `details["diff"]`.
  - Also expose `/undo` slash command in `interactive.rs` (optional user shortcut calling `EditUndoStore` directly or guiding the user).

### 6.3 `TodoTool` & MOIM Working Memory Injection (`crates/mehh-coding-agent/src/todo.rs`, `agent_session.rs`)

- **`TodoState` & `TodoTool` (`"todo"`):**
  - Shared `Arc<RwLock<TodoState>>` stored on `AgentSession` (and persisted optionally as `SessionEntryKind::CustomMessage` or dedicated `custom_type: "todo_state"` / reconstructed from active branch entries on `/resume` and `/tree`!).
  - `TodoItem { id: String, content: String, status: TodoStatus (Pending | InProgress | Completed), priority: Option<String> }`.
  - Tool operations (`action`: `"replace" | "add" | "update" | "list" | "clear"`): `is_read_only()` returns `None` (`list` is safe, mutating actions update only in-memory/session checklist — treated as `Allow` in `SmartApprove` since it touches no workspace files or external commands!).
- **MOIM (`Model-Observed Internal Memory`) via `prepare_request` hook:**
  - Wire `AgentLoopConfig.prepare_request` in `AgentSession::prepare_prompt_content`.
  - Before every LLM call inside `run_agent_loop` (including multi-turn tool chains and post-compaction turns), `prepare_request` reads:
    1. Active `TodoState` (if non-empty, formatted compactly as `<working_checklist>...</working_checklist>`).
    2. Optional pinned session/env invariants (`MEHH_MOIM_TEXT` / `MEHH_MOIM_FILE` or `.mehh/MOIM.md` when `trusted == true`, capped at 8 KB).
  - Injects this ephemeral block into the final `User` message (or as a transient `Message::System` at the tail of `req_ctx.context.messages` — *our call:* append as a single transient `Message::System` or trailing user reminder block only in `req_ctx.context.messages` after `transform_messages` strips/handles system turns cleanly!). Let's verify `transform_messages.rs` first so `Message::System` at the tail works across all 6 providers — if `Anthropic`/`Google` reject mid/trailing `System` messages, fold MOIM cleanly into the last `User`/`ToolResult` text context or into `system_prompt` clone! Wait: modifying `req_ctx.context.messages` to append a synthetic `<moim_context>` text block onto the latest user turn (or a trailing user context message only when the last message is `ToolResult` / `User`) guarantees 100% compatibility across Anthropic, OpenAI, Google, Bedrock, and Mistral without ever persisting to `new_messages` or `SessionManager`!

### 6.4 BM25 & Schema Indexing in `tool_search.rs`

- Upgrade `DeferredToolCatalog::search` (`crates/mehh-coding-agent/src/tool_search.rs`):
  - Precompute inverted document stats (`DeferredToolIndex`) on `DeferredToolCatalog::new` and `replace_deferred`:
    - Tokenize identifiers (`snake_case`, `camelCase`, `mcp__server__tool` splits on `_`, `-`, uppercase transitions) into normalized lowercase terms.
    - Weighted fields:
      - Exact tool name / local tool name boost (`+50.0` exact match, `+15.0` suffix/substring match).
      - BM25 (`k1 = 1.2`, `b = 0.75`) across weighted fields: `name` + `local_name` (`weight = 3.0`), `description` + JSON schema property names & property descriptions (`weight = 1.0`), server namespace (`weight = 0.5`).
  - Keep `ToolSearchTool` response format (`matches` + `newlyActivated`) backward-compatible so existing callers/tests and `CodemodeTool` work seamlessly.

---

## 7. Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing

### 7.1 Multi-Mode `SubagentTool` (`crates/mehh-coding-agent/src/subagent.rs`)

- **Tool Parameters:**
  - `prompt` (`string`, required when `tasks` is omitted): Self-contained objective.
  - `tasks` (`array` of `{ prompt: string, mode?: string, context?: string, max_turns?: integer }`, optional, max `6` items): Enables a single LLM turn to fan out parallel subtasks cleanly via `futures::future::join_all` (capped at concurrency `4` via `tokio::sync::Semaphore` to avoid API rate-limit bursts).
  - `mode` (`"research" | "coding" | "review"`, optional, default `"research"`):
    - `"research"`: `["read", "grep", "find", "ls", "analyze"]`.
    - `"coding"`: `["read", "write", "edit", "undo_edit", "bash", "powershell", "grep", "find", "ls", "analyze"]` (excludes `subagent`, `codemode`, `todo`).
    - `"review"`: `["read", "grep", "find", "ls", "analyze"]` with a review-oriented system prompt focusing on bugs, invariants, and missing tests.
  - `context` (`string`, optional): Extra context prepended to child user prompt.
  - `max_turns` (`integer`, optional, default `15`, clamped `1..=30`).
- **Safety & Execution Mode:**
  - Child `AgentLoopConfig` ALWAYS inherits the parent's `before_tool_call` (`ToolInspectionManager` + serialized `ApprovalHandler`) and `cancel.child_token()`.
  - Since `EditTool` / `WriteTool` / `UndoEditTool` already serialize per-file via `FileMutationQueue`, parallel `"coding"` tasks editing disjoint files run concurrently while same-file edits serialize cleanly; `SubagentTool::execution_mode()` returns `Parallel` when `mode` isn't statically known, relying on `FileMutationQueue` + serialized approval mutex for safety (or keeps `Sequential` for single calls when `mode == "coding"` if checked? Since `execution_mode(&self)` has no `params`, keeping `FileMutationQueue` + approval mutex safe for concurrent execution is mandatory!).
  - **Progress Streaming:** Wire `on_event` inside the child `run_agent_loop` to forward concise throttled updates (`"[subagent:<mode>] turn N: <tool_name>"` / `"[subagent batch 2/3] completed"`) through the parent tool's `on_update` callback!
  - **Rich Return Details:** Return `details: { mode, model, turns_used, total_tokens, cost, aborted }` alongside final assistant text (or aggregated structured report for `tasks` batches).

### 7.2 Lead/Worker Model Routing (`crates/mehh-coding-agent/src/lead_worker.rs`, `settings.rs`, `agent_session.rs`)

- **Configuration (`Settings` + CLI):**
  - Extend `Settings` (`settings.rs`) with optional fields:
    - `permission_mode: Option<PermissionMode>`
    - `worker_model: Option<String>` (e.g. `"anthropic/claude-3-5-haiku-latest"` or `"openai/gpt-4o-mini"` or local Ollama model from `models.json`)
    - `lead_turns: Option<usize>`: Defines the number of initial planning turns handled by the `lead` model for each user prompt.
      - **Per-Prompt Lifecycle:** `lead_turns` tracking resets at the start of each user prompt. The first `n` turns execute on the Lead model, switching to Worker for subsequent turns.
      - **Escalation Rules:** An automatic fallback/escalation from Worker back to Lead occurs ONLY when the Worker hits a provider error (`StopReason::Error`), returns an empty response, or trips `RepetitionInspector`. Normal tool errors (e.g. test failure, compiler error, missing file) do NOT trigger escalation. Once escalated, the session remains on Lead for the remainder of that user prompt.
  - Add `--worker-model <MODEL>` and `--lead-turns <N>` to `CliArgs` (`mehh-cli/src/main.rs`).
- **Resolution & Key Isolation:**
  - Resolve `worker_model` via `ModelRegistry` (`models.json` + built-in catalog) and resolve its provider + API key separately via `resolve_api_key_async` / `ModelRegistry::provider_api_key` so a cross-provider Lead/Worker pair (e.g., Anthropic Opus Lead + OpenAI/Gemini/Ollama Worker) never leaks Anthropic keys to OpenAI or vice versa!
- **Integration Points:**
  1. **`SubagentTool`:** When a `worker` model + provider + key is configured, `SubagentTool` uses the `worker` model for `"research"` and `"coding"` subtasks (unless `"review"` or explicitly overridden, or falls back to `lead` on worker failure).
  2. **Main `AgentSession` loop (`prepare_next_turn` / `prepare_request`):** Wait — notice `AgentLoopConfig` currently holds a single `provider: Arc<dyn LlmProvider>` and `stream_options.api_key: Option<String>`, while `AgentLoopTurnUpdate` can change `model` and `thinking_level` (`agent_loop.rs:173-178`). To support cross-provider Lead/Worker switching mid-loop cleanly, extend `AgentLoopTurnUpdate` / `AgentRequestUpdate` (`crates/mehh-agent/src/types.rs`) with optional `provider: Option<Arc<dyn LlmProvider>>` and `api_key: Option<Option<String>>` (non-breaking `#[serde(skip)]` fields or builder defaults), and update `run_agent_loop` to pass the active `(current_model, current_provider, current_api_key)` into `stream_assistant_response`!

---

## 8. Verification & Testing Strategy

- **TDD discipline:** Write failing unit/integration tests (`RED`) before implementing each component, using `FauxProvider` and `InMemoryTransport` (no live API calls).
- **Wave 1 tests:**
  - `mehh-mcp`: `McpToolInfo` deserialization with/without `annotations`; `McpAgentTool::is_read_only()` and `execution_mode()`.
  - `mehh-coding-agent`: `SecurityInspector` blocks credential exfiltration (`~/.ssh`, `auth.json`) and destructive shell patterns; `PermissionInspector` matrix across `Auto`, `SmartApprove`, `Approve`, `Chat` (`read` allowed, `edit`/`bash` require approval in `SmartApprove`); `RepetitionInspector` triggers after `max_repetitions`; `CodemodeTool` interceptor blocks dangerous inner JS `tools.bash(...)` calls; headless approval fails closed; serialized concurrent approval broker test; `TranscriptItem::ToolCall` live preview + diff rendering test.
- **Wave 2 tests:**
  - `mehh-tools`: `AnalyzeTool` `structure`, `symbols`, and `focus` tests across Rust/TS/Python/Go fixtures; `EditUndoStore` + `UndoEditTool` tests restoring BOM/CRLF edits, deleting newly created `write` files, and respecting `FileMutationQueue`.
  - `mehh-coding-agent`: `TodoTool` CRUD + `prepare_request` MOIM injection verification (present in `FauxProvider` captured request context, absent from `.jsonl` session entries); BM25 `tool_search` ranking tests verifying schema property matches outrank noise.
- **Wave 3 tests:**
  - `mehh-coding-agent` & `mehh-agent`: `SubagentTool` `"coding"` mode executing `edit` through `ToolInspectionManager`, parallel `tasks` batch execution, `on_update` progress emissions, and Lead/Worker cross-provider switching + error escalation via `FauxProvider`.
- **Final gate per wave:** `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.

---

## 9. Decisions and Open Calls Left Explicit

### Design Calls Made Here (Reviewable by Builder & User)
1. **Deterministic `SmartApprove` (no LLM permission judge):** `PermissionInspector` relies on `AgentTool::is_read_only()`, MCP `annotations.readOnlyHint`, and a deterministic read-only shell command parser rather than making an extra LLM classification call per unknown tool. Faster, zero token cost, deterministic, and offline-safe.
2. **Pure-Rust structural `analyze` parser (zero new crates):** Avoids adding `tree-sitter` C bindings and grammar crates, keeping builds fast and dependency-free without requiring `gcc`/`cl` in `PATH`. Can be upgraded later behind a Cargo feature flag if full syntax trees for obscure languages are needed.
3. **Ephemeral MOIM injection on trailing context (no `.jsonl` bloat):** `prepare_request` injects `<working_checklist>` / MOIM only into the outgoing `Context` snapshot, reconstructing `TodoState` from session entries (`CustomMessage` `"todo_state"`) so `/resume` and `/tree` branch switches keep the right task state automatically.
4. **Headless `SmartApprove` fails closed on mutating tools:** When running `-p`/`--mode json` without `--permission-mode auto`, tools requiring approval fail closed with an explicit message rather than silently auto-approving or hanging waiting for stdin.
5. **Cross-provider `provider` + `api_key` swap in `AgentLoopTurnUpdate`:** Extending `AgentLoopTurnUpdate` (`crates/mehh-agent/src/types.rs`) lets `LeadWorker` switch between different providers (e.g., Anthropic Lead -> OpenAI/Ollama Worker) mid-turn cleanly without duplicating loop logic.
6. **Granular rule scoping in `permissions.json`:** Rules support optional `arg_prefix` (for shell commands) and `path_glob` (for file mutations). Exact prefix/glob rules take precedence over general tool-level permissions.
7. **Read-only conversational `Chat` mode:** `/mode chat` permits read-only codebase exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, MCP `readOnlyHint: true`) and denies all mutating tools, shell execution, and unannotated MCP tools.
8. **Per-prompt Lead/Worker tracking & strict escalation:** `lead_turns` resets on every user prompt. Escalation from Worker back to Lead triggers only on provider error (`StopReason::Error`), empty assistant completion, or `RepetitionInspector` loop breaks (routine tool errors like compiler/test failures stay on Worker).
9. **Workspace security lockdown:** Workspace `.mehh/settings.json` cannot elevate `permission_mode` to `Auto`, and `<cwd>/.mehh/*` configuration files are strictly write-protected from in-session agent mutations.
10. **RPC capability handshake:** Non-interactive or legacy RPC clients fail closed on `RequireApproval` unless they announce `"capabilities": { "tool_approvals": true }` during session handshake.

### What Is Left to the Builder
- Exact regex set for `SecurityInspector` and read-only command whitelist tokens inside `PermissionInspector` (follow fail-closed conservative defaults: anything with shell redirections, subshells, pipes to interpreters, or unknown commands escalates to `RequireApproval`).
- Exact UI layout spacing and color token mapping for `SelectorKind::ToolApproval` and diff line truncation inside `crates/mehh-coding-agent/src/interactive.rs`.
- Exact BM25 tokenization edge cases and `EditUndoStore` eviction policy (`VecDeque`/FIFO per file).
`````

## docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md

`````markdown
# OpenCode-Style Interactive Session Switcher & Management — Design Specification

**Date:** 2026-11-17  
**Target:** `crates/*` (`mehh-tui`, `mehh-coding-agent`, `mehh-cli`)  
**Approach:** Approach B — Interactive TUI-First Session Switcher & Management  

---

## 1. Intent and Why

Mehhcode already possesses a robust append-only JSONL v3 branching tree session format (`crates/mehh-coding-agent/src/session.rs`), zero-allocation session listing (`FastSessionLine`), and an in-memory transcript rebuilder (`rebuild_transcript_items`).

However, user experience falls short of modern AI coding agents like OpenCode:
1. **Hidden / Fragmented Commands:** Users intuitively type `/session` or `/sessions` to switch sessions, but Mehhcode's `/session` only dumps plain text stats, `/sessions` fails as an unknown command, and switching is hidden under `/resume`.
2. **Missing Hotkey:** No keyboard shortcut (e.g. `Ctrl+S`) exists to invoke the session list.
3. **Directory Isolation:** `list_sessions(cwd)` only searches `~/.mehh/sessions/<cwd_hash>/`. Launching `mehh` from a subfolder (e.g. `target/release` or a sub-crate) isolates the user from previous sessions in the project root.
4. **No In-Modal Actions:** The current `/resume` picker cannot create a new session (`/new`), delete stale sessions, or toggle between current directory and global workspace sessions.

Implementing Approach B transforms session management into an intuitive, keyboard-driven experience matching OpenCode while maintaining Mehhcode's zero-dependency, single-binary architecture.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
- **Unified Slash Commands:**
  - `/sessions` and `/session` (without arguments) open the interactive Session Switcher modal.
  - `/session info` displays session metadata and token stats.
  - `/session new` acts as an alias for `/new`.
  - `/resume` remains supported as a backward-compatible alias.
- **Dedicated Hotkey (`Ctrl+S`):**
  - Register `TuiAction::SessionPicker` (default `ctrl+s`) in `Keybindings` (`crates/mehh-tui/src/keys.rs`).
  - Pressing `Ctrl+S` when no selector is open immediately launches the Session Switcher.
- **Quick New Session Entry:**
  - The first item in the Session Switcher list is always `[+ Start New Session]`. Selecting it invokes `session.session_mut().new_session()` without exiting.
- **Workspace vs Global Scope Toggle (`Tab`):**
  - Implement `list_all_sessions()` in `session.rs` to scan all session subdirectories under `~/.mehh/sessions/`.
  - In the session modal, pressing `Tab` toggles between *Current Workspace* (`~/.mehh/sessions/<cwd_hash>/`) and *All Workspaces* (`~/.mehh/sessions/*`).
- **Session Deletion (`d` / `Delete`):**
  - Pressing `d` or `Delete` on an existing session item deletes the `.jsonl` file from disk and removes it from the selector list.

### Non-Goals
- Adding SQLite databases or migrating away from JSONL v3 files (file-based append-only persistence remains).
- Full cross-session semantic vector search (reserved for `ChatRecall`).
- Electron or browser-based GUI session managers.

### Anti-Goals
- Deleting the currently active running session from within the modal without confirmation or fallback.
- Blocking the UI thread during file deletion or cross-project scanning.
- Adding any new external crates to `Cargo.toml`.

---

## 3. Approaches Considered

1. **Approach A (Goose-style CLI Subcommands):** Focuses on external CLI flags (`mehh session list`, `mehh session resume`). Rejected by the user because daily coding happens inside the interactive TUI.
2. **Approach B (OpenCode-style Interactive TUI Focus - Chosen):** In-terminal modal popup with `Ctrl+S`, `/sessions`, scope toggling, quick new session, and deletion. Delivers immediate UX parity with OpenCode and Claude Code.
3. **Approach C (Hybrid CLI + TUI):** Combines both. Approach B covers the high-priority interactive workflows immediately without expanding CLI surface area unnecessarily.

---

## 4. Detailed Design & Architecture

### 4.1 Keybindings (`crates/mehh-tui/src/keys.rs`)

Add `TuiAction::SessionPicker`:
```rust
pub enum TuiAction {
    // ... existing variants
    SessionPicker,
}
```
In `Keybindings::default_bindings()`:
```rust
map.insert(TuiAction::SessionPicker, vec!["ctrl+s".into()]);
```
Format hotkeys in `/hotkeys` to display:
```text
Ctrl+S: Switch or create sessions
```

### 4.2 Cross-Directory Session Discovery (`crates/mehh-coding-agent/src/session.rs`)

Add `list_all_sessions() -> Result<Vec<SessionSummary>>`:
1. Locates `~/.mehh/sessions/`.
2. Iterates over all subdirectories (`<cwd_hash>`).
3. Reads all `.jsonl` session files using `BufReader` and `FastSessionLine`.
4. Extracts `SessionSummary { id, path, timestamp, cwd, entry_count, name, first_prompt }`.
5. Sorts descending by `timestamp` (newest first).

Add `delete_session_file(path: &Path) -> Result<()>`:
- Verifies path is within `~/.mehh/sessions/` and ends with `.jsonl` before calling `std::fs::remove_file`.

### 4.3 Interactive Session Switcher Overlay (`crates/mehh-coding-agent/src/interactive.rs`)

#### Selector State Extension:
Add scope tracking in `Selector`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerScope {
    CurrentWorkspace,
    AllWorkspaces,
}
```
`SelectorKind::Resume` handles:
- **Item 0:** `SelectItem::new("__new__", "[+ Start New Session]").with_description("Start a fresh conversation in current directory")`
- **Subsequent Items:** Sessions formatted with relative time/timestamp, title/first prompt snippet, entry count, and (in All Workspaces mode) project directory name.

#### Key Event Dispatches:
When `state.selector` has `kind == SelectorKind::Resume`:
- **`Enter`:**
  - If `item.id == "__new__"`: calls `session.clear_queues()`, `session.session_mut().new_session()`, resets transcript to welcome message.
  - Otherwise: loads `SessionManager::load_file(&item.id)` and calls `rebuild_transcript_items`.
- **`Tab`:**
  - Toggles `scope` between `CurrentWorkspace` and `AllWorkspaces`.
  - Re-populates `sel.list` items without closing the popup.
- **`d` or `delete`:**
  - If `item.id == "__new__"`: no-op.
  - If `item.id == current_session_path`: prints notice "Cannot delete currently active session."
  - Otherwise: calls `delete_session_file(&item.id)`, removes from `sel.list`, prints confirmation.
- **`Escape`:** Closes selector.

#### Outside Selector:
- Pressing `keybindings.matches(&key_str, TuiAction::SessionPicker)` opens the Session Switcher modal.

---

## 5. Verification & Testing

1. **Unit tests in `crates/mehh-coding-agent/src/session.rs`:**
   - Test `list_all_sessions` aggregates sessions across multiple `cwd_hash` directories.
   - Test `delete_session_file` removes target `.jsonl` and fails closed on external paths.
2. **TUI tests in `crates/mehh-coding-agent/src/interactive.rs`:**
   - Test `/sessions` and `/session` open `SelectorKind::Resume`.
   - Test `/session info` prints session details.
   - Test selecting `__new__` initializes a fresh session.
   - Test `Tab` toggles scope and updates item list.
   - Test `d` removes session entry from selector list.
3. **Workspace Checks:**
   - `cargo check --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
`````

## docs/superpowers/specs/2026-11-18-subagent-port-claw-code-design.md

`````markdown
# Subagent Port from claw-code — Design

**Date:** 2026-11-18

## Intent and why

The human partner reports that mehh code's subagent **display** and **optimization** are weak. They asked to study claw-code (Rust port of Claude Code), which they believe is the strongest subagent implementation, and port what is genuinely better.

Recon and line-by-line verification over both codebases changed the premise: most of claw-code's advertised subagent features are stubs or stale changelog claims. Only a small set of mechanisms is real and worth porting. This document records what was verified, what gets ported, and what is deliberately left out.

## What was verified in claw-code

All claims were read directly in source, not inferred from docs.

| Mechanism | Evidence | Real? |
|---|---|---|
| Machine-readable lifecycle manifest (`laneEvents`, blocker classification, derived state, commit provenance) | `rust/crates/tools/src/lib.rs:4526-4570`, `:5132-5171`, `:5173-5187`, `:5252-5288` | Yes |
| Allowlist applied to tool definitions sent to provider | `lib.rs:5355-5366`, `:5549-5557` | Yes |
| No-recursive-fan-out default (allowlist omits `Agent`) | `lib.rs:4485-4504`, test `:9696-9717` | Yes, but static omission, not a depth guard |
| Panic containment writes `failed` manifest | `lib.rs:4345-4366` | Yes |
| Per-child provider fallback chain | `lib.rs:5309-5332`, `:5383-5395` | Yes |
| Correct parallel `tool_result` merge ("both sides pure") | `lib.rs:5630-5644`, comment `:5611-5612` | Yes |
| SDK `TurnControlSignal` (steer/abort queue) | `rust/crates/runtime/src/conversation.rs:119-214` | Yes, but session-level only; `run_turn(prompt, None)` at `lib.rs:4375` means children get no signal |

Advertised but **not implemented** in claw-code: `/subagent list|steer|kill` (spec only, `commands/src/lib.rs:1024-1030`), `AgentGate`/`maxConcurrentAgents`/`maxParallelToolCalls` (only `CHANGELOG.md:98-100`), `subagentModel` in spawn path (`resolve_agent_model` at `lib.rs:4420-4426` ignores it), subagent TUI (none exists).

## Goals

1. Give every mehh subagent a **machine-readable lifecycle manifest** on disk: id, mode, status, timeline events, failure class, derived state, terminal result/error.
2. Make subagent activity **visible in the TUI** as a nested group with per-child rows, driven by structured events instead of ad-hoc text lines.
3. Add a **global concurrency budget** shared across all subagent calls in a session (today `Semaphore::new(4)` is created per call, so 3 concurrent calls allow 12 children).
4. Add **per-child lifecycle handles**: `list`, `steer`, `kill` with stable child IDs, reusing mehh's existing `AgentLoopConfig.steering_queue`.
5. Persist terminal state on **all** exit paths: success, error, timeout, abort, task panic.

## Non-goals

- No per-child provider fallback chain. This needs new provider-chain infrastructure in `mehh-ai` (no fallback type exists there today); it is a separate project.
- No change to the three existing modes (`research`/`coding`/`review`) or their tool filters.
- No change to `mehh-agent`'s agent loop or `mehh-tui`'s generic components.
- No recursive subagent spawning. Keep current exclusion of `subagent`/`codemode` from child tools.
- No persistence of child conversation history, no resume of killed children.

## Anti-goals (what would make this a failure)

- A TUI that shows a nested group but still cannot tell which child is running or what it last did.
- A manifest that is written only on success, leaving `running` forever on timeout.
- A "global" budget that is still per-call, or a `kill` that cancels the whole parent turn instead of one child.
- Copying claw-code's `iso8601_now()` bug (it returns epoch seconds despite its name). mehh stores integer epoch milliseconds with explicit `_ms` field names.

## Constraints

- Rust workspace, `cargo test -p mehh-coding-agent` from workspace root is the verification command.
- Only erasable-safe, idiomatic Rust; no new runtime dependencies required. `uuid`, `serde`, `serde_json`, `tokio`, `tokio-util` are already declared in `crates/mehh-coding-agent/Cargo.toml`.
- Timestamps are `i64` epoch milliseconds via `mehh_ai::now_ms()` (returns `i64`, matches `UserMessage.timestamp`); field names end in `_ms`.
- Manifests live in `<cwd>/.mehh-agents` unless `MEHH_AGENT_STORE` overrides it. That directory is diagnostic output; do not add it to the repo `.gitignore` in this change.
- Follow existing naming: `SubagentTool`, `AgentToolResult`, `AgentEvent`, `TranscriptItem`.
- TDD: each task writes a failing test first.
- Commits use `feat(subagent): <ascii subject>`; stage only files touched by the task.

## Decisions (with rationale)

| # | claw-code mechanism | Decision | Why |
|---|---|---|---|
| 1 | Lifecycle manifest | **Port, redesigned** — structured events + failure classes + derived state | This is the highest-value piece and the data source for display |
| 2 | Allowlist applied to provider tool defs | **Already satisfied** — `agent_loop.rs:411-413` builds `tool_definitions` from `config.tools`, which `subagent.rs:156` sets to filtered `child_tools` | No code needed; record in tests only |
| 3 | No-recursion guard | **Already satisfied** — `subagent.rs:274-278` depth guard plus `:115` tool exclusion, tested at `:390-417` | No code needed |
| 4 | Panic/terminal containment | **Port** — async equivalent: finalize manifest on success, error, timeout, abort, `JoinError` | claw-code's panic case is the analogue of mehh's `JoinError` |
| 5 | Per-child provider fallback | **Defer** to separate project | Requires a `LlmProvider` chain type in `mehh-ai`; out of scope |
| 6 | Wire-format `tool_result` merge | **Audit only** — mehh has its own `transform_messages.rs` and provider transforms (`anthropic.rs:490-551`, `bedrock.rs:532-605`) that already combine tool results | Adding claw-code's fold would be redundant and risky |
| 7 | Steer/abort control | **Port, adapted** — `SubagentControl` registry + `AgentLoopConfig.steering_queue` | claw-code's `TurnControlSignal` is session-level; mehh already has a per-loop queue that can be handed to a child |

Additional decisions made here (builder may treat as settled):

- Child IDs use `uuid::Uuid::now_v7()` prefixed `sub-`, sortable and collision-free.
- Structured updates travel in `AgentToolResult.details` as `{"subagent": {...}}`, not in text, so the TUI can branch on it. Text updates remain for backward compatibility with non-interactive output.
- `SubagentControl` is optional on `SubagentTool` (`Option<Arc<...>>`) with a local `Semaphore(4)` fallback, so unit tests that construct `SubagentTool::new` directly keep working unchanged.
- `kill` cancels one child's token; it never calls `AgentSession::abort`.

## What is left to the builder

- Exact file set, task order, signatures, and tests are in `docs/superpowers/plans/2026-11-18-subagent-port-claw-code.md`.
- The implementer chooses the body of each function; signatures and tests pin the behavior.
- If `cargo test -p mehh-coding-agent` reveals a pre-existing failure unrelated to this change, stop and report rather than patching around it.
`````

## docs/superpowers/specs/2026-11-20-review-suggestions-design.md

`````markdown
# Review follow-ups design (2026-11-20)

## Intent

The independent reviewer passed the current patch set (`passed=true`, no security or
logic errors) but left four non-blocking suggestions. This spec turns those four into
work that can be done tomorrow without re-deriving the context.

Source of truth for each item is the verified finding, not the reviewer's wording:

1. `compaction.rs:534-541` — `compact_with_provider` keeps the head of a chronological
   transcript and drops the tail, while its marker claims older history was truncated.
2. `interactive.rs:2497-2503` — `expand_prompt_with_files` uses `split_whitespace()` and
   rejoins with `" "`, flattening line breaks and breaking `@path` values with spaces.
3. `main.rs:1011-1013` — piped stdin is read whenever stdin is not a terminal, even when
   a prompt was supplied; `read_to_string` waits for EOF. HEAD only read stdin when the
   prompt was empty.
4. `file_args.rs:79` — `path_str` is interpolated into `<file name="...">` unescaped.

## Goals

- Summary prompt keeps the most recent history, and its marker matches what was cut.
- TUI prompts preserve exact whitespace, and quoted `@"path with spaces"` works.
- Print mode never blocks on an open stdin pipe when a prompt argument already exists.
- File-name markup is escaped before it enters the LLM prompt.

## Non-goals

- No new CLI flag for combining piped stdin with a prompt argument.
- No refactor of `interactive.rs` into a module directory in this batch.
- No changes to the already-passing MOIM, Lead/Worker, MCP sync, or Windows URI fixes.
- No commit unless explicitly requested later.

## Constraints

- Rust workspace; run debug tests only (`cargo test -p ... --lib`), no release builds.
- TDD: every fix gets a failing test first, then minimal code, then a green run.
- Do not touch unrelated uncommitted work from other sessions, especially the stdin
  region of `crates/mehh-cli/src/main.rs`; confirm ownership before editing it.
- Reuse `crate::process_file_arguments`; do not duplicate file-read logic in `interactive.rs`.
- Keep the existing public signatures of `compact_with_provider`,
  `expand_prompt_with_files`, `process_file_arguments`, and `run_print_flow` unchanged.
`````

## docs/superpowers/specs/2026-11-21-tui-ux-optimization-design.md

`````markdown
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
`````

