# Wave 1 — Loop Resilience, Context Recovery & Dynamic Models Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Rust agent loop survive transient provider/network failures (`429`/`5xx`/timeouts), proactively compact proportionally across small and large context windows, recover safely from mid-turn overflow without replaying side-effecting tools, preserve CLI system prompts across tool refreshes, and load dynamic OpenAI-compatible/custom model configs via `models.json`.

**Architecture:** `mehh-ai` gains pure, provider-agnostic modules (`retry.rs` and `overflow.rs`) consumed identically by all 7 `reqwest`-based providers and upper session layers. `mehh-coding-agent` adds proportional budget helpers in `compaction.rs`, `Settings::effective_auto_compact`, prompt-override retention plus Case A/B overflow recovery in `agent_session.rs` and `interactive.rs`, and `ModelRegistry` (`models_config.rs`) wired across `AgentSession`, `rpc.rs`, `interactive.rs`, and `mehh-cli`.

**Tech Stack:** Rust workspace crates (`tokio`, `tokio-util`, `reqwest`, `serde`, `serde_json`, `regex`, `uuid`, `anyhow`, `thiserror` — zero new external crates).

**Spec:** [2026-11-13-strongest-rust-agent-design.md](../../specs/2026-11-13-strongest-rust-agent-design.md)

**Global Constraints:**
- Never execute shell commands (`"!cmd"`) inside `models.json` `apiKey` fields; only literal strings and `"$ENV_VAR"` are supported.
- Strip UTF-8 BOM when reading JSON configs (`models.json`); `ENOENT` maps to empty config, while syntax/schema errors must return explicit `Err` with file path context.
- Every sleep/request retry must honor `CancellationToken` immediately (`RetryFailure::Aborted`).
- Never retry permanent `4xx` responses (`400`, `401`, `403`, `404`, `413`, `422`) or mid-stream SSE errors after HTTP `200`.
- After every wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with exit code `0`.

**Review Focus:**
- **Abort during retry sleep:** Covered by `retry::tests::send_with_retry_aborts_immediately_during_backoff`.
- **No replay of completed mid-turn tool calls on overflow:** Covered by `agent_session::tests::prompt_case_b_mid_turn_overflow_resumes_without_reexecuting_tools`.
- **CLI `--system-prompt` / `--append-system-prompt` preserved across tool reloads:** Covered by `agent_session::tests::enable_mcp_and_codemode_preserves_prompt_overrides`.
- **BOM + syntax error fail-closed + `$ENV_VAR` unset handling + `set_model` key reset in `models.json`:** Covered by `models_config::tests` and `agent_session::tests::set_model_clears_cached_key_and_resolves_from_registry`.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Create | `crates/mehh-ai/src/retry.rs` | `RetryPolicy`, `RetryFailure`, `send_with_retry`, `Retry-After` parsing, jitter backoff |
| Create | `crates/mehh-ai/src/overflow.rs` | `is_context_overflow` provider error + silent/length-stop overflow classifier |
| Modify | `crates/mehh-ai/src/lib.rs` | Export and re-export `retry` and `overflow` |
| Modify | `crates/mehh-ai/src/{anthropic,openai,openai_responses,google,mistral,azure_openai,bedrock}.rs` | Route HTTP `send()` through `send_with_retry` |
| Modify | `crates/mehh-coding-agent/src/compaction.rs` | `DEFAULT_RESERVE_TOKENS`, `effective_reserve_tokens`, `effective_keep_recent_tokens`, `should_auto_compact` |
| Modify | `crates/mehh-coding-agent/src/settings.rs` | Add `Settings::effective_auto_compact(&self) -> bool` defaulting to `true` |
| Create | `crates/mehh-coding-agent/src/models_config.rs` | `ModelsConfigFile`, `ProviderConfig`, `ModelEntryConfig`, `ModelOverrideConfig`, `ModelRegistry`, `$ENV_VAR` lookup |
| Modify | `crates/mehh-coding-agent/src/lib.rs` | Export `models_config` |
| Modify | `crates/mehh-coding-agent/src/agent_session.rs` | Prompt overrides (`set_prompt_overrides`, `rebuild_system_prompt`), proactive compaction, Case A/B overflow retry, `model_registry` + `explicit_cli_api_key` key precedence |
| Modify | `crates/mehh-coding-agent/src/interactive.rs` | `auto_compact` default alignment, proactive compaction status, `overflow_retried` Case A/B handling, `/model` registry lookup |
| Modify | `crates/mehh-coding-agent/src/rpc.rs` | Resolve models & clear/re-resolve API keys via `session.model_registry()` in `set_model` and `get_available_models` |
| Modify | `crates/mehh-cli/src/main.rs` | Load `ModelRegistry`, set prompt overrides & explicit CLI key on `AgentSession`, list custom models under `--list-models` |

---

### Task 1: Unified HTTP Retry & Backoff in `mehh-ai`

**Files:**
- Create: `crates/mehh-ai/src/retry.rs`
- Modify: `crates/mehh-ai/src/lib.rs`
- Modify: `crates/mehh-ai/src/anthropic.rs:60-104`
- Modify: `crates/mehh-ai/src/openai.rs:60-104`
- Modify: `crates/mehh-ai/src/openai_responses.rs:78-120`
- Modify: `crates/mehh-ai/src/google.rs:70-118`
- Modify: `crates/mehh-ai/src/mistral.rs:245-292`
- Modify: `crates/mehh-ai/src/azure_openai.rs:120-168`
- Modify: `crates/mehh-ai/src/bedrock.rs:920-978`

**Interfaces:**
- **Consumes:** `reqwest::RequestBuilder`, `reqwest::Response`, `reqwest::StatusCode`, `tokio_util::sync::CancellationToken`
- **Produces:** `mehh_ai::retry::{RetryPolicy, RetryFailure, is_retryable_status, parse_retry_after, send_with_retry}`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/retry.rs`**

Add `crates/mehh-ai/src/retry.rs` starting with the public types stub and `tokio::net::TcpListener` unit tests asserting:
1. `send_with_retry_retries_429_then_succeeds`: mock server returns `429 Too Many Requests` with `Retry-After: 0` twice, then `200 OK`; total requests observed == `3`.
2. `send_with_retry_fails_fast_on_400_and_413`: mock server returns `400 Bad Request` (and separately `413 Payload Too Large`); `send_with_retry` returns `Ok(resp)` on the very first response without retrying (`request_count == 1`).
3. `send_with_retry_exhausts_503_after_max_retries`: mock server always returns `503 Service Unavailable`; with `max_retries = 2`, total attempts == `3`, final response status == `503`.
4. `send_with_retry_aborts_immediately_during_backoff`: mock server returns `503` once, `cancel.cancel()` fires after `20ms` while `initial_backoff = 5s`; `send_with_retry` returns `Err(RetryFailure::Aborted)` in `< 300ms`.

- [ ] **Step 2: Export `pub mod retry;` in `crates/mehh-ai/src/lib.rs` and verify RED**

Run: `cargo test -p mehh-ai retry`
Expected: FAIL (`todo!()` panic or unimplemented `send_with_retry`).

- [ ] **Step 3: Implement `crates/mehh-ai/src/retry.rs`**

Implement:
- `pub struct RetryPolicy { pub max_retries: u32, pub initial_backoff: Duration, pub max_backoff: Duration, pub max_retry_after: Duration }` with `Default` (`3`, `500ms`, `10s`, `30s`).
- `#[derive(Debug, Clone, PartialEq, Eq)] pub enum RetryFailure { Aborted, Transport(String) }` with `Display` / `std::error::Error`.
- `pub fn is_retryable_status(status: reqwest::StatusCode) -> bool` matching `408 | 429 | 500 | 502 | 503 | 504 | 529`.
- `pub fn parse_retry_after(headers: &reqwest::header::HeaderMap, cap: Duration) -> Option<Duration>` parsing integer seconds from `Retry-After` and clamping to `cap`.
- `pub async fn send_with_retry<F>(mut build_req: F, policy: &RetryPolicy, cancel: &CancellationToken) -> Result<reqwest::Response, RetryFailure> where F: FnMut() -> reqwest::RequestBuilder`:
  - Loop `attempt in 0..=policy.max_retries`:
  - Select `cancel.cancelled()` vs `build_req().send()`:
    - On `Ok(resp)`: if `!is_retryable_status(resp.status())` or `attempt == policy.max_retries`, return `Ok(resp)` so callers format non-2xx status+body uniformly. Otherwise compute `delay` (`parse_retry_after` or exponential `initial_backoff * 2^attempt` capped at `max_backoff` + up to 15% jitter from `Uuid::now_v7().as_bytes()[15]`), drop `resp`, and `tokio::select! { _ = cancel.cancelled() => return Err(RetryFailure::Aborted), _ = tokio::time::sleep(delay) => {} }`.
    - On `Err(err)`: let `retryable = err.is_timeout() || err.is_connect() || err.is_request();`. If `!retryable || attempt == policy.max_retries`, return `Err(RetryFailure::Transport(err.to_string()))`. Otherwise sleep with exponential backoff + cancellation select.

- [ ] **Step 4: Run retry tests to verify GREEN**

Run: `cargo test -p mehh-ai retry`
Expected: PASS (4 passed, 0 failed).

- [ ] **Step 5: Wire all 7 `reqwest` providers to `send_with_retry`**

In each of `anthropic.rs`, `openai.rs`, `openai_responses.rs`, `google.rs`, `mistral.rs`, `azure_openai.rs`, and `bedrock.rs`, factor request construction into a helper/closure returning `reqwest::RequestBuilder` (including headers, URL, and JSON/bytes body via cloned `body`/`body_bytes` — note in `bedrock.rs`, re-build inside closure or clone `sig_headers`/`body_bytes`), call `send_with_retry(|| ..., &RetryPolicy::default(), &cancel).await`, and map `RetryFailure::Aborted` -> `StopReason::Aborted` (`"Request was cancelled"`) and `RetryFailure::Transport(msg)` -> `StopReason::Error` (`msg`). Keep the existing `!resp.status().is_success()` status+body error emission unchanged.

- [ ] **Step 6: Verify `mehh-ai` compiles, tests pass, and clippy is clean**

Run:
`cargo test -p mehh-ai`
`cargo clippy -p mehh-ai --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 2: Context Overflow Classifier in `mehh-ai`

**Files:**
- Create: `crates/mehh-ai/src/overflow.rs`
- Modify: `crates/mehh-ai/src/lib.rs`

**Interfaces:**
- **Consumes:** `crate::types::{AssistantMessage, StopReason}`
- **Produces:** `pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/overflow.rs`**

Write unit tests covering:
1. Provider error strings (`Anthropic` `"prompt is too long: 210000 tokens > 200000"`, `OpenAI` `"Your input exceeds the context window of this model"`, `Google` `"The input token count (1050000) exceeds the maximum number of tokens allowed"`, `Bedrock` `"input is too long for requested model"`, `llama.cpp` `"request exceeds the available context size"`, `"HTTP 413 Payload Too Large"`).
2. Non-overflow exclusions: `"rate limit exceeded"`, `"429 Too many requests"`, `"Throttling error: Too many tokens per day"` must return `false`.
3. Silent overflow (`StopReason::Stop` with `usage.input + usage.cache_read > context_window`) returns `true`.
4. Saturated length overflow (`StopReason::Length` with `usage.output == 0` and `usage.input + usage.cache_read >= context_window * 99 / 100`) returns `true`, while normal `StopReason::Length` with `usage.output > 0` returns `false`.

- [ ] **Step 2: Export `pub mod overflow;` in `crates/mehh-ai/src/lib.rs` and run RED**

Run: `cargo test -p mehh-ai overflow`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/mehh-ai/src/overflow.rs`**

Port the exact `NON_OVERFLOW_PATTERNS` and `OVERFLOW_PATTERNS` regex tables using `std::sync::OnceLock<Vec<Regex>>`:
- Check `StopReason::Error` error text against `NON_OVERFLOW_PATTERNS` (early return `false`) then `OVERFLOW_PATTERNS` (including bare `^4(13)\b` or `status code:\s*413` / `HTTP 413`).
- When `context_window > 0`:
  - `let total_input = message.usage.input.saturating_add(message.usage.cache_read);`
  - Return `true` on `StopReason::Stop && total_input > context_window`.
  - Return `true` on `StopReason::Length && message.usage.output == 0 && total_input >= context_window.saturating_mul(99) / 100`.

- [ ] **Step 4: Run overflow tests and clippy to verify GREEN**

Run:
`cargo test -p mehh-ai overflow`
`cargo clippy -p mehh-ai --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 3: Proportional Compaction Budgets & `effective_auto_compact` Default

**Files:**
- Modify: `crates/mehh-coding-agent/src/compaction.rs`
- Modify: `crates/mehh-coding-agent/src/settings.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:333-348`
- Modify: `crates/mehh-coding-agent/src/interactive.rs:453-485`

**Interfaces:**
- **Consumes:** `crate::session::SessionEntry`, `mehh_ai::Model.context_window`
- **Produces:** `compaction::{DEFAULT_RESERVE_TOKENS, effective_reserve_tokens, effective_keep_recent_tokens, should_auto_compact}`, `Settings::effective_auto_compact`

- [ ] **Step 1: Write failing tests in `compaction.rs` and `settings.rs`**

1. In `compaction.rs`:
   - `test_effective_budgets_scale_for_small_and_large_windows`: asserts `effective_reserve_tokens(200_000) == 16_384`, `effective_keep_recent_tokens(200_000) == 20_000`, `effective_reserve_tokens(8_192) == 2_048`, `effective_keep_recent_tokens(8_192) == 4_096`, `effective_reserve_tokens(2_048) == 512`, `effective_reserve_tokens(0) == 0`.
   - `test_should_auto_compact_small_8k_window`: creates an active session with ~`6_500` estimated tokens across multiple messages on `context_window = 8_192`; asserts `should_auto_compact(&entries, 8_192) == true` (while `<= 4_000` estimated tokens or a single huge message that `prepare_compaction` cannot compact returns `false`).
2. In `settings.rs`:
   - `test_effective_auto_compact_defaults_to_true`: asserts `Settings::default().effective_auto_compact() == true` and explicit `Some(false)` returns `false`.

- [ ] **Step 2: Run targeted tests to verify RED**

Run: `cargo test -p mehh-coding-agent effective_budgets effective_auto_compact`
Expected: FAIL.

- [ ] **Step 3: Implement proportional budgets & setting default**

1. In `crates/mehh-coding-agent/src/compaction.rs`:
   - `pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;`
   - `pub fn effective_reserve_tokens(context_window: u64) -> usize`:
     ```rust
     let cw = context_window as usize;
     if cw == 0 {
         return 0;
     }
     let min_floor = if cw >= 2048 { 512 } else { 1 };
     DEFAULT_RESERVE_TOKENS.min(cw / 4).max(min_floor)
     ```
   - `pub fn effective_keep_recent_tokens(context_window: u64) -> usize`:
     `DEFAULT_KEEP_RECENT_TOKENS.min((context_window as usize) / 2)`
   - `pub fn should_auto_compact(entries: &[SessionEntry], context_window: u64) -> bool`:
     If `context_window == 0`, return `false`. Sum `estimate_message_tokens` after the latest `Compaction` summary entry (reuse existing active-slice logic inside `compaction.rs`), compute `threshold = (context_window as usize).saturating_sub(effective_reserve_tokens(context_window))`, and return `active_tokens >= threshold && prepare_compaction(entries, effective_keep_recent_tokens(context_window)).is_some()`.
2. In `crates/mehh-coding-agent/src/agent_session.rs:342-345`: replace `DEFAULT_KEEP_RECENT_TOKENS` with `crate::compaction::effective_keep_recent_tokens(self.model.context_window)`.
3. In `crates/mehh-coding-agent/src/settings.rs`: add `pub fn effective_auto_compact(&self) -> bool { self.auto_compact.unwrap_or(true) }`.
4. In `crates/mehh-coding-agent/src/interactive.rs`: update `settings_selector_items` (`line 455`) and `apply_setting_cycle` (`auto_compact` arm) to read `settings.effective_auto_compact()`.

- [ ] **Step 4: Run targeted tests to verify GREEN**

Run: `cargo test -p mehh-coding-agent compaction settings`
Expected: PASS.

---

### Task 4: Prompt Override Preservation & Safe Case A / Case B Overflow Recovery

**Files:**
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-coding-agent/src/interactive.rs`
- Modify: `crates/mehh-cli/src/main.rs:513-526`

**Interfaces:**
- **Consumes:** `mehh_ai::is_context_overflow`, `compaction::should_auto_compact`, `mehh_agent::run_agent_loop(vec![], history, config, on_event)`
- **Produces:** `AgentSession::{set_prompt_overrides, rebuild_system_prompt}`, proactive compaction + single-retry Case A/B recovery in `AgentSession::prompt` and interactive turn loop

- [ ] **Step 1: Write failing tests in `agent_session.rs` and `interactive.rs`**

Write `FauxProvider`-based tests covering:
1. `enable_mcp_and_codemode_preserves_prompt_overrides`: call `session.set_prompt_overrides(Some("CUSTOM SYS".into()), Some("APPEND TAIL".into()))`, then `session.enable_mcp_and_codemode(vec![], true)`; assert `session.system_prompt` contains both `"CUSTOM SYS"` and `"APPEND TAIL"` plus `"codemode"`.
2. `prompt_proactively_compacts_before_turn_when_over_threshold`: populate session near threshold on a `8_192`-window faux model with `FauxProvider` returning a summary on the compaction call then a final assistant reply on the prompt call; verify history compacted before sending user prompt and assistant response succeeds.
3. `prompt_case_a_turn0_overflow_compacts_and_retries_once`: seed compactable prior history, queue `AssistantMessage` overflow error `"prompt is too long"` (`StopReason::Error`) -> compaction summary -> valid final reply; verify final returned messages end with the valid reply and session contains compaction entry + 1 clean user turn (no permanent overflow error left on active turn).
4. `prompt_case_b_mid_turn_overflow_resumes_without_reexecuting_tools`: use an `AtomicUsize`-counting mock `AgentTool` (`write_counter`); `FauxProvider` turn returns: (1) assistant tool call calling `write_counter`, (2) after tool runs, assistant overflow error `"prompt is too long"`, (3) compaction summary, (4) final assistant reply `"Done after compact"`. Assert `write_counter.load(Ordering::SeqCst) == 1` (never re-executed!), returned/persisted messages include executed tool result + compaction + final assistant reply `"Done after compact"`.
5. `prompt_overflow_retry_bounded_to_one`: two consecutive overflow errors fail after at most 1 compaction retry (never loops infinitely).

- [ ] **Step 2: Run targeted tests to verify RED**

Run: `cargo test -p mehh-coding-agent prompt_case_ enable_mcp_and_codemode_preserves`
Expected: FAIL.

- [ ] **Step 3: Implement prompt override preservation & `AgentSession::prompt` lifecycle**

1. Add `pub custom_prompt: Option<String>` and `pub append_prompt: Option<String>` (`None` by default) to `AgentSession`.
2. Add `pub fn set_prompt_overrides(&mut self, custom_prompt: Option<String>, append_prompt: Option<String>)` and `pub fn rebuild_system_prompt(&mut self)` (using `build_system_prompt_with_skills(&self.cwd, self.custom_prompt.as_deref(), self.append_prompt.as_deref(), &tool_names, &self.skills)`).
3. Update `enable_mcp_and_codemode` to call `self.rebuild_system_prompt()` instead of passing `None, None`. Update `crates/mehh-cli/src/main.rs` lines 513-526 to call `agent_session.set_prompt_overrides(args.system_prompt.clone(), args.append_system_prompt.clone())` and `agent_session.rebuild_system_prompt()`.
4. Add helper `fn last_message_is_overflow(messages: &[Message], context_window: u64) -> bool`.
5. Update `AgentSession::prompt`:
   - If `self.settings.effective_auto_compact()` && proactive `should_auto_compact(&active_entries, self.model.context_window)`, run `let _ = self.compact(None).await;` (if compaction transiently errs, log warning to stderr or emit non-fatal status and proceed).
   - Run first turn via `prepare_prompt` + `run_agent_loop`. Clear `active_cancel`.
   - If `self.settings.effective_auto_compact()` && `last_message_is_overflow(&new_messages, self.model.context_window)`:
     - Distinguish Case A vs Case B:
       `let has_completed_tool = new_messages.iter().any(|m| matches!(m, Message::ToolResult(_)));`
       - **Case A (`!has_completed_tool`):** Run `match self.compact(None).await`: if `Ok(Some(_))`, prepare fresh config/history and run `run_agent_loop(vec![user_msg_fresh], fresh_history, fresh_config, on_event)` once, then `finish_prompt(&retry_messages)?` and return `Ok(retry_messages)`. Otherwise (`Ok(None)` or `Err`), persist original `new_messages` and return `Ok(new_messages)`.
       - **Case B (`has_completed_tool`):** Take `prefix = &new_messages[..new_messages.len() - 1]` (preserving `[UserPrompt, AssistantToolCall, ToolResult, ...]` before trailing overflow `AssistantMessage`), call `self.finish_prompt(prefix)?`, then `if let Ok(Some(_)) = self.compact(None).await`: build fresh `history = self.session.build_context_messages()` and `config` with `cancel`, call `run_agent_loop(vec![], history, config, on_event).await`, call `self.finish_prompt(&continuation)?`, combine `prefix + continuation` and return! (If compaction yields `None`/`Err`, persist original `new_messages` instead so failure isn't lost.)
   - Otherwise `self.finish_prompt(&new_messages)?` and return `Ok(new_messages)`.

- [ ] **Step 4: Wire proactive compaction & Case A/B overflow recovery in `crates/mehh-coding-agent/src/interactive.rs`**

1. Add `pub overflow_retried: bool` and `pub pending_retry_prompt: Option<String>` to `InteractiveState`.
2. Before `start_turn` on interactive submit: reset `state.overflow_retried = false`, `state.pending_retry_prompt = Some(trimmed.to_string())`; if `session.settings.effective_auto_compact()` && `should_auto_compact`, set `state.is_busy = true; state.status = Some("Auto-compacting context...".to_string()); render_now(...)`, call `let _ = session.compact(None).await`.
3. In the interactive loop when `AgentSignal::Finished(messages)` arrives: check `if session.settings.effective_auto_compact() && !state.overflow_retried && last_message_is_overflow(&messages, session.model.context_window)`:
   - Set `state.overflow_retried = true`.
   - Perform Case A (compact then re-run `start_turn` without adding a duplicate `TranscriptItem::User`) or Case B (`finish_prompt` prefix, `compact(None).await`, then spawn continuation `run_agent_loop(vec![], compacted_history, config, Some(on_event))` cleanly via `resume_mid_turn_after_compaction`).
   - Write an interactive unit test verifying Case A / Case B state transition helpers in `interactive.rs`.

- [ ] **Step 5: Run tests & clippy to verify GREEN**

Run:
`cargo test -p mehh-coding-agent`
`cargo clippy -p mehh-coding-agent --all-targets -- -D warnings`
Expected: PASS, 0 warnings.

---

### Task 5: Custom Model & Endpoint Registry (`models.json`)

**Files:**
- Create: `crates/mehh-coding-agent/src/models_config.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs`
- Modify: `crates/mehh-coding-agent/src/rpc.rs:211-285`
- Modify: `crates/mehh-coding-agent/src/interactive.rs:671-699,1657-1670`
- Modify: `crates/mehh-cli/src/main.rs:399-496`

**Interfaces:**
- **Consumes:** `mehh_ai::{Model, ModelCost, catalog::builtin_models}`, `crate::auth::resolve_api_key_async`
- **Produces:** `ModelRegistry::{load, resolve_model, list_models, resolve_provider_api_key, resolve_env_value}` and `AgentSession::{with_model_registry, set_explicit_cli_api_key, resolve_and_refresh_api_key}`

- [ ] **Step 1: Write failing tests in `crates/mehh-coding-agent/src/models_config.rs` & `agent_session.rs`**

Write tests asserting:
1. `models_json_strips_bom_and_allows_enoent`: missing file -> empty `ModelsConfigFile`; UTF-8 BOM prefix -> parses cleanly; malformed JSON -> returns `Err` containing file path.
2. `models_json_interpolates_env_vars_without_mutating_disk`: `"$TEST_VAR"` resolves when set and yields `None` (key) / header omitted (headers map) when unset; literal strings pass through unchanged.
3. `models_json_supports_custom_provider_and_provider_scoped_model_overrides`: `"ollama"` adds `qwen2.5-coder:32b` with custom `baseUrl`, `api: "openai-completions"`, `headers`, `contextWindow: 65536`; `"openrouter"` with `modelOverrides["anthropic/claude-sonnet-4-5"]` (slashed model ID) overrides `baseUrl` and `headers` while preserving remaining catalog defaults.
4. `set_model_clears_cached_key_and_resolves_from_registry`: switching from Anthropic (with cached `"sk-ant-cached"`) to custom `"ollama"` (with `apiKey: "ollama-local"`) clears stale `"sk-ant-cached"` and sets `session.api_key == Some("ollama-local")`; switching to local provider with unset `"$MISSING_OLLAMA_KEY"` clears key to `None`; CLI `--api-key` override takes precedence over both `auth.json` and `models.json`.

- [ ] **Step 2: Expose `pub mod models_config;` and run RED**

Run: `cargo test -p mehh-coding-agent models_config set_model_clears_cached_key`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/mehh-coding-agent/src/models_config.rs`**

Implement serde structs (`#[serde(rename_all = "camelCase")]`):
- `ModelsConfigFile { pub providers: HashMap<String, ProviderConfig> }`
- `ProviderConfig { base_url: Option<String>, api: Option<String>, api_key: Option<String>, headers: Option<HashMap<String, String>>, models: Option<Vec<ModelEntryConfig>>, model_overrides: Option<HashMap<String, ModelOverrideConfig>> }`
- `ModelEntryConfig` (`id`, `name`, `api`, `reasoning`, `input`, `cost`, `context_window`, `max_tokens`, `headers`, `compat` optional ignored fields if present cleanly).
- `ModelOverrideConfig` (`name`, `base_url`, `api`, `api_key`, `reasoning`, `input`, `cost`, `context_window`, `max_tokens`, `headers`).
- Env interpolation helper `resolve_env_or_literal(raw: &str) -> Option<String>`: if `let Some(var) = raw.strip_prefix('$')` -> `std::env::var(var).ok().filter(|v| !v.is_empty())`, else `Some(raw.to_string())`. Apply same helper cleanly to both header values (omit header if `None`) and `api_key`. Note: NEVER implement `!shell_command` execution in Wave 1.
- `ModelRegistry::from_configs(global: Option<ModelsConfigFile>, project: Option<ModelsConfigFile>)`: merges built-in catalog + custom provider models (inheriting provider `base_url`, `api`, merged `headers`) + applies `providers.<provider>.model_overrides.<model_id>` cleanly. Lookup order supports `resolve_model(query: &str, provider_hint: Option<&str>) -> Option<Model>` (`provider/id` split on first `/`, exact match under `provider`, else unique suffix/id match) and `resolve_api_key(&self, provider: &str, model_id: &str) -> Option<String>`.

- [ ] **Step 4: Wire `ModelRegistry` & key precedence across `AgentSession`, `rpc.rs`, `interactive.rs`, and `mehh-cli`**

1. On `AgentSession`: add `pub model_registry: ModelRegistry` and `pub explicit_cli_api_key: Option<String>`.
2. Add `pub async fn refresh_api_key(&mut self) -> Result<()>` enforcing exact precedence:
   `explicit_cli_api_key` -> `resolve_api_key_async(&self.model.provider, None, None).await?` -> `self.model_registry.resolve_api_key(&self.model.provider, &self.model.id)`.
   Call `refresh_api_key` if `self.api_key.is_none()` (excluding `faux`) inside `AgentSession::prompt`.
3. Update `AgentSession::set_model(&mut self, model: Model, provider: Arc<dyn LlmProvider>)` to clear `self.api_key = self.explicit_cli_api_key.clone()` (so switching providers never leaks the previous provider's credential).
4. Add `AgentSession::switch_model_and_refresh_key(&mut self, model: Model) -> Result<()>` (builds provider via `default_provider_for_model(&model)`, calls `set_model`, then calls `refresh_api_key().await?`) and call it from:
   - `crates/mehh-coding-agent/src/interactive.rs`: `/model <arg>` and `SelectorKind::Model` selection (also sourcing model list from `session.model_registry.list_models()`).
   - `crates/mehh-coding-agent/src/rpc.rs`: `set_model` (looking up `session.model_registry.resolve_model(model_id, Some(provider))` before falling back) and `get_available_models` (`session.model_registry.list_models()`).
5. In `crates/mehh-cli/src/main.rs`:
   - Load `ModelRegistry::load(&cwd)?`.
   - Use registry under `--list-models` and startup model resolution (`registry.resolve_model(...)` merged with fallback).
   - Populate `agent_session.model_registry = registry`, `agent_session.explicit_cli_api_key = args.api_key.clone()`, then call `agent_session.refresh_api_key().await?`.

- [ ] **Step 5: Run full workspace tests & clippy**

Run:
`cargo test --workspace`
`cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (0 failures, 0 warnings).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-11-13-wave-1-loop-and-models.md`. Two execution options:
1. **Subagent-Driven (`superpowers:subagent-driven-development`)** — dispatch one fresh coding subagent per task + two-stage spec/code review between tasks.
2. **Inline (`superpowers:executing-plans`)** — implement task-by-task directly in the current session with verification after each task.
