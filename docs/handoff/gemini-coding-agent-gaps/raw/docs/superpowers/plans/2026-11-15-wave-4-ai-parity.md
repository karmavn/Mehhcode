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
