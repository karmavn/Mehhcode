# Dump 10 - Implementation plans (nguyen si)

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| docs/superpowers/plans/2026-11-13-wave-1-loop-and-models.md | 28 | 326 | 6d1941568cc2e2bc0f79c49ad72aaf36f78d216fd292508e6ba096819aa74703 |
| docs/superpowers/plans/2026-11-13-wave-2-tools-search-and-subagent.md | 20.8 | 257 | ebf26ea4d2b74f8883746348eced8aebf2c2fab86d6a733dc785b1049c4a6659 |
| docs/superpowers/plans/2026-11-13-wave-3-trust-and-tui.md | 20.2 | 232 | 67e3e2096e1eace4a25622f84ae54d0654ae85ffa56ce07f38d8c6abaeed3d2f |
| docs/superpowers/plans/2026-11-15-wave-4-ai-parity.md | 24.7 | 304 | 19435b514c638e58a6e41d57fcc8e9a1b2a531521b9a0b806ea02973bf528e66 |
| docs/superpowers/plans/2026-11-15-wave-5-agent-tools-session-cli.md | 21.2 | 314 | 182d9c1f641a01a2474e6ffd4d53e3adb02bfcf14164f9e654423ed6f5b18af7 |
| docs/superpowers/plans/2026-11-15-wave-6-mcp-tui-parity.md | 15.3 | 231 | 2ba5639f478e74f8095bb6c84130f778091cdddb3db2043b80990e8c1ba3fb15 |
| docs/superpowers/plans/2026-11-16-goose-parity-core-plan.md | 36.8 | 860 | b73e34be916c9db8bf6025a2fde526aba41289671ef4d36435727f087aeb63c6 |
| docs/superpowers/plans/2026-11-17-interactive-session-switcher-plan.md | 16.1 | 381 | 986fff26e5fa69a2c0a9147478d05765b83978b8269161deeb7830d553e5388c |
| docs/superpowers/plans/2026-11-18-subagent-port-claw-code.md | 38.2 | 635 | e4224a319dbce60b63a9aab3bc626b335329a213e0c1ab96bdc6def069e6c1f9 |
| docs/superpowers/plans/2026-11-20-review-suggestions.md | 13.1 | 244 | b063c7f4417e96bd00cf530975a258b640eaf0c0afea9bae26863c68e9133c8f |
| docs/superpowers/plans/2026-11-21-tui-ux-optimization.md | 44.5 | 762 | 2a9edcbdf64df58ba569b27da40ccf52204c3fd374d996b698539c36c217cf1c |

## docs/superpowers/plans/2026-11-13-wave-1-loop-and-models.md

`````markdown
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
`````

## docs/superpowers/plans/2026-11-13-wave-2-tools-search-and-subagent.md

`````markdown
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
`````

## docs/superpowers/plans/2026-11-13-wave-3-trust-and-tui.md

`````markdown
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
`````

## docs/superpowers/plans/2026-11-15-wave-4-ai-parity.md

`````markdown
# Wave 4 — `mehh-ai` Provider, Streaming & Auth Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Achieve behavioral and protocol parity for `mehh-ai` and OAuth credential locking by adding cross-model `transform_messages` + `split_error_tool_result_images` across all 6 providers, Anthropic `cache_control` + dynamic `anthropic-beta` + thinking/effort budgets + OAuth mimicry, tolerant `StreamingJsonParser` + `repair_json`, surrogate/escaped-surrogate sanitization, and process-safe `CredentialStore` locking for `auth.json` and `mcp-auth.json`.

**Architecture:** `crates/mehh-ai` gains focused modules (`unicode.rs`, `transform_messages.rs`, `streaming_json.rs`) and additive optional fields on `Model`, `StreamOptions`, and `oauth::CredentialStore`. All 6 provider builders (`anthropic`, `bedrock`, `google`, `mistral`, `openai`, `openai_responses` shared by `azure_openai`) route messages through `transform_messages` while removing superseded inline normalizers, and both file-backed auth stores (`crates/mehh-coding-agent/src/auth.rs` and `crates/mehh-mcp/src/oauth.rs`) serialize OAuth refreshes with `std::fs::File::lock` via `tokio::task::spawn_blocking`.

**Tech Stack:** Rust workspace crates (`tokio`, `reqwest`, `serde`, `serde_json`, `sha2`, `anyhow`, `regex`, `std::fs::File::lock` — zero new external crates).

**Spec:** [2026-11-15-ts-parity-waves-4-5-6-design.md](../../specs/2026-11-15-ts-parity-waves-4-5-6-design.md)

## Global Constraints

- Zero new external Cargo crates; use existing workspace dependencies plus `rustc 1.100.0` `std::fs::File::lock` / `try_lock`.
- All new fields on `Model`, `StreamOptions`, and wire types must use `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing cross-crate callers compile unchanged.
- Remove superseded inline normalizers (`mistral.rs` inline `tc`/`tr` normalizations, `openai_responses.rs` inline `split('|').next()`) instead of leaving parallel code paths.
- Never swallow internal JSON syntax, I/O, or lock failures via `.ok()` or `unwrap_or_default()`; distinguish `ENOENT` from parse errors and strip UTF-8 BOM on config reads.
- After Wave 4 completes, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with exit code `0`.

## Review Focus

- **Existing text block equal to image placeholder before an image block:** Covered by `transform_messages::tests::replace_images_deduplicates_preexisting_placeholder`.
- **Concurrent OAuth refresh on rotated single-use refresh token:** Covered by `auth::tests::file_credential_store_serializes_concurrent_refresh` (re-reads under lock and skips duplicate HTTP refresh once expiry moved forward).
- **Unpaired escaped UTF-16 surrogates (`\uD83D`) inside provider payloads:** Covered by `unicode::tests::sanitize_surrogates_strips_unpaired_escapes_preserves_valid_pairs`.
- **OAuth Anthropic bearer auth & Claude Code canonical tool casing roundtrip:** Covered by `anthropic::tests::oauth_token_switches_to_bearer_and_maps_claude_code_tool_names`.
- **Mistral 9-char alphanumeric collision across distinct foreign tool IDs:** Covered by `mistral::tests::mistral_normalizer_resolves_hash_collisions_deterministically`.

---

## File Structure

| Action | File | Responsibility |
| --- | --- | --- |
| Modify | `crates/mehh-ai/src/types.rs` | `ModelCompat`, `CacheRetention`, `AnthropicEffort`, `AnthropicThinkingDisplay`, `Model` & `StreamOptions` optional fields |
| Create | `crates/mehh-ai/src/unicode.rs` | `sanitize_surrogates` and `sanitize_json_surrogates` for unpaired UTF-16 escape/NUL cleanup |
| Create | `crates/mehh-ai/src/transform_messages.rs` | `transform_messages`, `split_error_tool_result_images`, `SplitErrorToolResult`, non-vision downgrade |
| Create | `crates/mehh-ai/src/streaming_json.rs` | `repair_json`, `parse_tool_call_arguments`, `StreamingJsonParser`, `ToolCallArgumentsStream` |
| Modify | `crates/mehh-ai/src/lib.rs` | Export and re-export `unicode`, `transform_messages`, `streaming_json` |
| Modify | `crates/mehh-ai/src/anthropic.rs` | Wire `transform_messages`, `split_error_tool_result_images`, `ToolCallArgumentsStream`, `cache_control`, `anthropic-beta`, `ThinkingBudgets`/effort, `is_oauth_token` |
| Modify | `crates/mehh-ai/src/bedrock.rs` | Wire `transform_messages`, `split_error_tool_result_images` (Claude), `ToolCallArgumentsStream`, `cachePoint`, `sanitize_surrogates` |
| Modify | `crates/mehh-ai/src/google.rs` | Wire `transform_messages` with `requires_tool_call_id` normalizer and `sanitize_surrogates` |
| Modify | `crates/mehh-ai/src/mistral.rs` | Upgrade `normalize_mistral_tool_call_id` to `MistralToolCallIdNormalizer`, wire `transform_messages`, remove inline `tc`/`tr` calls, add `sanitize_surrogates` |
| Modify | `crates/mehh-ai/src/openai.rs` | Wire `transform_messages` (pipe ID -> <=40 chars), `ToolCallArgumentsStream`, `sanitize_surrogates` |
| Modify | `crates/mehh-ai/src/openai_responses.rs` | Wire `transform_messages` (`{call_id}\|fc_{hash}`), remove inline `split('|').next()` hacks, wire `ToolCallArgumentsStream`, `sanitize_surrogates` |
| Modify | `crates/mehh-ai/src/oauth.rs` | Async `CredentialStore` trait and `InMemoryCredentialStore` |
| Modify | `crates/mehh-coding-agent/src/auth.rs` | `FileCredentialStore: CredentialStore` with `spawn_blocking` + `std::fs::File::lock` across read -> expiry check -> refresh -> write |
| Modify | `crates/mehh-mcp/src/oauth.rs` | Add `<path>.lock` advisory locking (`spawn_blocking` + `std::fs::File::lock`) to `McpAuthStore` |

---

### Task 1: `types.rs` Extensions & `unicode.rs` Surrogate Sanitization

**Files:**
- Modify: `crates/mehh-ai/src/types.rs:324-356`
- Create: `crates/mehh-ai/src/unicode.rs`
- Modify: `crates/mehh-ai/src/lib.rs`
- Modify (struct literal compatibility): `crates/mehh-ai/src/catalog.rs`, `crates/mehh-coding-agent/src/models_config.rs`, `crates/mehh-cli/src/main.rs`, and existing `Model { ... }` / `StreamOptions { ... }` test literals

**Interfaces:**
- **Consumes:** Existing `Model`, `StreamOptions`, `ThinkingLevel` in `crates/mehh-ai/src/types.rs`
- **Produces:**
  - `mehh_ai::types::{ModelCompat, CacheRetention, ThinkingBudgets, AnthropicEffort, AnthropicThinkingDisplay}` and optional fields on `Model` (`compat`, `thinking_level_map`) and `StreamOptions` (`cache_retention`, `thinking_budgets`, `thinking_budget_tokens`, `effort`, `thinking_display`, `interleaved_thinking`)
  - `mehh_ai::unicode::{sanitize_surrogates, sanitize_json_surrogates}`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/unicode.rs` and `types.rs`**

Add tests covering:
1. `types::tests::model_and_stream_options_default_new_optional_fields`: deserializing existing JSON without `compat` or `cacheRetention` succeeds with `None`, and serializing skips `None` fields.
2. `unicode::tests::sanitize_surrogates_strips_unpaired_escapes_preserves_valid_pairs`:
   - Input `"ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end"` -> `"ok \\uD83D\\uDE48 bad  tail  end"`.
   - Input containing NUL `'\0'` or unpaired replacement artifacts cleans cleanly while preserving valid multi-byte UTF-8 (`"Tiếng Việt 🙈"`).

- [ ] **Step 2: Export `pub mod unicode;` in `crates/mehh-ai/src/lib.rs` and verify RED**

Run: `cargo test -p mehh-ai unicode`
Expected: FAIL (function not implemented).

- [ ] **Step 3: Add `types.rs` structs/fields and implement `unicode.rs`**

In `crates/mehh-ai/src/types.rs`:
- Add `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)] #[serde(rename_all = "camelCase")] pub struct ModelCompat` with optional `bool` flags (`supports_long_cache_retention`, `supports_cache_control_on_tools`, `force_adaptive_thinking`, `supports_mid_convo_effort`, `supports_temperature`, `allow_empty_signature`, `supports_strict_tools`).
- Add `ThinkingBudgets` (`minimal`, `low`, `medium`, `high` optional `u64`), `CacheRetention` (`None | Short | Long`), `AnthropicEffort` (`Low | Medium | High | Xhigh | Max`), and `AnthropicThinkingDisplay` (`Summarized | Omitted`) with lowercase serde renames.
- Append `#[serde(default, skip_serializing_if = "Option::is_none")]` fields to `Model` (derive `Default`) and `StreamOptions`; propagate optional `compat`/`thinking_level_map` in `crates/mehh-coding-agent/src/models_config.rs` `ModelEntry` loading and add `..Default::default()` to existing `Model { ... }` / `StreamOptions { ... }` literals in `catalog.rs`, `mehh-cli/src/main.rs`, and tests.
In `crates/mehh-ai/src/lib.rs`:
- Follow the crate's existing export style (`pub mod unicode;` + `pub use unicode::*;`, and similarly for `transform_messages` / `streaming_json` in Tasks 2 & 4).
In `crates/mehh-ai/src/unicode.rs`:
- Implement `pub fn sanitize_surrogates(input: &str) -> String` stripping NUL bytes and unpaired escaped UTF-16 surrogates (`\uD800`..`\uDBFF` not followed by `\uDC00`..`\uDFFF`, and vice versa, case-insensitive hex) while keeping valid surrogate pairs, plus `pub fn sanitize_json_surrogates(value: &mut serde_json::Value)` walking strings recursively.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-ai unicode model_and_stream_options`
Expected: PASS.

- [ ] **Step 5: Verify workspace compiles cleanly after `Model`/`StreamOptions` changes**

Run: `cargo check --workspace`
Expected: PASS (exit code 0).

---

### Task 2: `transform_messages` & `split_error_tool_result_images`

**Files:**
- Create: `crates/mehh-ai/src/transform_messages.rs`
- Modify: `crates/mehh-ai/src/lib.rs`

**Interfaces:**
- **Consumes:** `crates/mehh-ai/src/types::{Message, SystemMessage, UserMessage, UserContent, UserContentBlock, AssistantMessage, AssistantContentBlock, ToolResultMessage, ToolResultContentBlock, ToolCall, TextContent, ImageContent, Model, StopReason, now_ms}`
- **Produces:**
  - `pub const NON_VISION_USER_IMAGE_PLACEHOLDER: &str`
  - `pub const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str`
  - `pub const ERROR_TOOL_IMAGE_PLACEHOLDER: &str`
  - `pub struct SplitErrorToolResult { pub content: Vec<ToolResultContentBlock>, pub caption: String, pub images: Vec<ImageContent> }`
  - `pub fn split_error_tool_result_images(msg: &ToolResultMessage) -> Option<SplitErrorToolResult>`
  - `pub fn transform_messages<F>(messages: &[Message], model: &Model, normalize_tool_call_id: Option<F>) -> Vec<Message> where F: FnMut(&str, &Model, &AssistantMessage) -> String`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/transform_messages.rs`**

Port TS regression scenarios (`#10995`, `#10794`) plus Review Focus case:
1. `cross_model_thinking_dropped_and_same_model_retained`: drops `Thinking` when `!is_same_model`; retains same-model thinking, keeps empty `Thinking` if `redacted == Some(true)` or `thinking_signature.is_some()`, drops unsigned empty thinking.
2. `cross_model_strips_signatures_and_maps_tool_call_ids`: strips `text_signature` and `thought_signature` on model mismatch; rewrites `ToolCall.id` and matching subsequent `ToolResultMessage.tool_call_id`.
3. `skips_errored_and_aborted_assistant_turns`: omits assistant turns with `StopReason::Error` or `StopReason::Aborted`.
4. `orphaned_tool_calls_synthesize_error_results_and_defer_system_messages`: inserts synthetic `ToolResultMessage` (`"No result provided"`, `is_error: true`) before a new `User` turn, a new `Assistant` turn, or end of transcript; holds mid-turn `Message::System` and emits it right after resolved/synthetic tool results.
5. `downgrade_unsupported_images_collapses_runs`: replaces user & tool images when `!model.input.iter().any(|i| i == "image")`.
6. `replace_images_deduplicates_preexisting_placeholder`: `[Text(NON_VISION_USER_IMAGE_PLACEHOLDER), Image(..)]` collapses to a single `Text(NON_VISION_USER_IMAGE_PLACEHOLDER)`.
7. `split_error_tool_result_images_extracts_images_and_caption`: returns `None` when `!is_error` or no images; when `is_error` with images, returns `ERROR_TOOL_IMAGE_PLACEHOLDER` blocks, exact caption, and extracted images.

- [ ] **Step 2: Export `pub mod transform_messages;` in `crates/mehh-ai/src/lib.rs` and verify RED**

Run: `cargo test -p mehh-ai transform_messages`
Expected: FAIL.

- [ ] **Step 3: Implement `transform_messages.rs`**

Implement exact two-pass TS algorithm (`downgrade_unsupported_images` -> Pass 1 `is_same_model` + `tool_call_id_map` -> Pass 2 `close_pending_tool_calls` + `held_system_messages` + `StopReason::Error | Aborted` skipping) and `split_error_tool_result_images`. Keep `replace_images_with_placeholder` shared via a small private trait or helper across `UserContentBlock` and `ToolResultContentBlock` (no duplicate loops).

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-ai transform_messages`
Expected: PASS.

---

### Task 3: Wire `transform_messages` & Provider ID Normalizers Across All 6 Providers

**Files:**
- Modify: `crates/mehh-ai/src/anthropic.rs:191-403`
- Modify: `crates/mehh-ai/src/bedrock.rs:372-520`
- Modify: `crates/mehh-ai/src/google.rs:201-350`
- Modify: `crates/mehh-ai/src/mistral.rs:40-185`
- Modify: `crates/mehh-ai/src/openai.rs:190-330`
- Modify: `crates/mehh-ai/src/openai_responses.rs:171-310`

**Interfaces:**
- **Consumes:** `transform_messages::transform_messages`, `transform_messages::split_error_tool_result_images`, `unicode::sanitize_surrogates`
- **Produces:**
  - `mistral::MistralToolCallIdNormalizer` (collision-resistant 9-char alphanumeric `id_map`/`reverse_map` upgrading `normalize_mistral_tool_call_id`)
  - Provider-normalized request payloads across all 6 builders

- [ ] **Step 1: Write failing provider payload tests across the 6 modules**

Add tests verifying:
1. `anthropic::tests::build_request_transforms_cross_model_and_splits_error_images`: foreign tool call ID with `|` is sanitized (`[^a-zA-Z0-9_-]` -> `_`, max 64), foreign thinking is dropped, and an error `ToolResultMessage` containing an image emits placeholder in `tool_result` plus caption + image block after the turn's tool results.
2. `bedrock::tests::build_request_splits_error_images_for_claude_and_normalizes_ids`: Claude Bedrock models apply `split_error_tool_result_images`; orphaned tool calls get synthetic `"No result provided"`.
3. `google::tests::build_request_normalizes_ids_for_gemini_3_and_drops_foreign_thinking`: `gemini-3-pro` normalizes foreign IDs (`requires_tool_call_id`) and strips foreign `thought_signature`.
4. `mistral::tests::mistral_normalizer_resolves_hash_collisions_deterministically`: distinct inputs colliding on the same 9-char prefix receive distinct deterministic 9-char alphanumeric IDs, and matching `ToolResult` adopts the mapped ID.
5. `openai::tests::build_request_normalizes_pipe_ids_to_40_chars`: `call_123|fc_456` collapses cleanly (`<= 40` chars or `prefix_shortHash`), matching tool result uses normalized ID.
6. `openai_responses::tests::build_request_normalizes_foreign_fc_item_ids`: foreign Responses/Completions tool calls rewrite `{call_id}|fc_{hash}` and orphan calls synthesize error results without relying on inline `.split('|').next()`.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test -p mehh-ai build_request_ mistral_normalizer`
Expected: FAIL.

- [ ] **Step 3: Wire `transform_messages`, normalizers, and `sanitize_surrogates` in all 6 providers**

- Update `anthropic.rs`, `bedrock.rs`, `google.rs`, `mistral.rs`, `openai.rs`, and `openai_responses.rs` (`azure_openai.rs` inherits this automatically via `build_openai_responses_request`) to transform `context.messages` via `transform_messages` before serializing messages, and pass text fields through `sanitize_surrogates`.
- In `mistral.rs`, upgrade `normalize_mistral_tool_call_id` using `MistralToolCallIdNormalizer` (`id_map`/`reverse_map` with `attempt` hash loop) and **delete** the separate inline normalizations at lines 133 and 152.
- In `openai_responses.rs`, **delete** the inline `.split('|').next()` workarounds at lines 256 and 269 (since `transform_messages` now guarantees normalized IDs).
- Wire `split_error_tool_result_images` in `anthropic.rs` and `bedrock.rs` (for Claude models), grouping consecutive `ToolResult` blocks and appending extracted images + captions at the end of that user turn.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-ai`
Expected: PASS.

---

### Task 4: `repair_json` & Incremental `StreamingJsonParser`

**Files:**
- Create: `crates/mehh-ai/src/streaming_json.rs`
- Modify: `crates/mehh-ai/src/lib.rs`
- Modify: `crates/mehh-ai/src/anthropic.rs:600-910`
- Modify: `crates/mehh-ai/src/openai.rs`
- Modify: `crates/mehh-ai/src/openai_responses.rs`
- Modify: `crates/mehh-ai/src/bedrock.rs`

**Interfaces:**
- **Consumes:** `crate::types::ToolCall`, `serde_json::{Map, Value}`
- **Produces:**
  - `pub fn repair_json(json: &str) -> String`
  - `pub fn parse_tool_call_arguments(json: Option<&str>) -> (serde_json::Map<String, serde_json::Value>, Option<String>)`
  - `pub struct StreamingJsonParser` + `pub struct ToolCallArgumentsStream` (`append(&mut self, delta: &str) -> serde_json::Map<String, serde_json::Value>`, `replace(&mut self, text: &str) -> serde_json::Map<String, serde_json::Value>`, `finish(self) -> (serde_json::Map<String, serde_json::Value>, Option<String>)`)

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/streaming_json.rs` and SSE provider tests**

Assert:
1. `repair_json_fixes_raw_newlines_and_invalid_escapes`: escapes raw `\n`/`\t`/control bytes inside JSON strings and doubles invalid `\d` escapes without breaking valid `\u0041`.
2. `parse_tool_call_arguments_rejects_non_object_with_ts_message`: JSON array `"[1,2]"` returns empty map + `Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]")`.
3. `streaming_parser_emits_partial_objects_across_chunks`: feeding `{"path":"src/` then `main.rs","line":` then `10}` yields `{"path": "src/"}` then `{"path": "src/main.rs"}` then `{"path": "src/main.rs", "line": 10}`.
4. `anthropic_sse_updates_partial_tool_arguments_incrementally`: verify `ToolCallDelta`'s `partial` `AssistantMessage` reflects parsed partial JSON arguments before `content_block_stop`.

- [ ] **Step 2: Export `pub mod streaming_json;` in `crates/mehh-ai/src/lib.rs` and verify RED**

Run: `cargo test -p mehh-ai streaming_json anthropic_sse_updates_partial`
Expected: FAIL.

- [ ] **Step 3: Implement `streaming_json.rs` and wire into SSE stream states**

Implement `repair_json`, `parse_tool_call_arguments`, and `StreamingJsonParser` / `ToolCallArgumentsStream` (tolerant close of unterminated strings/objects/arrays with amortized work bounds), then replace raw `json_buffer: String` + plain `serde_json::from_str` in `anthropic.rs`, `openai.rs`, `openai_responses.rs`, and `bedrock.rs` tool-call SSE accumulation so partial deltas update `ToolCall.arguments` live and final finish uses `parse_tool_call_arguments`.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-ai`
Expected: PASS.

---

### Task 5: Anthropic Caching, Beta Headers, Thinking/Effort Budgets & OAuth Mimicry

**Files:**
- Modify: `crates/mehh-ai/src/anthropic.rs`
- Modify: `crates/mehh-ai/src/bedrock.rs` (prompt `cachePoint` parity)

**Interfaces:**
- **Consumes:** `Model.compat`, `StreamOptions.{cache_retention, thinking_budgets, thinking_budget_tokens, effort, thinking_display, interleaved_thinking}`
- **Produces:**
  - `anthropic::is_oauth_token(api_key: &str) -> bool`
  - `anthropic::to_claude_code_name(name: &str) -> String` & `from_claude_code_name(name: &str, declared_tools: &[ToolDefinition]) -> String`
  - `anthropic::clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64` & `adjust_max_tokens_for_thinking(...) -> (u64, u64)`
  - `anthropic::resolve_anthropic_beta_headers(model: &Model, options: &StreamOptions, is_oauth: bool) -> String`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/anthropic.rs` and `bedrock.rs`**

Assert:
1. `cache_control_placed_on_system_last_tool_and_last_turn_message`: with `CacheRetention::Short`, `"cache_control": {"type": "ephemeral"}` appears on system block, final tool (when compat permits), and final block of last message; with `CacheRetention::Long` and `supports_long_cache_retention = Some(true)`, includes `"ttl": "1h"`; with `CacheRetention::None`, omitted everywhere.
2. `thinking_budgets_adaptive_and_disabled_modes`: tests fixed budget clamping (`MIN_ANSWER_TOKENS = 1024`), `force_adaptive_thinking = Some(true)` emitting `{"type":"adaptive","display":"summarized"}` + `output_config.effort`, and explicit off emitting `{"type":"disabled"}`.
3. `oauth_token_switches_to_bearer_and_maps_claude_code_tool_names`: `sk-ant-oat...` uses `Authorization: Bearer ...` instead of `x-api-key`, adds Claude Code identity headers + `claude-code-20250219,oauth-2025-04-20` beta headers + `"You are Claude Code..."` system block, maps `read` -> `Read` in request and restores `Read` -> `read` in streamed `ToolCall`.
4. `bedrock_prompt_cache_point_added_for_supported_models`: attaches `cachePoint` on system and final user message when `cache_retention != Some(CacheRetention::None)`.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-ai cache_control oauth_token_switches thinking_budgets_adaptive bedrock_prompt_cache`
Expected: FAIL.

- [ ] **Step 3: Implement caching, beta aggregation, thinking/effort rules, and OAuth mimicry**

Update `build_anthropic_request` (accepting `is_oauth: bool` so tests and `stream()` share one builder), `AnthropicProvider::stream` header construction, `adjust_max_tokens_for_thinking` + `clamp_thinking_budget_to_answer_room` (replacing the existing `max_tokens.max(effective_budget + 1024)` clamp so effective `max_tokens` respects `model.max_tokens` and leaves at least `MIN_ANSWER_TOKENS = 1024`), `AnthropicStreamState` tool-name unmapping, and `build_bedrock_request` `cachePoint` placement.

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p mehh-ai`
Expected: PASS.

---

### Task 6: Process-Safe `CredentialStore` Locking (`auth.json` & `mcp-auth.json`) + Wave 4 Gate

**Files:**
- Modify: `crates/mehh-ai/src/oauth.rs`
- Modify: `crates/mehh-coding-agent/src/auth.rs`
- Modify: `crates/mehh-mcp/src/oauth.rs`

**Interfaces:**
- **Consumes:** `std::fs::File::lock`, `tokio::task::spawn_blocking`, `mehh_ai::oauth::StoredCredential`
- **Produces:**
  - `#[async_trait::async_trait] pub trait CredentialStore: Send + Sync` (`read`, `list`, `modify`, `delete`)
  - `pub struct InMemoryCredentialStore` in `crates/mehh-ai/src/oauth.rs`
  - `pub struct FileCredentialStore` in `crates/mehh-coding-agent/src/auth.rs`
  - Advisory `<path>.lock` file locking on `McpAuthStore` (`crates/mehh-mcp/src/oauth.rs`)

- [ ] **Step 1: Write failing concurrency/lock tests**

Add tests:
1. `mehh_ai::oauth::tests::in_memory_credential_store_serializes_modify_per_provider`: concurrent `modify` tasks on the same provider observe sequential state without lost updates.
2. `mehh_coding_agent::auth::tests::file_credential_store_serializes_concurrent_refresh`: two concurrent `resolve_api_key_async` / `modify_locked` calls on an expired OAuth entry serialize via `<auth_path>.lock`, re-check expiry under lock, and execute only **1** HTTP refresh (against a local `TcpListener` mock counting `/token` POSTs).
3. `mehh_mcp::oauth::tests::mcp_auth_store_lock_preserves_concurrent_server_writes`: concurrent `McpAuthStore::set` calls for distinct server keys retain both entries in `mcp-auth.json` without clobbering.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p mehh-ai in_memory_credential_store && cargo test -p mehh-coding-agent file_credential_store_serializes && cargo test -p mehh-mcp mcp_auth_store_lock`
Expected: FAIL.

- [ ] **Step 3: Implement `CredentialStore`, `FileCredentialStore`, and `McpAuthStore` locking**

- In `crates/mehh-ai/src/oauth.rs`, define `CredentialStore` and `InMemoryCredentialStore`.
- Implement a small lockfile helper (`<path>.lock`, `std::fs::OpenOptions::create(true).read(true).write(true)` -> `spawn_blocking(move || file.lock())` keeping the `File` guard alive across read -> expiry re-check -> async refresh -> atomic write) in `FileCredentialStore` (`crates/mehh-coding-agent/src/auth.rs`) and `McpAuthStore` (`crates/mehh-mcp/src/oauth.rs`). Hold an in-process `tokio::sync::Mutex` alongside the OS file lock so same-process tasks serialize cleanly without burning Tokio threads.

- [ ] **Step 4: Run Wave 4 tests & full workspace verification gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (exit code 0). (Note: if pre-existing `unused dependency thiserror` in `mehh-coding-agent/Cargo.toml` fails `-D warnings`, verify whether any `thiserror` usage exists or clean it in Wave 5 / note exact clippy result.)
`````

## docs/superpowers/plans/2026-11-15-wave-5-agent-tools-session-cli.md

`````markdown
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
`````

## docs/superpowers/plans/2026-11-15-wave-6-mcp-tui-parity.md

`````markdown
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
`````

## docs/superpowers/plans/2026-11-16-goose-parity-core-plan.md

`````markdown
# Goose-Parity Core & Runtime Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform Mehhcode into a safer, lower-token, and faster single-binary coding agent that surpasses `aaif-goose/goose` by implementing runtime tool governance (`ToolInspector`), MCP hints, pure-Rust structural `analyze`, snapshot/rollback `undo_edit`, `Todo` + MOIM working memory, BM25 schema search, multi-mode parallel `subagent`, and Lead/Worker model routing.

**Architecture:** A 3-wave, test-driven evolution across `crates/*`:
- Wave 1 adds `ToolInspectionManager` (`SecurityInspector` -> `PermissionInspector` -> `RepetitionInspector`), parses MCP `annotations`, binds `AgentSession` loop hooks (`before_tool_call`), intercepts `CodemodeTool`, adds TUI approval modals with unified diff previews, and implements `--permission-mode`.
- Wave 2 implements zero-dependency pure-Rust `analyze` (AST symbols and structure for Rust/TS/Python/Go), `EditUndoStore` + `undo_edit`, `TodoTool` with ephemeral `prepare_request` MOIM injection, and Okapi BM25 schema indexing in `tool_search`.
- Wave 3 upgrades `SubagentTool` with multi-mode (`research`, `coding`, `review`), parallel batch execution, and live progress forwarding, plus Lead/Worker model routing with cross-provider credential isolation and error escalation.

**Tech Stack:** Rust 2021, `crates/mehh-*`, Tokio, Crossterm, Serde, Regex, Globset, Similar, QuickJS (`rquickjs`). Strictly zero new external Cargo crates.

**Spec:** `docs/superpowers/specs/2026-11-16-goose-parity-core-design.md`

## Global Constraints

- Strictly zero new external crates: use only crates already in root `Cargo.toml`.
- Source files must be UTF-8 without BOM; preserve target file's existing BOM/CRLF on edits and undos.
- Fail closed on security checks: permission, path, and trust checks never default to open.
- Normalize all paths canonically (with lowercase normalization on Windows `cfg!(windows)`) before keying rules, queues, or stores.
- No `any` or disabling lints; no `@ts-ignore` in TS; no unhandled error swallowing (`unwrap_or_default` without local recovery).
- Never commit unless the user explicitly requests it; stage only explicitly modified files.
- All unit and integration tests must run offline with `FauxProvider` (zero real network calls or paid API keys).

## Review Focus

1. **BOM & CRLF preservation during file rollback:** `undo_edit` must restore exact original file bytes (including UTF-8 BOM `0xEF 0xBB 0xBF` and Windows `\r\n`) without normalizing line endings or stripping BOM. (Pinned to Task 6, Step 1).
2. **Prompt injection defence on `.mehh` metadata files:** `SecurityInspector` must block any `edit`, `write`, or `undo_edit` targeting `<cwd>/.mehh/*` or `~/.mehh/agent/*` with `Deny` across ALL modes including `Auto`. (Pinned to Task 2, Step 1).
3. **Headless fail-closed execution without stdin hang:** Headless runs (`-p`, `--mode json`) triggering a mutating tool in `SmartApprove` mode without `--permission-mode auto` must terminate with an actionable error rather than blocking on stdin. (Pinned to Task 3, Step 1).
4. **Subagent recursion prohibition:** Child subagents in `"coding"` or `"research"` mode must have `subagent` and `codemode` stripped from their tool catalog to prevent runaway recursion. (Pinned to Task 9, Step 1).
5. **Cross-provider credential isolation:** Mid-turn Lead-to-Worker switching (e.g. Anthropic Lead -> OpenAI Worker) must resolve and pass provider-specific API keys without leaking Anthropic keys to OpenAI headers. (Pinned to Task 10, Step 1).

---

## Task Decomposition

### Wave 1 — Runtime Tool Governance, MCP Hints & TUI Tool Visibility

#### Task 1: MCP Tool Annotations & AgentTool Read-Only Metadata

**Files:**
- Modify: `crates/mehh-mcp/src/client.rs:30-45`
- Modify: `crates/mehh-mcp/src/tool_adapter.rs:440-475`
- Modify: `crates/mehh-agent/src/types.rs:59-78`
- Modify: `crates/mehh-tools/src/read.rs`, `grep.rs`, `find.rs`, `ls.rs`, `write.rs`, `edit.rs`, `bash.rs`, `powershell.rs`
- Test: `crates/mehh-mcp/tests/test_annotations.rs`

**Interfaces:**
- Consumes: MCP JSON-RPC protocol schema, `AgentTool` trait in `mehh-agent`.
- Produces:
  ```rust
  // crates/mehh-mcp/src/client.rs
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
  #[serde(rename_all = "camelCase")]
  pub struct McpToolAnnotations {
      pub read_only_hint: Option<bool>,
      pub destructive_hint: Option<bool>,
      pub idempotent_hint: Option<bool>,
      pub open_world_hint: Option<bool>,
  }

  // crates/mehh-agent/src/types.rs
  pub trait AgentTool: Send + Sync {
      // ... existing methods
      fn is_read_only(&self) -> Option<bool> { None }
      fn is_safe_for_auto_approve(&self, _args: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
          self.is_read_only()
      }
  }
  ```

- [ ] **Step 1: Write the failing test for MCP annotations and `is_read_only`**

Create `crates/mehh-mcp/tests/test_annotations.rs`:
```rust
use mehh_mcp::client::{McpToolAnnotations, McpToolInfo};
use mehh_agent::AgentTool;

#[test]
fn test_mcp_tool_info_deserializes_annotations() {
    let json = serde_json::json!({
        "name": "read_query",
        "description": "Run SQL query",
        "inputSchema": { "type": "object" },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false
        }
    });
    let info: McpToolInfo = serde_json::from_value(json).expect("deserialize McpToolInfo");
    assert_eq!(info.annotations.as_ref().and_then(|a| a.read_only_hint), Some(true));
    assert_eq!(info.annotations.as_ref().and_then(|a| a.destructive_hint), Some(false));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-mcp --test test_annotations`
Expected: FAIL with "no field `annotations` on type `McpToolInfo`"

- [ ] **Step 3: Implement `McpToolAnnotations` and `AgentTool::is_read_only`**

- In `crates/mehh-mcp/src/client.rs`, define `McpToolAnnotations` with `#[serde(rename_all = "camelCase")]` and add `#[serde(default, skip_serializing_if = "Option::is_none")] pub annotations: Option<McpToolAnnotations>` to `McpToolInfo`.
- In `crates/mehh-agent/src/types.rs`, add `fn is_read_only(&self) -> Option<bool> { None }` and `fn is_safe_for_auto_approve(...)` default methods to `pub trait AgentTool`.
- In `crates/mehh-mcp/src/tool_adapter.rs`:
  - Implement `AgentTool::is_read_only` on `McpAgentTool` returning `self.tool_info.annotations.as_ref().and_then(|a| a.read_only_hint)`.
  - Update `AgentTool::execution_mode` on `McpAgentTool` to return `ToolExecutionMode::Parallel` when `read_only_hint == Some(true)`, and `ToolExecutionMode::Sequential` otherwise.
  - Implement `is_read_only() -> Some(true)` on `ListMcpResourcesTool`, `ListMcpResourceTemplatesTool`, and `ReadMcpResourceTool`.
- In `crates/mehh-tools/src/`:
  - `read.rs`, `grep.rs`, `find.rs`, `ls.rs`: implement `fn is_read_only(&self) -> Option<bool> { Some(true) }`.
  - `write.rs`, `edit.rs`, `bash.rs`, `powershell.rs`: implement `fn is_read_only(&self) -> Option<bool> { Some(false) }`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-mcp --test test_annotations`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 2: Tool Inspectors Pipeline & Permission Store (`tool_inspection.rs`)

**Files:**
- Create: `crates/mehh-coding-agent/src/tool_inspection.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs:25-45`
- Test: `crates/mehh-coding-agent/tests/test_tool_inspection.rs`

**Interfaces:**
- Consumes: `AgentTool`, `BeforeToolCallResult` from `mehh-agent`, canonical path helpers.
- Produces:
  ```rust
  pub enum InspectionVerdict {
      Allow,
      RequireApproval { reason: String, preview: Option<String> },
      Deny { reason: String },
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
  #[serde(rename_all = "snake_case")]
  pub enum PermissionMode {
      Auto,
      #[default]
      SmartApprove,
      Approve,
      Chat,
  }

  pub struct ToolInspectionManager { ... }
  impl ToolInspectionManager {
      pub fn new(mode: PermissionMode, cwd: PathBuf) -> Self;
      pub async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
  }
  ```

- [ ] **Step 1: Write the failing test for `ToolInspectionManager` and security boundaries**

Create `crates/mehh-coding-agent/tests/test_tool_inspection.rs`:
```rust
use std::path::PathBuf;
use mehh_coding_agent::tool_inspection::{InspectionVerdict, PermissionMode, ToolInspectionContext, ToolInspectionManager};

#[tokio::test]
async fn test_security_inspector_blocks_mehh_metadata_edits() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(".mehh/settings.json"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_1".to_string(),
        tool_name: "edit".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
    };

    let verdict = mgr.inspect(&ctx).await;
    assert!(matches!(verdict, InspectionVerdict::Deny { .. }));
}

#[tokio::test]
async fn test_repetition_inspector_triggers_after_three_repeats() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("cargo check"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_repeat".to_string(),
        tool_name: "bash".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
    };

    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Deny { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_tool_inspection`
Expected: FAIL with "unresolved import `mehh_coding_agent::tool_inspection`"

- [ ] **Step 3: Implement `tool_inspection.rs`**

- In `crates/mehh-coding-agent/src/tool_inspection.rs`:
  - Define `InspectionVerdict`, `PermissionMode`, `ToolInspectionContext`.
  - Define `pub trait ToolInspector: Send + Sync`.
  - Implement `SecurityInspector`:
    - Checks commands (`bash`/`powershell`) against catastrophic patterns (`rm -rf /`, `curl | sh`, reading `~/.ssh/id_*`, `~/.mehh/agent/auth.json`, `Invoke-Expression`).
    - Checks file paths (`edit`, `write`, `undo_edit`) against `.mehh/*` and `~/.mehh/agent/*` (strict `Deny` in all modes).
    - Checks writes outside normalized `cwd` (`RequireApproval`).
  - Implement `PermissionInspector`:
    - Handles `Auto` (allows all except security flags).
    - Handles `SmartApprove`: allows `is_read_only == Some(true)`, checks shell whitelist (`cargo check`, `cargo test`, `git status`, `git diff`, `ls`), requires approval for mutating tools (`edit`, `write`, `bash` unknown).
    - Handles `Approve`: requires approval for all non-whitelisted tools.
    - Handles `Chat`: allows read-only exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, MCP `readOnlyHint: true`), denies mutating tools and shell execution.
    - Loads persisted `permissions.json` with exact rule matching (`tool_name` + optional `arg_prefix`/`path_glob`).
  - Implement `RepetitionInspector`:
    - Tracks consecutive `(tool_name, canonical_json_args)` calls up to `max_repetitions = 3`.
    - Returns `Deny` on 4th identical call.
  - Implement `ToolInspectionManager` composing the three inspectors in order.
- Register `pub mod tool_inspection;` in `crates/mehh-coding-agent/src/lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_tool_inspection`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 3: Wiring `AgentSession` Hooks, `CodemodeTool` Interceptor & Headless Fail-Closed

**Files:**
- Modify: `crates/mehh-codemode/src/tool.rs:135-170`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:550-610`
- Modify: `crates/mehh-coding-agent/src/settings.rs:30-65`
- Test: `crates/mehh-coding-agent/tests/test_agent_session_governance.rs`

**Interfaces:**
- Consumes: `ToolInspectionManager`, `CodemodeTool`, `AgentLoopConfig.before_tool_call`.
- Produces:
  ```rust
  // crates/mehh-codemode/src/tool.rs
  pub type ToolCallInterceptor = Arc<
      dyn Fn(&str, &serde_json::Map<String, serde_json::Value>) -> Result<(), String> + Send + Sync,
  >;
  impl CodemodeTool {
      pub fn with_interceptor(mut self, interceptor: ToolCallInterceptor) -> Self;
  }
  ```

- [ ] **Step 1: Write the failing test for headless fail-closed and `CodemodeTool` interception**

Create `crates/mehh-coding-agent/tests/test_agent_session_governance.rs`:
```rust
use std::sync::Arc;
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::tool_inspection::PermissionMode;

#[tokio::test]
async fn test_headless_smart_approve_blocks_mutating_tool() {
    let cwd = std::env::current_dir().unwrap();
    // In headless mode with SmartApprove, mutating bash tool must fail closed with Deny
    let mut session = AgentSession::create_for_test(cwd, PermissionMode::SmartApprove).await;
    let (_msg, _hist, config) = session.prepare_prompt("run dangerous command");
    
    assert!(config.before_tool_call.is_some(), "before_tool_call must be wired");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_agent_session_governance`
Expected: FAIL

- [ ] **Step 3: Implement hook wiring, interceptor, and headless policy**

- In `crates/mehh-codemode/src/tool.rs`:
  - Add `tool_interceptor: Option<ToolCallInterceptor>` to `CodemodeTool`.
  - In `execute` sandbox loop, call `interceptor(name_str, &params_map)` before `tool.execute(...)`.
- In `crates/mehh-coding-agent/src/settings.rs`:
  - Add `pub permission_mode: Option<PermissionMode>` to `Settings`.
  - Enforce permission mode escalation guard: ignore `permission_mode = auto` from workspace `.mehh/settings.json`, clamp to `SmartApprove`.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Add `inspection_manager: Arc<ToolInspectionManager>` and `approval_handler: Option<Arc<dyn ApprovalHandler>>` to `AgentSession`.
  - In `prepare_prompt_content`, wire `config.before_tool_call` to call `inspection_manager.inspect_and_authorize(...)`.
  - Attach `ToolInspectionManager` interceptor to `self.codemode_tool`.
  - Provide `HeadlessDenyHandler` for headless environments (`-p`, `--mode json`) that returns `Deny` on `RequireApproval`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_agent_session_governance`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 4: Interactive TUI Approval Modal, Rich Tool Cards & `/mode` Slash Command

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:30-80, 140-180, 500-600, 1100-1300`
- Modify: `crates/mehh-cli/src/main.rs:20-60, 200-240`
- Test: `crates/mehh-coding-agent/tests/test_interactive_approvals.rs`

**Interfaces:**
- Consumes: `InspectionVerdict::RequireApproval`, `ApprovalRequest`, `TranscriptItem::ToolCall`.
- Produces:
  ```rust
  // crates/mehh-coding-agent/src/interactive.rs
  pub enum SelectorKind {
      Model,
      Tree,
      Resume,
      Settings,
      Login,
      ToolApproval,
  }
  ```

- [ ] **Step 1: Write the failing test for `/mode` slash command and tool call preview**

Create `crates/mehh-coding-agent/tests/test_interactive_approvals.rs`:
```rust
use mehh_coding_agent::interactive::{handle_slash_command, SlashCommandOutcome};
use mehh_coding_agent::tool_inspection::PermissionMode;

#[test]
fn test_mode_slash_command_switches_mode() {
    let mut current_mode = PermissionMode::SmartApprove;
    let outcome = handle_slash_command("/mode chat", &mut current_mode);
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert_eq!(current_mode, PermissionMode::Chat);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_interactive_approvals`
Expected: FAIL

- [ ] **Step 3: Implement TUI approval selector, tool card preview, and `/mode`**

- In `crates/mehh-coding-agent/src/interactive.rs`:
  - Extend `SelectorKind` with `ToolApproval`.
  - Implement `ApprovalRequest` broker with async mutex to serialize concurrent approvals.
  - Render approval overlay inside `BorderedBox`: display tool name, target path or command, up to 18 lines of diff/output preview, and options:
    - 1. Allow once (`y` / `Enter`)
    - 2. Allow tool for session (`a`)
    - 3. Always allow tool (`A`, saved to `~/.mehh/agent/permissions.json`)
    - 4. Deny (`n` / `Esc`)
  - Extend `TranscriptItem::ToolCall` with `{ id, name, status, is_error, args_summary, output_preview, diff }`.
  - In `apply_agent_event`:
    - `ToolExecutionStart`: set `args_summary`.
    - `ToolExecutionUpdate`: buffer last 8 non-empty lines into `output_preview`.
    - `ToolExecutionEnd`: populate unified `diff` (capped at 24 lines with `+`/`-`/`@@` theme colors) and update final status.
  - Add `/mode [auto|smart_approve|approve|chat]` to `builtin_slash_commands()` and `handle_slash_command`.
- In `crates/mehh-cli/src/main.rs`:
  - Add `#[arg(long = "permission-mode", value_name = "MODE")] pub permission_mode: Option<String>` to `CliArgs`.
  - Pass configured permission mode into `AgentSession`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_interactive_approvals`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

### Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM & BM25 `tool_search`

#### Task 5: Pure-Rust Structural Code Analyzer (`crates/mehh-tools/src/analyze.rs`)

**Files:**
- Create: `crates/mehh-tools/src/analyze.rs`
- Modify: `crates/mehh-tools/src/lib.rs:1-45`
- Test: `crates/mehh-tools/tests/test_analyze.rs`

**Interfaces:**
- Consumes: `AgentTool`, `ignore::WalkBuilder`, `regex::Regex`.
- Produces:
  ```rust
  pub struct AnalyzeTool {
      cwd: PathBuf,
  }
  impl AnalyzeTool {
      pub fn new(cwd: PathBuf) -> Self;
  }
  ```

- [ ] **Step 1: Write the failing test for `AnalyzeTool` on Rust and TypeScript fixtures**

Create `crates/mehh-tools/tests/test_analyze.rs`:
```rust
use std::path::PathBuf;
use mehh_tools::AnalyzeTool;
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_analyze_rust_symbols() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = AnalyzeTool::new(cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    args.insert("mode".to_string(), serde_json::json!("symbols"));

    let res = tool.execute("call_1", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute analyze");
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("pub fn create_coding_tools"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tools --test test_analyze`
Expected: FAIL with "unresolved import `mehh_tools::AnalyzeTool`"

- [ ] **Step 3: Implement `AnalyzeTool`**

- In `crates/mehh-tools/src/analyze.rs`:
  - Define `AnalyzeTool` implementing `AgentTool` with `is_read_only() -> Some(true)` and `execution_mode() -> ToolExecutionMode::Parallel`.
  - Modes:
    - `"structure"`: Directory tree traversal via `ignore::WalkBuilder` with clamped depth `1..=5` (default 2), file counts, and extensions.
    - `"symbols"`: File-level structural extraction with line spans:
      - Rust (`.rs`): regex matching item declarations (`fn`, `struct`, `enum`, `trait`, `impl`, `type`) with brace depth tracking for body spans.
      - TS/JS (`.ts`, `.tsx`, `.js`, `.jsx`): declarations (`export`, `class`, `interface`, `type`, `function`, methods).
      - Python (`.py`): indentation-based `def` and `class` tracking.
      - Go (`.go`): `func`, `type ... struct`, `type ... interface`.
    - `"focus"`: Extracts full source of a named symbol up to 250 lines.
  - Enforce `MAX_ANALYZE_CHARS = 24_000` with clean truncation note.
- In `crates/mehh-tools/src/lib.rs`:
  - Add `pub mod analyze; pub use analyze::AnalyzeTool;`.
  - Register `AnalyzeTool` in `create_coding_tools` and `create_all_tools`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tools --test test_analyze`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 6: File Snapshot & Rollback (`EditUndoStore` + `undo_edit` tool)

**Files:**
- Create: `crates/mehh-tools/src/undo_edit.rs`
- Modify: `crates/mehh-tools/src/edit.rs:1-60, 200-240`
- Modify: `crates/mehh-tools/src/write.rs:1-50, 100-140`
- Modify: `crates/mehh-tools/src/lib.rs:1-45`
- Test: `crates/mehh-tools/tests/test_undo_edit.rs`

**Interfaces:**
- Consumes: `with_file_mutation_queue`, `similar::TextDiff`.
- Produces:
  ```rust
  pub struct EditUndoStore { ... }
  pub struct UndoEditTool { ... }
  ```

- [ ] **Step 1: Write the failing test for BOM/CRLF preservation and `undo_edit`**

Create `crates/mehh-tools/tests/test_undo_edit.rs`:
```rust
use std::sync::Arc;
use mehh_tools::{EditTool, EditUndoStore, UndoEditTool};
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_undo_edit_preserves_bom_and_crlf() {
    let temp = tempfile::tempdir().unwrap();
    let file_path = temp.path().join("crlf_bom.txt");
    
    // Original file with UTF-8 BOM and CRLF
    let original_bytes = b"\xEF\xBB\xBFline1\r\nline2\r\n";
    std::fs::write(&file_path, original_bytes).unwrap();

    let undo_store = Arc::new(EditUndoStore::new());
    let edit_tool = EditTool::new(temp.path().to_path_buf()).with_undo_store(undo_store.clone());
    let undo_tool = UndoEditTool::new(temp.path().to_path_buf(), undo_store);

    // Apply edit
    let mut edit_args = serde_json::Map::new();
    edit_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    edit_args.insert("old_string".to_string(), serde_json::json!("line2"));
    edit_args.insert("new_string".to_string(), serde_json::json!("line2_edited"));
    edit_tool.execute("call_edit", edit_args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();

    // Rollback with undo_edit
    let mut undo_args = serde_json::Map::new();
    undo_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    undo_tool.execute("call_undo", undo_args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();

    let restored_bytes = std::fs::read(&file_path).unwrap();
    assert_eq!(restored_bytes, original_bytes, "exact BOM and CRLF must be restored");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tools --test test_undo_edit`
Expected: FAIL with "unresolved import `mehh_tools::EditUndoStore`"

- [ ] **Step 3: Implement `EditUndoStore` and `UndoEditTool`**

- In `crates/mehh-tools/src/undo_edit.rs`:
  - Define `FileSnapshot` (`Existed { bytes: Vec<u8>, timestamp_ms: i64 }`, `DidNotExist`).
  - Implement `EditUndoStore`: thread-safe bounded per-file history (`MAX_SNAPSHOTS_PER_FILE = 20`, `MAX_UNDO_BYTES = 16 MiB`).
  - Implement `UndoEditTool`:
    - `is_read_only() -> Some(false)`.
    - Operates inside `with_file_mutation_queue`.
    - Restores previous snapshot or removes created file if `DidNotExist`.
    - Returns unified diff of the revert in `details["diff"]`.
- In `crates/mehh-tools/src/edit.rs` and `write.rs`:
  - Add optional `undo_store: Option<Arc<EditUndoStore>>` via `.with_undo_store(...)`.
  - Capture snapshot inside `with_file_mutation_queue` immediately prior to writing.
- In `crates/mehh-tools/src/lib.rs`: export `EditUndoStore` and `UndoEditTool`, add to `create_coding_tools`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tools --test test_undo_edit`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 7: `TodoTool` & MOIM Ephemeral Working Memory Injection

**Files:**
- Create: `crates/mehh-coding-agent/src/todo.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:550-610`
- Modify: `crates/mehh-coding-agent/src/lib.rs:25-45`
- Test: `crates/mehh-coding-agent/tests/test_todo_moim.rs`

**Interfaces:**
- Consumes: `AgentTool`, `AgentLoopConfig.prepare_request`.
- Produces:
  ```rust
  pub struct TodoTool { ... }
  pub struct TodoState { ... }
  ```

- [ ] **Step 1: Write the failing test for `TodoTool` CRUD and MOIM request injection**

Create `crates/mehh-coding-agent/tests/test_todo_moim.rs`:
```rust
use std::sync::Arc;
use tokio::sync::RwLock;
use mehh_coding_agent::todo::{TodoState, TodoTool};
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_todo_tool_updates_shared_state() {
    let state = Arc::new(RwLock::new(TodoState::default()));
    let tool = TodoTool::new(state.clone());

    let mut args = serde_json::Map::new();
    args.insert("action".to_string(), serde_json::json!("add"));
    args.insert("content".to_string(), serde_json::json!("Verify test suite"));

    tool.execute("call_1", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute todo add");

    let read = state.read().await;
    assert_eq!(read.items.len(), 1);
    assert_eq!(read.items[0].content, "Verify test suite");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_todo_moim`
Expected: FAIL with "unresolved import `mehh_coding_agent::todo`"

- [ ] **Step 3: Implement `TodoTool` and `prepare_request` MOIM hook**

- In `crates/mehh-coding-agent/src/todo.rs`:
  - Implement `TodoItem`, `TodoStatus` (`Pending`, `InProgress`, `Completed`), and `TodoState`.
  - Implement `TodoTool`: actions `"replace"`, `"add"`, `"update"`, `"list"`, `"clear"`.
  - Format helper `TodoState::format_moim_block(&self) -> Option<String>`.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Add `todo_state: Arc<RwLock<TodoState>>` to `AgentSession`.
  - In `prepare_prompt_content`, wire `config.prepare_request`:
    - Reads active `TodoState` (if non-empty) and optional `.mehh/MOIM.md`.
    - Appends `<working_checklist>...</working_checklist>` into the trailing user message context of `req_ctx.context.messages`.
    - Ensures ephemeral MOIM context is NOT saved to `session.append_message`.
- Register `pub mod todo;` in `crates/mehh-coding-agent/src/lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_todo_moim`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 8: BM25 Okapi & JSON Schema Property Indexing in `tool_search.rs`

**Files:**
- Modify: `crates/mehh-coding-agent/src/tool_search.rs:1-180`
- Test: `crates/mehh-coding-agent/tests/test_bm25_tool_search.rs`

**Interfaces:**
- Consumes: `DeferredToolCatalog`, `ToolSearchTool`.
- Produces:
  ```rust
  pub struct DeferredToolIndex { ... }
  // BM25 k1 = 1.2, b = 0.75 ranking algorithm
  ```

- [ ] **Step 1: Write the failing test for schema property ranking**

Create `crates/mehh-coding-agent/tests/test_bm25_tool_search.rs`:
```rust
use mehh_coding_agent::tool_search::DeferredToolCatalog;

#[test]
fn test_bm25_ranks_schema_property_matches_higher_than_noise() {
    let mut catalog = DeferredToolCatalog::new();
    catalog.register_test_tool("database_query", "Execute SQL", serde_json::json!({
        "properties": { "table_name": { "description": "Target SQL table" } }
    }));
    catalog.register_test_tool("unrelated_tool", "Unrelated utility", serde_json::json!({}));

    let results = catalog.search("table_name", 5);
    assert!(!results.is_empty());
    assert_eq!(results[0].name, "database_query");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_bm25_tool_search`
Expected: FAIL

- [ ] **Step 3: Implement BM25 Okapi and schema indexing**

- In `crates/mehh-coding-agent/src/tool_search.rs`:
  - Implement term tokenization (`snake_case`, `camelCase`, `mcp__server__tool` boundaries).
  - Implement `DeferredToolIndex` calculating term frequencies and document frequencies.
  - Implement Okapi BM25 (`k1 = 1.2`, `b = 0.75`) scoring:
    - Tool name exact match boost (`+50.0`).
    - Tool name substring boost (`+15.0`).
    - Weighted terms: `name` (3.0), `description` + JSON schema property names & descriptions (1.0), server namespace (0.5).
  - Maintain backward compatibility for `ToolSearchTool` result JSON structure.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_bm25_tool_search`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

### Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing

#### Task 9: Multi-Mode Parallel `SubagentTool` with Progress Streaming

**Files:**
- Modify: `crates/mehh-coding-agent/src/subagent.rs:1-250`
- Test: `crates/mehh-coding-agent/tests/test_subagent_multimode.rs`

**Interfaces:**
- Consumes: `ToolInspectionManager`, `AgentToolUpdateCallback`, `tokio::sync::Semaphore`.
- Produces:
  ```rust
  // Multi-mode SubagentTool supporting tasks batching and progress streaming
  pub struct SubagentTool { ... }
  ```

- [ ] **Step 1: Write the failing test for subagent recursion stripping and parallel task batching**

Create `crates/mehh-coding-agent/tests/test_subagent_multimode.rs`:
```rust
use mehh_coding_agent::subagent::SubagentTool;
use mehh_agent::AgentTool;

#[test]
fn test_subagent_tool_parameters_support_tasks_and_modes() {
    let params = SubagentTool::parameters_schema();
    assert!(params["properties"]["mode"]["enum"].as_array().unwrap().contains(&serde_json::json!("coding")));
    assert!(params["properties"]["mode"]["enum"].as_array().unwrap().contains(&serde_json::json!("review")));
    assert!(params["properties"]["tasks"].is_object());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_subagent_multimode`
Expected: FAIL

- [ ] **Step 3: Implement multi-mode, parallel batching, and progress streaming**

- In `crates/mehh-coding-agent/src/subagent.rs`:
  - Extend parameter schema: `mode` (`"research" | "coding" | "review"`), `tasks` (array of subtasks).
  - Filter tool catalogs per mode:
    - `"research"`: `read`, `grep`, `find`, `ls`, `analyze`.
    - `"coding"`: `read`, `write`, `edit`, `undo_edit`, `bash`, `powershell`, `grep`, `find`, `ls`, `analyze` (strictly exclude `subagent`, `codemode`, `todo` to eliminate recursion risk).
    - `"review"`: `read`, `grep`, `find`, `ls`, `analyze` + code review system prompt.
  - Implement parallel dispatch using `futures::future::join_all` bounded by `Semaphore::new(4)`.
  - Forward child turn events to parent `on_update` callback (`"[subagent:coding] turn N: edit"`).
  - Inherit parent's `ToolInspectionManager` and cancellation token in child loop.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_subagent_multimode`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 10: Lead/Worker Model Routing & Cross-Provider Error Escalation

**Files:**
- Modify: `crates/mehh-agent/src/types.rs:250-290`
- Modify: `crates/mehh-agent/src/agent_loop.rs:170-220`
- Create: `crates/mehh-coding-agent/src/lead_worker.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:460-520, 580-620`
- Modify: `crates/mehh-coding-agent/src/settings.rs:30-65`
- Modify: `crates/mehh-cli/src/main.rs:40-70, 250-290`
- Test: `crates/mehh-coding-agent/tests/test_lead_worker.rs`

**Interfaces:**
- Consumes: `AgentLoopTurnUpdate`, `ModelRegistry`, `resolve_api_key_async`.
- Produces:
  ```rust
  // crates/mehh-agent/src/types.rs
  pub struct AgentLoopTurnUpdate {
      pub model: Option<Model>,
      pub provider: Option<Arc<dyn LlmProvider>>,
      pub api_key: Option<Option<String>>,
      pub thinking_level: Option<Option<ThinkingLevel>>,
  }

  // crates/mehh-coding-agent/src/lead_worker.rs
  pub struct LeadWorkerManager { ... }
  ```

- [ ] **Step 1: Write the failing test for cross-provider key isolation and error escalation**

Create `crates/mehh-coding-agent/tests/test_lead_worker.rs`:
```rust
use mehh_coding_agent::lead_worker::LeadWorkerManager;
use mehh_ai::{Model, StopReason};

#[test]
fn test_lead_worker_escalates_on_provider_error() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 1);
    
    // Turn 1: Lead
    assert_eq!(mgr.active_model_for_turn(0), "lead-model");
    // Turn 2: Switches to Worker
    assert_eq!(mgr.active_model_for_turn(1), "worker-model");
    
    // Worker hits StopReason::Error -> escalates back to Lead
    mgr.record_turn_outcome(StopReason::Error, false);
    assert_eq!(mgr.active_model_for_turn(2), "lead-model");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_lead_worker`
Expected: FAIL

- [ ] **Step 3: Implement cross-provider turn updates, `LeadWorkerManager`, and CLI flags**

- In `crates/mehh-agent/src/types.rs`:
  - Extend `AgentLoopTurnUpdate` with `pub provider: Option<Arc<dyn LlmProvider>>` and `pub api_key: Option<Option<String>>`.
- In `crates/mehh-agent/src/agent_loop.rs`:
  - Update `run_agent_loop` to use `(current_model, current_provider, current_api_key)` when `turn_update` specifies them.
- In `crates/mehh-coding-agent/src/lead_worker.rs`:
  - Implement `LeadWorkerManager`:
    - Stores `lead_model`, `worker_model`, `lead_turns`, per-prompt turn counter.
    - Resolves distinct provider instances and API keys per model (zero key leakage).
    - Escalates to Lead on `StopReason::Error`, empty completions, or repetition breaks.
    - Resets turn count to 0 at the start of each user prompt.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Integrate `LeadWorkerManager` into `prepare_prompt_content` and `prepare_next_turn`.
  - Pass `worker_model` to `SubagentTool` as default execution model.
- In `crates/mehh-coding-agent/src/settings.rs`:
  - Add `pub worker_model: Option<String>` and `pub lead_turns: Option<usize>` to `Settings`.
- In `crates/mehh-cli/src/main.rs`:
  - Add `--worker-model <MODEL>` and `--lead-turns <N>` to `CliArgs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_lead_worker`
Expected: PASS

- [ ] **Step 5: Run full workspace test suite and linter**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: 0 test failures, 0 warnings

---

## Execution Self-Review

1. **Spec Coverage:**
   - Wave 1: `McpToolAnnotations` (Task 1), `ToolInspectionManager` (Task 2), `AgentSession` hooks + `CodemodeTool` interceptor (Task 3), TUI approval modal + `/mode` + CLI `--permission-mode` (Task 4).
   - Wave 2: `AnalyzeTool` pure-Rust parser (Task 5), `EditUndoStore` + `undo_edit` (Task 6), `TodoTool` + MOIM (Task 7), BM25 schema search (Task 8).
   - Wave 3: Multi-mode parallel `SubagentTool` (Task 9), Lead/Worker routing with cross-provider swap (Task 10).
   - Complete 100% coverage of all spec sections.
2. **Step Scan:** Every step defines exact signatures, exact file targets, checkable test code, and concrete verification commands. No hand-waving or undefined types.
3. **Type Consistency:** Types defined in early tasks (`ToolInspectionManager`, `EditUndoStore`, `McpToolAnnotations`, `TodoState`, `LeadWorkerManager`) match exact usages across later tasks.
4. **Review Focus:** All 5 failure modes (BOM/CRLF preservation, `.mehh` protection, headless fail-closed, subagent recursion stripping, cross-provider key isolation) are explicitly tested in Tasks 6, 2, 3, 9, and 10.
5. **Proportion:** The plan is focused, direct, and actionable without code bloat or redundant filler.
`````

## docs/superpowers/plans/2026-11-17-interactive-session-switcher-plan.md

`````markdown
# OpenCode-Style Interactive Session Switcher & Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement an OpenCode-style interactive TUI session switcher and manager in Mehhcode with `Ctrl+S`, `/sessions`, `/session`, quick `[+ Start New Session]`, `Tab` workspace scope toggle, and `d` in-modal deletion without any new external crates.

**Architecture:** Extend `mehh-tui::keys` with `TuiAction::SessionPicker` (`ctrl+s`); add `list_all_sessions` and safe `delete_session_file` in `mehh-coding-agent::session`; enhance `SelectorKind::Resume` in `mehh-coding-agent::interactive` with item 0 `__new__`, `Tab` scope toggle between current directory and all directories, and in-modal session deletion with `d`/`Delete`.

**Tech Stack:** Rust 2021, `crossterm`, `serde`, `serde_json`, `sha2`, `uuid` v7.

**Spec:** [docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md)

## Global Constraints

- Zero new external Cargo crates.
- No emojis anywhere in code, commits, or UI.
- All files UTF-8 without BOM.
- Preserve backward compatibility for `/resume` and `/new`.
- Security checks fail closed: `delete_session_file` only deletes `.jsonl` files strictly inside `~/.mehh/sessions/`.
- Active session cannot be deleted from within the picker modal without warning.

## Review Focus

1. Deleting active running session: attempting to delete currently active session must be rejected with an informative notice.
2. Path traversal in session deletion: deleting paths outside `~/.mehh/sessions/` or non-`.jsonl` files must fail closed with an error.
3. Empty directory session list: opening session picker when no previous sessions exist must still show `[+ Start New Session]` rather than failing or showing an empty popup.
4. Scope toggling with zero sessions: pressing `Tab` when either current workspace or global workspaces have zero sessions must cleanly update without panic or index out-of-bounds.
5. Windows path separator normalization: cross-directory scanning must handle Windows backslashes and forward slashes uniformly.

---

### Task 1: Add `TuiAction::SessionPicker` Keybinding in `mehh-tui`

**Files:**
- Modify: `crates/mehh-tui/src/keys.rs:147-240`
- Test: `crates/mehh-tui/src/keys.rs:250-327`

**Interfaces:**
- Produces: `TuiAction::SessionPicker` variant mapped by default to `"ctrl+s"`.

- [ ] **Step 1: Write the failing test**

Add unit test in `crates/mehh-tui/src/keys.rs` `tests` module:
```rust
#[test]
fn test_session_picker_keybinding_default() {
    let kb = Keybindings::default_bindings();
    assert!(kb.matches("ctrl+s", TuiAction::SessionPicker));
    assert!(kb.matches("ctrl+S", TuiAction::SessionPicker));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tui test_session_picker_keybinding_default`  
Expected: FAIL with `variant or associated item not found in TuiAction`

- [ ] **Step 3: Implement `TuiAction::SessionPicker`**

In `crates/mehh-tui/src/keys.rs`:
1. Add `SessionPicker` variant to `pub enum TuiAction`.
2. In `Keybindings::default_bindings()`, add:
```rust
map.insert(TuiAction::SessionPicker, vec!["ctrl+s".into()]);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tui test_session_picker_keybinding_default`  
Expected: PASS

---

### Task 2: Implement Global Session Scanning and Safe Deletion in `mehh-coding-agent`

**Files:**
- Modify: `crates/mehh-coding-agent/src/session.rs:370-500`
- Test: `crates/mehh-coding-agent/src/session.rs:1440-1914`

**Interfaces:**
- Consumes: `SessionSummary`, `session_dir_for_cwd`, `FastSessionLine`.
- Produces:
  - `pub fn parse_session_summary(path: &Path) -> Option<SessionSummary>`
  - `pub fn list_all_sessions() -> Result<Vec<SessionSummary>>`
  - `pub fn delete_session_file(path: &Path) -> Result<()>`

- [ ] **Step 1: Write the failing tests**

In `crates/mehh-coding-agent/src/session.rs` `tests` module:
```rust
#[test]
fn test_list_all_sessions_and_delete() {
    let home = std::env::temp_dir().join(format!("mehh_global_sess_test_{}", Uuid::now_v7()));
    let sess_root = home.join(".mehh").join("sessions");
    let dir_a = sess_root.join("hash_a");
    let dir_b = sess_root.join("hash_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let file_a = dir_a.join("2026-11-17T01-00-00.000Z_sess1.jsonl");
    let file_b = dir_b.join("2026-11-17T02-00-00.000Z_sess2.jsonl");

    let header_a = SessionHeader {
        kind: "session".into(),
        version: 3,
        id: "sess1".into(),
        timestamp: "2026-11-17T01:00:00.000Z".into(),
        cwd: "C:/project_a".into(),
        parent_session: None,
    };
    let header_b = SessionHeader {
        kind: "session".into(),
        version: 3,
        id: "sess2".into(),
        timestamp: "2026-11-17T02:00:00.000Z".into(),
        cwd: "C:/project_b".into(),
        parent_session: None,
    };
    std::fs::write(&file_a, format!("{}\n", serde_json::to_string(&header_a).unwrap())).unwrap();
    std::fs::write(&file_b, format!("{}\n", serde_json::to_string(&header_b).unwrap())).unwrap();

    // Test parse_session_summary
    let summary_a = parse_session_summary(&file_a).expect("parse file_a");
    assert_eq!(summary_a.id, "sess1");

    // Test delete_session_file
    delete_session_file(&file_a).expect("delete file_a");
    assert!(!file_a.exists());

    // Test fail closed on path outside .mehh/sessions
    let outside_file = std::env::temp_dir().join("outside.jsonl");
    std::fs::write(&outside_file, "{}").unwrap();
    assert!(delete_session_file(&outside_file).is_err());
    let _ = std::fs::remove_file(outside_file);
    let _ = std::fs::remove_dir_all(home);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_list_all_sessions_and_delete`  
Expected: FAIL with `cannot find function parse_session_summary in this scope`

- [ ] **Step 3: Implement `parse_session_summary`, `list_all_sessions`, and `delete_session_file`**

In `crates/mehh-coding-agent/src/session.rs`:
1. Extract file parsing logic from `list_sessions` into `pub fn parse_session_summary(path: &Path) -> Option<SessionSummary>`.
2. Implement `pub fn list_all_sessions() -> Result<Vec<SessionSummary>>`:
   - Resolves `~/.mehh/sessions/`.
   - Iterates all subdirectories, reads `.jsonl` files with `parse_session_summary`.
   - Sorts results descending by `timestamp`.
3. Implement `pub fn delete_session_file(path: &Path) -> Result<()>`:
   - Validates that canonical path or normalized path contains `.mehh` and `sessions` component and ends with `.jsonl`.
   - Calls `std::fs::remove_file(path)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_list_all_sessions_and_delete`  
Expected: PASS

---

### Task 3: Support `/sessions` Command and Enhance `/session` Routing

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:225-240, 1315-1345, 1470-1490`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4390-4450`

**Interfaces:**
- Consumes: `open_session_picker`, `SessionPickerScope`.
- Produces:
  - Command `/sessions`: opens session picker modal.
  - Command `/session`: opens session picker modal if no args; `/session info` displays token/entry stats; `/session new` creates new session.
  - Slash command completions list updated with `/sessions` and `/session`.

- [ ] **Step 1: Write the failing test**

In `crates/mehh-coding-agent/src/interactive.rs` `tests` module:
```rust
#[tokio::test]
async fn test_slash_command_sessions_and_session_routing() {
    let mut session = create_test_session();
    let mut state = InteractiveState::new();

    // /sessions opens the selector
    let outcome = handle_slash_command("/sessions", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_some());
    assert_eq!(state.selector.as_ref().unwrap().kind, SelectorKind::Resume);

    // /session without args opens selector
    state.selector = None;
    let outcome = handle_slash_command("/session", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_some());

    // /session info displays stats
    state.selector = None;
    let outcome = handle_slash_command("/session info", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_none());
    let last_item = state.items.last().unwrap();
    if let TranscriptItem::System(text) = last_item {
        assert!(text.contains("Session:"));
        assert!(text.contains("Entries:"));
    } else {
        panic!("expected system transcript item");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_slash_command_sessions_and_session_routing`  
Expected: FAIL because `/sessions` is unhandled and `/session` prints stats directly.

- [ ] **Step 3: Update slash command handling**

In `crates/mehh-coding-agent/src/interactive.rs`:
1. In `slash_command_items()`:
   - Add `/sessions` with description `"Switch or start sessions (Ctrl+S)"`.
   - Update `/session` description to `"Manage sessions (/session info, /session new)"`.
2. In `handle_slash_command`:
   - Match `"/sessions" | "/resume"` -> call `open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace)`.
   - Match `"/session"`:
     - If `arg == Some("info")` -> print session stats.
     - If `arg == Some("new")` -> execute new session logic (alias for `/new`).
     - Otherwise -> call `open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_slash_command_sessions_and_session_routing`  
Expected: PASS

---

### Task 4: Enhance Interactive Selector with Quick New Session, `Tab` Scope, and `d` Delete

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:50-70, 1575-1620, 1820-1845, 2745-2880`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4390-4457`

**Interfaces:**
- Produces:
  - `pub enum SessionPickerScope { CurrentWorkspace, AllWorkspaces }`
  - Field `pub session_scope: Option<SessionPickerScope>` on `Selector`
  - In `resume_selector_items`: item 0 is always `__new__` (`[+ Start New Session]`).
  - Key `Tab` when `kind == SelectorKind::Resume` toggles scope and updates items.
  - Key `d` / `delete` when `kind == SelectorKind::Resume` deletes session file.
  - Key `Enter` on `__new__` calls `new_session()` and initializes transcript.

- [ ] **Step 1: Write the failing test**

In `crates/mehh-coding-agent/src/interactive.rs` `tests` module:
```rust
#[tokio::test]
async fn test_session_picker_items_have_new_session_at_zero() {
    let sessions = vec![SessionSummary {
        path: PathBuf::from("test.jsonl"),
        id: "test1".into(),
        timestamp: "2026-11-17T01:00:00.000Z".into(),
        cwd: "C:/test".into(),
        entry_count: 5,
        name: Some("test session".into()),
        first_prompt: Some("hello".into()),
    }];
    let items = resume_selector_items(&sessions, SessionPickerScope::CurrentWorkspace);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "__new__");
    assert!(items[0].label.contains("Start New Session"));
    assert_eq!(items[1].id, "test.jsonl");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_session_picker_items_have_new_session_at_zero`  
Expected: FAIL with arguments mismatch in `resume_selector_items`.

- [ ] **Step 3: Implement Selector Extensions and Key Actions**

In `crates/mehh-coding-agent/src/interactive.rs`:
1. Add `SessionPickerScope` enum with `CurrentWorkspace` and `AllWorkspaces`.
2. Add `pub session_scope: Option<SessionPickerScope>` to `Selector`.
3. Update `resume_selector_items(sessions: &[SessionSummary], scope: SessionPickerScope) -> Vec<SelectItem>`:
   - Insert `SelectItem::new("__new__", "[+ Start New Session]").with_description("Start a fresh conversation in current directory")` as first item.
   - For all subsequent sessions, format label and description. When `scope == SessionPickerScope::AllWorkspaces`, append directory name to description.
4. Implement `open_session_picker`:
   - Loads sessions according to `scope`.
   - Creates `Selector` with `kind: SelectorKind::Resume`, items from `resume_selector_items`, and `session_scope: Some(scope)`.
5. Update `SelectorKind::Resume` title in `render_now`:
   ```rust
   SelectorKind::Resume => match selector.session_scope {
       Some(SessionPickerScope::AllWorkspaces) => "Sessions: All Workspaces (Enter: select, Tab: current dir, d: delete, Esc: cancel)",
       _ => "Sessions: Current Workspace (Enter: select, Tab: all dirs, d: delete, Esc: cancel)",
   }
   ```
6. In event loop key handler:
   - On `Enter`:
     - If `item.id == "__new__"`: reset queues, call `session.session_mut().new_session()`, reset transcript.
     - Else: call `SessionManager::open_file` and `rebuild_transcript_items`.
   - On `Tab` with `SelectorKind::Resume`:
     - Toggle scope (`CurrentWorkspace` <-> `AllWorkspaces`).
     - Reload sessions, re-populate `sel.list`, update `sel.session_scope`.
   - On `d` | `delete` with `SelectorKind::Resume`:
     - If `item.id == "__new__"`: no-op.
     - If `item.id == active_session_path`: push system message "Cannot delete currently active session."
     - Otherwise: call `delete_session_file`, remove item from `sel.list`, push system message "Deleted session."

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_session_picker_items_have_new_session_at_zero`  
Expected: PASS

---

### Task 5: Wire `Ctrl+S` Hotkey in Interactive Loop

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2935-2945`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4440-4457`

**Interfaces:**
- Consumes: `TuiAction::SessionPicker`, `open_session_picker`.

- [ ] **Step 1: Write test for hotkey triggering session picker**

In `crates/mehh-coding-agent/src/interactive.rs`:
```rust
#[test]
fn test_hotkeys_help_includes_session_picker() {
    let kb = Keybindings::default_bindings();
    let help = format_hotkeys(&kb);
    assert!(help.contains("Ctrl+S") || help.contains("ctrl+s"));
    assert!(help.contains("session") || help.contains("Session"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_hotkeys_help_includes_session_picker`  
Expected: FAIL because `format_hotkeys` doesn't list `SessionPicker`.

- [ ] **Step 3: Wire `SessionPicker` in `format_hotkeys` and event loop**

1. In `format_hotkeys`: add `Ctrl+S: Switch or create sessions`.
2. In `interactive.rs` loop before `handle_editor_key`:
```rust
if keybindings.matches(&key_str, TuiAction::SessionPicker) {
    open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace);
    render_now(&state, &editor, session, &mut terminal, &mut renderer);
    continue 'outer;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_hotkeys_help_includes_session_picker`  
Expected: PASS

---

### Task 6: Workspace-wide Compilation, Clippy, and Test Verification

**Files:**
- None (verification step across all 8 crates)

- [ ] **Step 1: Run `cargo check --workspace`**

Run: `cargo check --workspace`  
Expected: Clean exit 0 with 0 errors.

- [ ] **Step 2: Run `cargo clippy --workspace --all-targets -- -D warnings`**

Run: `cargo clippy --workspace --all-targets -- -D warnings`  
Expected: Clean exit 0 with 0 warnings.

- [ ] **Step 3: Run workspace tests**

Run: `cargo test --workspace`  
Expected: 100% test pass across all 8 crates.
`````

## docs/superpowers/plans/2026-11-18-subagent-port-claw-code.md

`````markdown
# Subagent Lifecycle, Control and Display Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give mehh subagents a machine-readable on-disk lifecycle, a global concurrency budget, per-child steer/kill handles, and a nested TUI view driven by structured events.

**Architecture:** A new `subagent_lifecycle` module owns the manifest data model and disk writer. `SubagentTool` builds a manifest per child, emits structured updates in `AgentToolResult.details`, and finalizes it on every exit path. A shared `SubagentControl` registry holds one handle per child (child cancel token + steering queue), providing the global semaphore and the list/steer/kill APIs. The interactive renderer branches on `details.subagent` to show a nested group instead of a generic tool card.

**Tech Stack:** Rust, `tokio`, `tokio-util`, `uuid`, `serde_json`, existing `mehh-agent`/`mehh-coding-agent` types.

**Spec:** `docs/superpowers/specs/2026-11-18-subagent-port-claw-code-design.md`

## Global Constraints

- Verify with `cargo test -p mehh-coding-agent` from the workspace root.
- No new dependencies. `uuid`, `serde`, `serde_json`, `tokio`, `tokio-util`, `async-trait` are already in `crates/mehh-coding-agent/Cargo.toml`.
- Timestamps are `i64` epoch milliseconds from `mehh_ai::now_ms()` (returns `i64`, same as `UserMessage.timestamp`); field names end in `_ms`. Never use a function named `iso8601_now`.
- Manifest directory: `<cwd>/.mehh-agents` unless env `MEHH_AGENT_STORE` overrides it. Tests must derive the expected path with `subagent_lifecycle::agent_store_dir(&cwd)` rather than hardcoding it, so an ambient env var cannot break them. Do not add the directory to `.gitignore` in this change.
- Reuse the existing `SUBAGENT_TIMEOUT_SECS` constant at `subagent.rs:27` as the default. Do not introduce a second timeout constant. The new `timeout_secs` parameter overrides the `Duration::from_secs(SUBAGENT_TIMEOUT_SECS)` currently hard-coded at `subagent.rs:228`.
- Integration tests calling `sub.execute(...)` must `use mehh_agent::AgentTool;` — `execute` is a trait method, not inherent.
- Preserve the existing three modes and their tool filters. Do not add recursive subagent spawning.
- Structured child events live in `AgentToolResult.details` under key `"subagent"`. Keep text updates intact. Emit a `Started` update at child start so the TUI and tests can learn the child id before any tool runs.
- `SubagentControl` is optional on `SubagentTool`; direct `SubagentTool::new(...)` construction must keep compiling and working. All `Self { ... }` child literals (`subagent.rs:311-320`, `:365-374`) must include `control: self.control.clone()`.
- `kill` cancels exactly one child's token, and that token must be an **ancestor** of the child's `config.cancel`. It must not call `AgentSession::abort`.
- Finalization is the single source of terminal events: push the terminal `SubagentEvent` into the manifest **and** emit it through `on_update` before writing the manifest.
- `run_agent_loop` checks cancellation only at turn boundaries (`mehh-agent/src/agent_loop.rs:113,151,335,363`); while awaiting `stream.next()` (`:440`) it does not. Therefore `run_child_prompt` must wrap the timeout future in a `tokio::select!` with a `cancel.cancelled()` branch returning `Err("Subagent execution was aborted")`. This stays inside `mehh-coding-agent` (respecting the no-`mehh-agent`-changes non-goal) and makes `kill`/abort effective even against providers whose streams do not self-interrupt.
- Commits: `feat(subagent): <ascii subject>`, explicit paths only.

## Review Focus

1. **Timeout or abort leaves status `running` forever** — terminal manifest must include a terminal event and non-null `completed_at_ms`. Test in Task 2 (timeout + abort).
2. **Global budget is still per-call** — two overlapping `subagent` tool calls must contend on one semaphore, not allocate two `Semaphore(4)` pools. Test in Task 3 with two concurrent real `execute` calls plus a blocked third acquire.
3. **`kill` takes down the parent turn or does nothing** — cancelling a child must stop that child's loop and leave the parent token intact. Test in Task 3 with a real hanging child.
4. **Steer targets the wrong child / unknown id** — unknown id returns `false`; two children do not share a queue; a steered message is actually consumed by the child loop. Tests in Task 3 and Task 4.
5. **Nested TUI rows get cross-wired between parallel tool calls** — the group is keyed by the parent `tool_call_id`; rows are keyed by `child_id`. Test in Task 5.

---

### Task 1: Subagent lifecycle data model and manifest writer

**Files:**
- Create: `crates/mehh-coding-agent/src/subagent_lifecycle.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs:20` (add `pub mod subagent_lifecycle;`) and `:45` (add `pub use subagent_lifecycle::*;`)
- Test: `crates/mehh-coding-agent/src/subagent_lifecycle.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces (all `pub`, in `subagent_lifecycle`):
  - `enum SubagentStatus { Running, Completed, Failed }` — `serde(rename_all = "snake_case")`
  - `enum SubagentEventKind { Started, ToolStarted, ToolFinished, Blocked, Finished, Aborted, TimedOut }` — snake_case
  - `struct SubagentEvent { pub kind: SubagentEventKind, pub at_ms: i64, pub detail: Option<String> }`
  - `enum SubagentFailureClass { Transport, ToolRuntime, Compile, Test, Mcp, Timeout, Aborted, Infra }` + `fn classify_failure(error: &str) -> SubagentFailureClass`
  - `fn derive_state(status: &SubagentStatus, failure_class: Option<&SubagentFailureClass>) -> &'static str`
  - `struct SubagentManifest { pub agent_id: String, pub mode: String, pub model_id: String, pub status: SubagentStatus, pub created_at_ms: i64, pub completed_at_ms: Option<i64>, pub events: Vec<SubagentEvent>, pub failure_class: Option<SubagentFailureClass>, pub derived_state: String, pub result: Option<String>, pub error: Option<String> }`
  - `fn new_agent_id() -> String` — `format!("sub-{}", uuid::Uuid::now_v7())`
  - `fn new_manifest(agent_id: String, mode: String, model_id: String, now_ms: i64) -> SubagentManifest` — sets `Running`, `derived_state = "working"`, one `Started` event
  - `fn agent_store_dir(cwd: &std::path::Path) -> std::path::PathBuf`
  - `fn write_manifest(dir: &std::path::Path, manifest: &SubagentManifest) -> Result<std::path::PathBuf, String>` — creates dir, writes pretty JSON to `<dir>/<agent_id>.json`, returns path

- [ ] **Step 1: Write failing tests in `subagent_lifecycle.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manifest_starts_running_with_started_event() {
        let m = new_manifest("sub-1".into(), "research".into(), "m".into(), 1000);
        assert_eq!(m.status, SubagentStatus::Running);
        assert_eq!(m.derived_state, "working");
        assert_eq!(m.events.len(), 1);
        assert_eq!(m.events[0].kind, SubagentEventKind::Started);
        assert_eq!(m.events[0].at_ms, 1000);
        assert_eq!(m.completed_at_ms, None);
    }

    #[test]
    fn classify_failure_maps_known_signals_and_defaults_to_infra() {
        assert_eq!(classify_failure("tool runtime exploded"), SubagentFailureClass::ToolRuntime);
        assert_eq!(classify_failure("compile failed"), SubagentFailureClass::Compile);
        assert_eq!(classify_failure("broken pipe"), SubagentFailureClass::Transport);
        assert_eq!(classify_failure("Subagent timed out after 180s"), SubagentFailureClass::Timeout);
        assert_eq!(classify_failure("no idea"), SubagentFailureClass::Infra);
    }

    #[test]
    fn write_manifest_persists_terminal_state_with_epoch_ms() {
        let dir = std::env::temp_dir().join(format!("mehh_lc_test_{}", uuid::Uuid::now_v7()));
        let mut m = new_manifest("sub-2".into(), "coding".into(), "m".into(), 1000);
        m.status = SubagentStatus::Failed;
        m.completed_at_ms = Some(2500);
        m.failure_class = Some(SubagentFailureClass::Test);
        m.derived_state = derive_state(&m.status, m.failure_class.as_ref()).to_string();
        m.error = Some("test failed".into());
        let path = write_manifest(&dir, &m).unwrap();
        assert_eq!(path.file_name().unwrap(), "sub-2.json");
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(raw["status"], "failed");
        assert_eq!(raw["created_at_ms"], 1000);
        assert_eq!(raw["completed_at_ms"], 2500);
        assert_eq!(raw["failure_class"], "test");
        assert!(raw.get("iso8601_now").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent subagent_lifecycle::tests -- --nocapture`
Expected: compile failure, module does not exist yet.

- [ ] **Step 3: Implement `subagent_lifecycle.rs`**

Define the types and functions listed in Interfaces. `classify_failure` checks lowercased substrings in this order: `timed out` → `Timeout`, `aborted` → `Aborted`, `compile`/`build failed` → `Compile`, `test` → `Test`, `tool runtime`/`tool failed` → `ToolRuntime`, `mcp` → `Mcp`, `transport`/`pipe`/`connection` → `Transport`, else `Infra`. `derive_state` returns `working` for `Running`, `finished_cleanable` for `Completed`, and the failure-class name for `Failed`. `agent_store_dir` reads `MEHH_AGENT_STORE`, else `cwd.join(".mehh-agents")`. Add the two `lib.rs` lines.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent subagent_lifecycle::tests`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent_lifecycle.rs crates/mehh-coding-agent/src/lib.rs
git commit -m "feat(subagent): add lifecycle manifest data model and writer"
```

---

### Task 2: Structured child events and terminal persistence

**Files:**
- Modify: `crates/mehh-coding-agent/src/subagent.rs:75-106` (schema), `:128-250` (`run_child_prompt`), `:267-381` (`execute`), `:384-419` (in-file tests)
- Create: `crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs`
- Test: both files above

**Interfaces:**
- Consumes: all Task 1 types; `AgentToolResult` from `mehh_agent::types` (pub `details: Option<Value>`, derives `Default`); `SUBAGENT_TIMEOUT_SECS` at `subagent.rs:27`.
- Produces:
  - `fn subagent_update(child_id: &str, mode: &str, event: &SubagentEvent) -> AgentToolResult` — `details = Some(json!({"subagent": {"child_id":..., "mode":..., "event_kind":..., "detail":...}}))`, text kept as `"[subagent:{mode}] ..."`
  - `run_child_prompt` gains parameters `manifest: &mut SubagentManifest`, `timeout_secs: u64`, and `steering_queue: Option<Arc<Mutex<VecDeque<Message>>>>`; it emits a `Started` update at child start, pushes `ToolStarted`/`ToolFinished` events on tool start/end, and finalizes exactly once
  - `parameters_schema()` gains optional `timeout_secs` integer, `minimum: 1`, `maximum: 180`
  - `execute()` creates the manifest, passes it into both the single-task and batch paths, finalizes any child whose `JoinHandle` returns `Err(JoinError)`, and returns an `AgentToolResult` with `details` set

- [ ] **Step 1: Write failing tests in `tests/test_subagent_lifecycle.rs`**

```rust
use std::sync::Arc;
use mehh_agent::AgentTool; // required: execute() is a trait method
use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_lifecycle::agent_store_dir;

struct HangingProvider;

#[async_trait::async_trait]
impl mehh_ai::LlmProvider for HangingProvider {
    fn stream(
        &self,
        _model: &mehh_ai::Model,
        _context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = mehh_ai::create_event_stream();
        std::mem::forget(tx); // channel stays open forever, so the loop hangs until timeout
        rx
    }
}

fn manifest_files(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(agent_store_dir(cwd)).unwrap()
        .map(|e| e.unwrap().path()).collect()
}

#[tokio::test]
async fn successful_child_writes_completed_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ok_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub.execute("tc1", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap();
    assert!(res.details.is_some(), "result must carry structured subagent details");

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "completed");
    assert!(m["completed_at_ms"].is_number());
    assert_eq!(m["events"][0]["kind"], "started");
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "finished"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn timeout_writes_terminal_timed_out_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_to_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(1));
    let err = sub.execute("tc2", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap_err();
    assert!(err.contains("timed out"));

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "timeout");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "timed_out"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn abort_writes_aborted_manifest_with_completed_at_ms() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ab_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { sub.execute("tc3", params, task_cancel, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap_err();
    assert!(err.contains("aborted") || err.contains("Aborted"), "unexpected: {err}");

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "aborted");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "aborted"));
    std::fs::remove_dir_all(dir).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_lifecycle`
Expected: compile failure (`timeout_secs` absent from schema; no `Started`/terminal events in manifest).

- [ ] **Step 3: Implement in `subagent.rs`**

Parse `timeout_secs` (default `SUBAGENT_TIMEOUT_SECS`) in `execute()` and pass it to `run_child_prompt`, replacing the hard-coded `Duration::from_secs(SUBAGENT_TIMEOUT_SECS)` at `:228` with `Duration::from_secs(timeout_secs)`. Wrap that timeout future in a `tokio::select!` with a second branch on `cancel.cancelled()` that returns `Err("Subagent execution was aborted".to_string())`; keep the `tokio::time::timeout` error text containing "timed out" for the timeout branch. Emit a `Started` `subagent_update` through `on_update` immediately after creating the manifest so the child id is observable before any tool runs. Restructure `run_child_prompt` so its body computes a `Result<String, String>` outcome in an inner block, then runs exactly one finalization step. Finalization must: set `Completed` + `result` on `Ok`, or `Failed` + `error` on `Err`; classify failure (if `cancel.is_cancelled()` on the timeout/abort path use `Aborted`, else `classify_failure(&error)`); derive state; set `completed_at_ms = Some(now_ms())`; **push a terminal `SubagentEvent` (`Finished`/`Aborted`/`TimedOut`) into `manifest.events` and call `subagent_update` for it**; then call `write_manifest`. In `execute()`, write the manifest after the single/batch path returns and set `details` on the returned `AgentToolResult` to `Some(json!({"subagent": {"child_id": manifest.agent_id, "mode": mode, "status": manifest.status, "manifest": path}}))` using `..Default::default()` for other fields. For batch mode: each task gets its own manifest; when a `JoinHandle` returns `Err(join_err)`, the parent finalizes that child with `Failed`, error `format!("subagent task panicked: {join_err}")`, and a terminal `Aborted` event, so no child stays `running`.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_lifecycle`
Expected: 3 tests pass.

- [ ] **Step 5: Full crate regression**

Run: `cargo test -p mehh-coding-agent`
Expected: all tests pass, including `test_subagent_multimode` and `test_subagent_governance`.

- [ ] **Step 6: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent.rs crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs
git commit -m "feat(subagent): emit structured lifecycle events and persist terminal state"
```

---

### Task 3: Shared child registry, global budget, and per-child control

**Files:**
- Create: `crates/mehh-coding-agent/src/subagent_control.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs` (module + re-export)
- Modify: `crates/mehh-coding-agent/src/subagent.rs` (new optional field + builder + registration in both paths + `control` in child literals)
- Modify: `crates/mehh-coding-agent/src/agent_session.rs` (field init in `new_with_trust` only; `refresh_subagent_tool`; accessor)
- Create: `crates/mehh-coding-agent/tests/test_subagent_control.rs`

**Interfaces:**
- Produces in `subagent_control`:
  - `pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 4;`
  - `pub struct SubagentHandle { pub agent_id: String, pub mode: String, pub started_ms: i64, pub cancel: CancellationToken, pub steering_queue: Arc<Mutex<VecDeque<Message>>> }`
  - `pub struct SubagentSummary { pub agent_id: String, pub mode: String, pub elapsed_ms: i64 }`
  - `pub struct SubagentControl { permits: Arc<Semaphore>, children: Arc<Mutex<HashMap<String, SubagentHandle>>> }`
  - `pub fn new(max_concurrent: usize) -> Self`
  - `pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit` — implemented as `Arc::clone(&self.permits).acquire_owned().await.expect("semaphore closed")`
  - `pub fn register(&self, agent_id: String, mode: String, cancel: CancellationToken) -> SubagentHandle`
  - `pub fn unregister(&self, agent_id: &str)`
  - `pub fn list(&self) -> Vec<SubagentSummary>`
  - `pub fn steer(&self, agent_id: &str, text: &str) -> bool` — pushes a `Message::User` into that child's queue; `false` for unknown id
  - `pub fn kill(&self, agent_id: &str) -> bool` — cancels only that child's token and removes the handle; `false` for unknown id
- `SubagentTool` gains `pub control: Option<Arc<SubagentControl>>` and `pub fn with_control(mut self, control: Arc<SubagentControl>) -> Self`.
- `AgentSession` gains `pub subagent_control: Arc<SubagentControl>` and `pub fn subagent_control(&self) -> Arc<SubagentControl>`.

- [ ] **Step 1: Write failing tests in `tests/test_subagent_control.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;
use mehh_agent::AgentTool;
use mehh_ai::{faux_model, Context, Model, StreamOptions};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};

struct HangingProvider;
#[async_trait::async_trait]
impl mehh_ai::LlmProvider for HangingProvider {
    fn stream(&self, _m: &Model, _c: &Context, _o: &StreamOptions, _x: tokio_util::sync::CancellationToken) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = mehh_ai::create_event_stream();
        std::mem::forget(tx);
        rx
    }
}

#[tokio::test]
async fn global_budget_is_shared_and_limited() {
    let control = Arc::new(SubagentControl::new(2));
    let _a = control.acquire().await;
    let _b = control.acquire().await;
    let blocked = tokio::time::timeout(Duration::from_millis(50), control.clone().acquire()).await;
    assert!(blocked.is_err(), "third acquire must block while both permits are held");
    assert_eq!(DEFAULT_MAX_CONCURRENT_SUBAGENTS, 4);
}

#[tokio::test]
async fn two_overlapping_subagent_calls_share_one_budget() {
    let control = Arc::new(SubagentControl::new(2));
    let cwd = std::env::temp_dir().join(format!("mehh_budget_{}", uuid::Uuid::now_v7()));
    let mk = || SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
        .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let c1 = tokio_util::sync::CancellationToken::new();
    let c2 = tokio_util::sync::CancellationToken::new();
    let c3 = tokio_util::sync::CancellationToken::new();
    let t1 = tokio::spawn(mk().execute("a", params.clone(), c1.clone(), None));
    let t2 = tokio::spawn(mk().execute("b", params.clone(), c2.clone(), None));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(control.list().len(), 2, "two children from two separate calls");
    let t3 = tokio::spawn(mk().execute("c", params, c3.clone(), None));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tokio::time::timeout(Duration::from_millis(50), control.clone().acquire()).await.is_err(),
        "third child must be blocked by the shared budget");
    c1.cancel(); c2.cancel(); c3.cancel();
    for t in [t1, t2, t3] {
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
    }
    std::fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn kill_targets_one_child_and_steer_reaches_only_that_child() {
    let control = Arc::new(SubagentControl::new(4));
    let parent = tokio_util::sync::CancellationToken::new();
    let a = control.register("sub-a".into(), "coding".into(), parent.child_token());
    let b = control.register("sub-b".into(), "research".into(), parent.child_token());

    assert!(control.steer("sub-a", "new instruction"));
    assert_eq!(a.steering_queue.lock().unwrap().len(), 1);
    assert_eq!(b.steering_queue.lock().unwrap().len(), 0);
    assert!(!control.steer("missing", "x"));

    assert!(control.kill("sub-a"));
    assert!(a.cancel.is_cancelled());
    assert!(!b.cancel.is_cancelled());
    assert!(!parent.is_cancelled(), "parent turn must survive child kill");
    assert!(!control.kill("sub-a"), "already-unregistered id returns false");
    control.unregister("sub-a");
    assert_eq!(control.list().len(), 1);
}

#[tokio::test]
async fn kill_stops_a_real_child_loop() {
    let control = Arc::new(SubagentControl::new(4));
    let cwd = std::env::temp_dir().join(format!("mehh_kill_{}", uuid::Uuid::now_v7()));
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let child_id = Arc::new(std::sync::Mutex::new(None::<String>));
    let seen = child_id.clone();
    let on_update: mehh_agent::AgentToolUpdateCallback = Arc::new(move |res| {
        if let Some(id) = res.details.as_ref().and_then(|d| d.get("subagent")).and_then(|s| s.get("child_id")).and_then(|v| v.as_str()) {
            if seen.lock().unwrap().is_none() {
                *seen.lock().unwrap() = Some(id.to_string());
                let _ = tx.send(id.to_string());
            }
        }
    });
    let sub = SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
        .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let token = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(async move { sub.execute("tc", params, token, Some(on_update)).await });
    let id = tokio::time::timeout(Duration::from_secs(2), rx).await.unwrap().unwrap();
    assert!(control.kill(&id), "registered child must be killable");
    let result = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(result.is_err(), "killed child must terminate with error");
    std::fs::remove_dir_all(cwd).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_control`
Expected: compile failure, `subagent_control` module missing.

- [ ] **Step 3: Implement `subagent_control.rs`**

`register` inserts a handle and returns a clone. `steer` pushes `Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: now_ms() })`. `kill` looks up the id, calls `cancel.cancel()`, removes the handle. `unregister` removes by id. `list` maps remaining handles to `SubagentSummary` with `elapsed_ms = now_ms() - started_ms`. `acquire` uses `acquire_owned().await.expect(...)`.

- [ ] **Step 4: Wire into `SubagentTool`**

Add the `control` field and `with_control` builder. In single-task and batch paths: obtain a permit from `self.control` when present (else a local `Semaphore::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS)`), create the child id, call `control.register(id, mode, parent_cancel.clone())`, and **pass `handle.cancel.clone()` as the `cancel` argument to `run_child_prompt`** so `config.cancel = handle.cancel.child_token()` — this makes `kill` effective. Set `config.steering_queue = handle.steering_queue.clone()` and `config.steering_mode = QueueMode::OneAtATime` inside `run_child_prompt`. Call `unregister` on every exit path, including the `JoinError` branch from Task 2. Add `control: self.control.clone()` to both child struct literals at `subagent.rs:311-320` and `:365-374`.

- [ ] **Step 5: Wire into `AgentSession`**

In `new_with_trust` only (`new` delegates to it at `agent_session.rs:64-72`), construct `SubagentControl::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS)`, store it in the new field, and pass `.with_control(control.clone())` at both subagent construction sites (`:145-156` and `refresh_subagent_tool` at `:530`). Add the accessor.

- [ ] **Step 6: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_control`
Expected: 4 tests pass.

- [ ] **Step 7: Steer consumed by a child loop**

Add a test with a recording provider: its `stream()` pushes an `AssistantMessageEvent::Done` carrying a message whose text is the first `Context` message text, into the `AssistantEventStream`. Start a child with a steered message queued through `control.steer(&id, "ROUTE_ME")`, then assert the provider observed `"ROUTE_ME"` in the context. This proves `config.steering_queue` is wired, not just that the queue holds a message.

- [ ] **Step 8: Full crate regression**

Run: `cargo test -p mehh-coding-agent`
Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent_control.rs crates/mehh-coding-agent/src/lib.rs crates/mehh-coding-agent/src/subagent.rs crates/mehh-coding-agent/src/agent_session.rs crates/mehh-coding-agent/tests/test_subagent_control.rs
git commit -m "feat(subagent): add shared child registry, budget and steer/kill controls"
```

---

### Task 4: RPC commands for listing and controlling children

**Files:**
- Modify: `crates/mehh-coding-agent/src/rpc.rs:459-479` (add three match arms)
- Create: `crates/mehh-coding-agent/tests/test_subagent_rpc.rs`

**Interfaces:**
- Consumes: `AgentSession::subagent_control()` from Task 3; `run_rpc_mode<R, W>(&mut AgentSession, R, W) -> anyhow::Result<()>` at `rpc.rs:130`, with commands selected by the JSON `type` field (`rpc.rs:171`).
- Produces:
  - `"subagent_list"` → `send_success(id, "subagent_list", Some(json!({"agents": [...] })))`
  - `"subagent_steer"` with `"agent_id"` + `"message"` → `send_success` with `{"ok": bool}`
  - `"subagent_kill"` with `"agent_id"` → `send_success` with `{"ok": bool}`

- [ ] **Step 1: Write failing test in `tests/test_subagent_rpc.rs`**

```rust
use std::sync::Arc;
use tokio::io::{AsyncReadExt, BufReader};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::rpc::run_rpc_mode;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rpc_can_list_steer_and_kill_a_child() {
    let dir = std::env::temp_dir().join(format!("mehh_rpc_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::new(
        dir.clone(), mehh_ai::faux_model(), Arc::new(mehh_ai::FauxProvider::new()),
        SessionManager::in_memory(&dir), Settings::default(),
    );
    let child = session.subagent_control().register("sub-x".into(), "research".into(), CancellationToken::new());

    let input = b"{\"id\":\"1\",\"type\":\"subagent_list\"}\n{\"id\":\"2\",\"type\":\"subagent_steer\",\"agent_id\":\"sub-x\",\"message\":\"focus\"}\n{\"id\":\"3\",\"type\":\"subagent_kill\",\"agent_id\":\"sub-x\"}\n";
    let (writer, mut reader) = tokio::io::duplex(8192);
    run_rpc_mode(&mut session, BufReader::new(&input[..]), writer).await.unwrap();
    drop(session);

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("sub-x"), "list response must include the child id");
    assert!(text.contains("\"ok\":true"), "steer and kill must report success");
    assert_eq!(child.steering_queue.lock().unwrap().len(), 1);
    assert!(child.cancel.is_cancelled());
    std::fs::remove_dir_all(dir).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_rpc`
Expected: unknown-command errors in the JSONL output.

- [ ] **Step 3: Implement three match arms in `rpc.rs`**

Follow the `"steer"` arm at `:463-471`. Read `agent_id` from `val.get("agent_id").and_then(|v| v.as_str())`; missing or unknown id yields `{"ok": false}` rather than an error. List uses `session.subagent_control().list()` and serializes each `SubagentSummary`.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_rpc`
Expected: test passes.

- [ ] **Step 5: Full crate regression and commit**

Run: `cargo test -p mehh-coding-agent`

```bash
git add crates/mehh-coding-agent/src/rpc.rs crates/mehh-coding-agent/tests/test_subagent_rpc.rs
git commit -m "feat(subagent): add subagent list, steer and kill RPC commands"
```

---

### Task 5: Nested subagent view in the interactive TUI

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:31-46` (TranscriptItem), tool-event arms at `:1697-1761` (the enclosing `apply_agent_event` starts near `:1625`), render arm at `:1917-1965`, tests near `:3195-3400` (`make_test_state` is at `:3117`)
- Test: `crates/mehh-coding-agent/src/interactive.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `AgentEvent::ToolExecutionStart/Update/End` and `AgentToolResult.details["subagent"]` from Task 2.
- Produces:
  - `struct SubagentChildRow { pub child_id: String, pub label: String, pub status: String, pub detail: String }`
  - `TranscriptItem::Subagent { id: String, mode: String, status: String, is_error: bool, children: Vec<SubagentChildRow> }`
  - `ToolExecutionStart` with `tool_name == "subagent"` creates `TranscriptItem::Subagent`; `mode` comes from `args["mode"]`, default `"research"`
  - `ToolExecutionUpdate` with `details["subagent"]` finds the item whose `id == tool_call_id`, then updates or inserts a row keyed by `child_id` (never by label)
  - `ToolExecutionEnd` sets the group's final `status` and `is_error`
  - Renderer prints `  ⚙ subagent [mode] (id): status`, then indented rows `    - <label>: <status> <detail>`

- [ ] **Step 1: Write failing tests in `interactive.rs` tests module**

```rust
#[test]
fn test_subagent_group_renders_nested_child_rows_keyed_by_tool_call_id() {
    let mut state = make_test_state();
    let tool_id = "tc_sub_1".to_string();

    apply_agent_event(&mut state, &AgentEvent::ToolExecutionStart {
        tool_call_id: tool_id.clone(),
        tool_name: "subagent".to_string(),
        args: serde_json::json!({"mode": "coding"}).as_object().unwrap().clone(),
    });
    assert!(matches!(state.items.last().unwrap(), TranscriptItem::Subagent { mode, .. } if mode == "coding"));

    let child_update = |id: &str, kind: &str, detail: &str| AgentEvent::ToolExecutionUpdate {
        tool_call_id: tool_id.clone(), tool_name: "subagent".to_string(), args: serde_json::Map::new(),
        partial_result: mehh_agent::AgentToolResult {
            details: Some(serde_json::json!({"subagent": {
                "child_id": id, "mode": "coding", "event_kind": kind, "detail": detail
            }})),
            ..Default::default()
        },
    };
    apply_agent_event(&mut state, &child_update("sub-a", "tool_started", "read"));
    apply_agent_event(&mut state, &child_update("sub-b", "tool_started", "read"));
    apply_agent_event(&mut state, &child_update("sub-b", "tool_finished", "read"));

    match state.items.last().unwrap() {
        TranscriptItem::Subagent { children, .. } => {
            assert_eq!(children.len(), 2);
            let a = children.iter().find(|r| r.child_id == "sub-a").unwrap();
            let b = children.iter().find(|r| r.child_id == "sub-b").unwrap();
            assert_eq!(a.status, "running");
            assert_eq!(b.status, "completed");
        }
        _ => panic!("expected subagent group"),
    }

    apply_agent_event(&mut state, &AgentEvent::ToolExecutionStart {
        tool_call_id: "tc_sub_2".to_string(), tool_name: "subagent".to_string(), args: serde_json::Map::new(),
    });
    assert_eq!(state.items.iter().filter(|i| matches!(i, TranscriptItem::Subagent { .. })).count(), 2);
}

#[test]
fn test_subagent_group_render_shows_child_rows() {
    let mut state = make_test_state();
    state.items.push(TranscriptItem::Subagent {
        id: "tc_x".into(), mode: "coding".into(), status: "running".into(), is_error: false,
        children: vec![SubagentChildRow {
            child_id: "sub-a".into(), label: "read".into(),
            status: "running".into(), detail: "src/main.rs".into(),
        }],
    });
    let editor = /* reuse make_test_editor pattern from test_render_interactive_frame */;
    let session = /* reuse make_test_session pattern */;
    let frame = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
    assert!(frame.contains("subagent"), "header must name the tool");
    assert!(frame.contains("sub-a") || frame.contains("read"), "child row must be rendered");
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent test_subagent_group_renders_nested_child_rows_keyed_by_tool_call_id`
Expected: compile failure, `TranscriptItem::Subagent` missing.

- [ ] **Step 3: Implement the variant and event handling**

Add the struct/variant. In `apply_agent_event`, branch on `tool_name == "subagent"` in the three arms. For `ToolExecutionUpdate`, parse `partial_result.details.as_ref()?.get("subagent")?`; ignore updates without that key so unrelated tool updates still work. Insert or update a `SubagentChildRow` by `child_id`: `tool_started` → label = `detail`, status `running`; `tool_finished` → update that row's status to `completed` and keep its label; `finished`/`aborted`/`timed_out` → set group status. Keep the last 8 rows per group, same cap as today's `output_preview`.

- [ ] **Step 4: Implement rendering**

In the new `TranscriptItem::Subagent` arm of `render_interactive_frame`, print the header with existing theme tokens (`tool_pending`/`tool_success`/`tool_error`, `accent`, `dim`) using the same `wrap_text_with_ansi` call as `ToolCall`, then one indented line per child row. Use `⚙`, `✓`, `✗` by status exactly as the generic tool card does.

- [ ] **Step 5: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent test_subagent_group`
Expected: both tests pass.

- [ ] **Step 6: Add failure-path write test**

Extend `tests/test_subagent_lifecycle.rs` with a test that sets `MEHH_AGENT_STORE` to a path whose parent is a regular file (so `create_dir_all` fails) and asserts `execute()` returns `Err` mentioning the store path rather than silently succeeding. Restore/remove the env var in the test. If parallel test execution makes env mutation unsafe, instead call `write_manifest` directly with an invalid directory and assert `Err`.

- [ ] **Step 7: Full crate regression and commit**

Run: `cargo test -p mehh-coding-agent`

```bash
git add crates/mehh-coding-agent/src/interactive.rs crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs
git commit -m "feat(subagent): render nested subagent groups in interactive TUI"
```

---

## Final verification (after all tasks)

- [ ] `cargo test -p mehh-coding-agent` — all tests green
- [ ] `cargo check --workspace` — no type errors
- [ ] Manually run an interactive session, delegate a `coding` task, and confirm the nested group shows child tool rows in real time
- [ ] Confirm `<cwd>/.mehh-agents/<id>.json` contains a `started` event, child tool events, and a terminal event with `completed_at_ms` after the child finishes
`````

## docs/superpowers/plans/2026-11-20-review-suggestions.md

`````markdown
# Review Suggestions Follow-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the four non-blocking suggestions left by the independent reviewer: summary truncation direction, TUI `@file` whitespace/quoted paths, stdin blocking in print mode, and unescaped file-name markup.

**Architecture:** Four small, independent fixes in three crates. Each fix adds a pure helper with a focused unit test, then rewires exactly one call site to use it, so semantic decisions are testable without a TTY, a network, or an MCP server. Task 3 touches code that belongs to another uncommitted session; confirm ownership before editing.

**Tech Stack:** Rust workspace, `tokio` async I/O, `cargo test`, `mehh_ai::UserContentBlock`.

**Spec:** `docs/superpowers/specs/2026-11-20-review-suggestions-design.md`

## Global Constraints

- No release builds on this laptop. Use `cargo test -p <crate> --lib` and `cargo check -p <crate> --tests` only.
- TDD for every fix: write the failing test, watch it fail, add minimal code, watch it pass.
- Do not commit unless the user explicitly asks. If asked, stage only the paths listed in each task.
- Do not touch unrelated uncommitted work. Before Task 3, run `git diff -- crates/mehh-cli/src/main.rs` and confirm the stdin block is still the uncommitted version described in the spec.
- Keep these signatures unchanged: `compact_with_provider`, `expand_prompt_with_files`, `process_file_arguments`, `run_print_flow`.
- Reuse `crate::process_file_arguments`; do not reimplement file reads or base64 image handling.

## Review Focus

- Multi-byte text at the clamp boundary: must not panic on a non-char-boundary; pinned by Task 1 helper test with `é`.
- One enormous message with no whitespace: clamp must still terminate and keep the newest text; pinned by Task 1 `compact_with_provider_caps_oversized_history` assertion.
- `@"my notes.txt"` with spaces and quotes: must resolve the file and keep surrounding line breaks; pinned by Task 2 quoted-path test.
- Prompt text with newlines but no `@` token: output must be byte-identical to input; pinned by Task 2 identity test.
- Piped stdin open but prompt already supplied: must not call `read_to_string`; pinned by Task 3 predicate test with a non-empty prompt.

---

### Task 1: Keep newest history in compaction summary

**Files:**
- Modify: `crates/mehh-coding-agent/src/compaction.rs:534-541` and its `#[cfg(test)] mod tests`
- Test: `cargo test -p mehh-coding-agent --lib compaction::tests`

**Interfaces:**
- Consumes: `serialize_conversation(&[Message]) -> String` (`compaction.rs:274`) with chronological order (oldest first), produced by `prepare_compaction`.
- Produces: `fn keep_newest_chars(text: &str, budget_chars: usize) -> String` — private helper in `compaction.rs`; returns `text` unchanged when `text.chars().count() <= budget_chars`, otherwise returns `"[... older history truncated for summarization]\n\n"` followed by the last `budget_chars` characters, sliced on a char boundary.

- [ ] **Step 1: Write the failing helper test**

In `compaction.rs::tests`, add `keep_newest_chars_keeps_tail_and_is_char_boundary_safe`:

```rust
let input = format!("{}{}", "old ".repeat(4000), "é".repeat(10) + "NEWEST");
let out = super::keep_newest_chars(&input, 20);
assert!(out.starts_with("[... older history truncated for summarization]\n\n"));
assert!(out.contains("NEWEST"));
assert!(!out.contains("old "));
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-coding-agent --lib keep_newest_chars_keeps_tail`
Expected: FAIL with "cannot find function `keep_newest_chars`".

- [ ] **Step 3: Implement `keep_newest_chars`**

Add the helper next to `estimate_message_tokens`. Use `text.chars().count()` for the budget check, `char_indices().nth(count - budget_chars)` to find the start of the retained tail, and `&text[start..]` for the slice. Do not change `compact_with_provider`'s signature.

- [ ] **Step 4: Run the helper test**

Run: `cargo test -p mehh-coding-agent --lib keep_newest_chars_keeps_tail`
Expected: PASS.

- [ ] **Step 5: Update the end-to-end test fixture**

In `compact_with_provider_caps_oversized_history`, replace the single `"x".repeat(200_000)` message with two messages: `Message::User` holding `"OLDEST".repeat(20_000)` followed by `Message::User` holding `"NEWEST_MARKER"`. Add assertions that the recorded prompt contains `NEWEST_MARKER` and does not contain `OLDEST`, while keeping the existing `< 20_000` length assertion.

- [ ] **Step 6: Replace the inline clamp in `compact_with_provider`**

Substitute the current truncate-at-`nth(budget_chars)` block with `let serialized_conv = keep_newest_chars(&serialize_conversation(&prep.messages_to_summarize), budget_chars);`. Keep the existing `budget_chars` calculation unchanged.

- [ ] **Step 7: Run compaction tests**

Run: `cargo test -p mehh-coding-agent --lib compaction::tests`
Expected: all PASS, including the existing `test_compact_with_provider_faux`.

- [ ] **Step 8: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/compaction.rs && git commit -m "fix(compaction): keep newest history in summary prompt"`

---

### Task 2: Preserve whitespace and quoted `@file` paths in the TUI

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2497-2503` (`expand_prompt_with_files`) and `:2757` (autocomplete item)
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `crate::process_file_arguments(cwd: &Path, args: &[String]) -> anyhow::Result<ProcessedPromptArgs>` with fields `text: String`, `blocks: Vec<UserContentBlock>`.
- Produces: `fn file_completion_label(rel: &str) -> String` — private helper returning `@{rel}` unchanged unless `rel` contains whitespace, then `@"{rel}"`.
- Produces: `expand_prompt_with_files(cwd: &Path, text: &str) -> anyhow::Result<(String, Vec<UserContentBlock>)>` (same signature as today) — output text keeps all characters outside file tokens exactly; each file token is replaced by that file's `ProcessedPromptArgs.text`; returned blocks contain only non-`Text` blocks (images). `start_turn` already inserts one `Text` block for the assembled text.

- [ ] **Step 1: Write the identity failing test**

Add `expand_prompt_preserves_plain_text_verbatim`:

```rust
let input = "first line\n\n  second  line";
let (out, blocks) = expand_prompt_with_files(Path::new("."), input).unwrap();
assert_eq!(out, input);
assert!(blocks.is_empty());
```

- [ ] **Step 2: Write the quoted-path failing test**

Add `expand_prompt_resolves_quoted_path_with_spaces`: create a temp dir, write `my notes.txt` containing `BODY`, then call with `"keep\n\n@\"my notes.txt\" tail"`. Assert `out.contains("keep\n\n")`, `out.contains("<file name=\"my notes.txt\">")`, `out.contains("BODY")`, `!out.contains("@\"")`, and `blocks.iter().all(|b| !matches!(b, UserContentBlock::Text(_)))`.

- [ ] **Step 3: Write the autocomplete-label failing test**

Add `file_completion_label_quotes_whitespace`: `assert_eq!(file_completion_label("src/a.rs"), "@src/a.rs")` and `assert_eq!(file_completion_label("my notes.txt"), "@\"my notes.txt\"")`.

- [ ] **Step 4: Run the three tests and watch them fail**

Run: `cargo test -p mehh-coding-agent --lib expand_prompt` then `cargo test -p mehh-coding-agent --lib file_completion_label`
Expected: FAIL with missing function / wrong output (flattened newlines).

- [ ] **Step 5: Add `file_completion_label` and use it at `interactive.rs:2757`**

Replace `format!("@{rel}")` with `file_completion_label(&rel)`. Keep the label (second argument to `SelectItem::new`) as the original `rel`.

- [ ] **Step 6: Rewrite `expand_prompt_with_files`**

Scan with `char_indices`. A file token starts at `@` when it is first in the text or preceded by whitespace. If the next character is `"` or `'`, read to the matching quote; otherwise read until whitespace. Append non-token spans with `&text[start..end]` unchanged. For each token, call `process_file_arguments(cwd, &[format!("@{path}")])?`, push only non-`Text` blocks into the result, and append `processed.text` to the output. Keep the current early return when no token starts with `@`.

- [ ] **Step 7: Run the three tests**

Run: `cargo test -p mehh-coding-agent --lib expand_prompt` then `cargo test -p mehh-coding-agent --lib file_completion_label`
Expected: all PASS.

- [ ] **Step 8: Run the full crate suite**

Run: `cargo test -p mehh-coding-agent --lib`
Expected: 141+ passed, 0 failed.

- [ ] **Step 9: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/interactive.rs && git commit -m "fix(tui): preserve prompt whitespace and quoted file paths"`

---

### Task 3: Gate stdin reads in print mode

**Files:**
- Modify: `crates/mehh-cli/src/main.rs:994-1021` and its `#[cfg(test)] mod tests`; add `UserContentBlock` to the `mehh_ai` import at the top.
- Test: `cargo test -p mehh-cli`

**Interfaces:**
- Consumes: `io::stdin().is_terminal()` and `blocks: Vec<UserContentBlock>` assembled by `process_file_arguments`.
- Produces: `fn should_read_stdin(prompt_text: &str, blocks: &[UserContentBlock], stdin_is_terminal: bool) -> bool` — private helper in `main.rs`; returns true only when stdin is not a terminal AND `prompt_text.trim().is_empty()` AND `blocks.is_empty()`.

- [ ] **Step 0: Ownership check**

Run `git diff -- crates/mehh-cli/src/main.rs` and confirm the stdin block is still the other session's uncommitted change. If that session landed a different version, stop and re-scope this task rather than overwriting it.

- [ ] **Step 1: Write the failing predicate tests**

In `main.rs::tests`, add `should_read_stdin_only_when_prompt_is_empty`:

```rust
assert!(should_read_stdin("", &[], false));
assert!(!should_read_stdin("review", &[], false));
assert!(!should_read_stdin("", &[mehh_ai::UserContentBlock::Image(
    mehh_ai::ImageContent { data: "x".into(), mime_type: "image/png".into() }
)], false));
assert!(!should_read_stdin("", &[], true));
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-cli should_read_stdin_only_when_prompt_is_empty`
Expected: FAIL with "cannot find function `should_read_stdin`".

- [ ] **Step 3: Add the helper and rewire the call site**

Add `should_read_stdin` near `run_print_flow`. Replace `if !io::stdin().is_terminal() {` at `main.rs:1011` with `if should_read_stdin(&prompt_text, &blocks, io::stdin().is_terminal()) {`. Leave the read/append body unchanged.

- [ ] **Step 4: Run the CLI tests**

Run: `cargo test -p mehh-cli`
Expected: 48+ passed, 0 failed.

- [ ] **Step 5: Commit (only if the user asked)**

`git add crates/mehh-cli/src/main.rs && git commit -m "fix(cli): do not read piped stdin when a prompt is present"`

---

### Task 4: Escape file names in the prompt markup

**Files:**
- Modify: `crates/mehh-coding-agent/src/file_args.rs:79` and its test module
- Test: `cargo test -p mehh-coding-agent --lib file_args`

**Interfaces:**
- Consumes: `path_str` from `arg.strip_prefix('@')` in `process_file_arguments`.
- Produces: `fn escape_file_tag_value(raw: &str) -> String` — private helper in `file_args.rs`; escapes `&` first, then `"` -> `&quot;`, `'` -> `&apos;`, `<` -> `&lt;`, `>` -> `&gt;`.

- [ ] **Step 1: Write the failing helper test**

Add `file_tag_value_is_escaped`:

```rust
assert_eq!(
    super::escape_file_tag_value("a\"b<c>d&e'f"),
    "a&quot;b&lt;c&gt;d&amp;e&apos;f"
);
assert_eq!(super::escape_file_tag_value("notes.txt"), "notes.txt");
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-coding-agent --lib file_tag_value_is_escaped`
Expected: FAIL with "cannot find function `escape_file_tag_value`".

- [ ] **Step 3: Implement and use the helper**

Add `escape_file_tag_value` above `process_file_arguments`. Change the tag at `file_args.rs:79` to `format!("<file name=\"{}\">\n{clean}\n</file>", escape_file_tag_value(path_str))`. Do not change the file-content handling or image branch.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mehh-coding-agent --lib file_args` then `cargo test -p mehh-cli`
Expected: both PASS; the existing CLI `process_file_arguments_expands_at_text_with_bom_strip_and_at_images` still passes.

- [ ] **Step 5: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/file_args.rs && git commit -m "fix(prompt): escape file names in attached-file markup"`

---

## Final verification (after all four tasks)

- [ ] **Run both suites**

Run: `cargo test -p mehh-coding-agent --lib && cargo test -p mehh-cli && cargo check -p mehh-cli --tests`
Expected: 0 failures, `cargo check` finishes cleanly.

- [ ] **Re-run the independent review pipeline**

Regenerate per-file patches into a fresh scratch dir (include the untracked `file_args.rs` via `git diff --no-index /dev/null ...`), run the static scan, and dispatch a fresh reviewer with `requesting-code-review`. Only report done when `passed=true` with empty `security_concerns` and `logic_errors`.
`````

## docs/superpowers/plans/2026-11-21-tui-ux-optimization.md

`````markdown
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
`````

