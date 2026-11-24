# Web Search & Web Fetch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add built-in, zero-config `web_search` and `web_fetch` tools to Mehhcode in Rust with 6 keyless providers, local-first HTML-to-Markdown fetch + automatic keyless extract fallback, and native integration into Mehhcode's SSRF, settings, permissions, session, TUI, and CLI layers.

**Architecture:** Consolidate connect-time SSRF and URL safety into `mehh-ai::http_safety` (shared by `mehh-mcp` and `mehh-tools`), implement the keyless web engine and both `AgentTool`s inside `crates/mehh-tools/src/web/`, and wire settings, URL-boundary permissions, async redirect policy checks, TUI summaries, and CLI tool selection through `mehh-coding-agent` and `mehh-cli`.

**Tech Stack:** Rust 2021 workspace (`reqwest` 0.12 with `rustls-tls`/`stream`/`json`, `tokio`, `tokio-util`, `futures`, `serde`, `serde_json`, `regex`, `uuid`, `anyhow`, `async-trait`, `tracing`).

**Spec:** `docs/superpowers/specs/2026-11-24-web-search-fetch-design.md`

## Global Constraints

- **Zero new external crates outside root `Cargo.toml`**: only add existing workspace dependencies (`reqwest`, `futures`, `uuid`, `tracing`) to `crates/mehh-tools/Cargo.toml`.
- **No `Accept-Encoding: gzip, br`**: workspace `reqwest` disables default features (`gzip`/`brotli` are off); never advertise compressed encodings manually.
- **Cross-platform & file hygiene**: UTF-8 without BOM on all files; portable across Windows, macOS, and Linux.
- **Cancellation & timeouts**: every HTTP call and streaming body read must observe `CancellationToken` via `tokio::select!` alongside per-request timeouts.
- **Fail-closed security & config**: strip UTF-8 BOM on config load, distinguish `ENOENT` from invalid JSON, never fall back to an insecure HTTP client, never loosen global security from a repo `.mehh/settings.json`.
- **Offline tests only**: all tests use local `tokio::net::TcpListener` mock servers and fixture payloads — zero live network requests.
- **Commits & CI**: per `AGENTS.md`, do not run `git commit` unless explicitly asked by the user; targeted `cargo test -p <crate> ...` commands listed in each task are the authoritative verification commands (run on CI or a capable build host if local compilation is resource-constrained).

## Review Focus

1. **Redirect hopping to private/cloud-metadata IP or `NeverAllow` URL** (including protocol-relative `Location: //127.0.0.1/...`): Stage 1 must abort on the hop without dialing the private target and must NOT trigger Stage 2 remote extract fallback. (Pinned in Task 6)
2. **Single-flight cancellation isolation**: when Caller 1 cancels its `CancellationToken` while an in-flight deduplicated search is running, Caller 2 waiting on the same normalized query must not hang or inherit Caller 1's cancellation, and no stale slot may remain in the single-flight map. (Pinned in Task 4)
3. **Adversarial HTML input** (1,000 nested `<div>` tags, unclosed `<script>` tag, giant `data:image/png;base64,...` URI): converter must remain stack-safe, emit zero `<script>` body text, and replace the base64 URI with `[image: <alt>]`. (Pinned in Task 5)
4. **SSE `data:` line framing with CJK / Unicode line separators (`U+2028` / `U+2029`)**: `mcp_rpc` must split SSE frames strictly on `\r\n`, `\r`, or `\n` so Unicode paragraph/line separators inside JSON strings do not corrupt the envelope. (Pinned in Task 2)
5. **URL permission prefix boundary bypasses**: an `AlwaysAllow` rule for `"https://docs.rs"` must NOT match `"https://docs.rs.evil.com"` or `"https://docs.rs:8443/x"`, `"https://docs.rs/reqwest"` must NOT match `"https://docs.rs/reqwest-evil"`, while `"HTTPS://DOCS.RS:443/reqwest/latest"` MUST match. (Pinned in Task 7)

---

### Task 1: Shared SSRF & URL Safety in `mehh-ai` + `mehh-mcp` Migration + Anthropic OAuth Tool Mapping

**Files:**
- Create: `crates/mehh-ai/src/http_safety.rs`
- Modify: `crates/mehh-ai/src/lib.rs:1-42`
- Modify: `crates/mehh-ai/src/anthropic.rs:245-264`
- Modify: `crates/mehh-mcp/src/oauth.rs:110-255`
- Modify: `crates/mehh-mcp/src/transport.rs:398-420`

**Interfaces:**
- Consumes: `reqwest`, `tokio::net::lookup_host`, `anyhow` (all already in `mehh-ai/Cargo.toml`)
- Produces:
  - `mehh_ai::http_safety::DnsPolicy` (`PublicOnly`, `PublicAndLoopback`, `AllowPrivate`)
  - `mehh_ai::http_safety::SafeDnsResolver::new(policy: DnsPolicy) -> Self` (implements `reqwest::dns::Resolve`)
  - `mehh_ai::http_safety::is_always_blocked_ip(ip: std::net::IpAddr) -> bool`
  - `mehh_ai::http_safety::is_always_blocked_hostname(host: &str) -> bool`
  - `mehh_ai::http_safety::is_private_or_local_ip(ip: std::net::IpAddr) -> bool`
  - `mehh_ai::http_safety::is_local_or_internal_hostname(host: &str) -> bool`
  - `mehh_ai::http_safety::is_loopback_host(host: &str) -> bool`
  - `mehh_ai::http_safety::build_safe_http_client(policy: DnsPolicy, connect_timeout: Duration, request_timeout: Option<Duration>) -> anyhow::Result<reqwest::Client>`
  - `mehh_ai::http_safety::normalize_and_validate_http_url(raw: &str) -> Result<reqwest::Url, String>`
  - `mehh_ai::http_safety::is_eligible_for_remote_extract(url: &reqwest::Url) -> Result<(), &'static str>`
  - Updated `mehh_ai::anthropic::{to_claude_code_name, from_claude_code_name}` matching case-insensitively ignoring `_`

- [ ] **Step 1: Write failing tests in `crates/mehh-ai/src/http_safety.rs` and `crates/mehh-ai/src/anthropic.rs`**

Add unit tests in `http_safety.rs` asserting:
- `test_always_blocked_metadata_floor`: `169.254.169.254`, `169.254.170.2`, `169.254.169.253`, `100.100.100.200`, `fd00:ec2::254`, `::ffff:169.254.169.254`, `::ffff:0:169.254.169.254`, `64:ff9b::169.254.169.254`, `metadata.google.internal.`, `METADATA.GOOG` are all blocked even under `DnsPolicy::AllowPrivate`.
- `test_private_and_special_use_ips`: RFC 1918 (`10.0.0.1`, `172.16.0.1`, `192.168.1.1`), loopback (`127.0.0.1`, `::1`), CGNAT (`100.64.0.1`), IPv6 ULA (`fc00::1`), NAT64 (`64:ff9b::10.0.0.1`), and IPv6 special-use (`100::1`, `2001:2::1`, `2001:20::1`) are blocked under `PublicOnly`; `127.0.0.1` is allowed under `PublicAndLoopback` and `AllowPrivate`; `10.0.0.1` is blocked under `PublicAndLoopback` and allowed under `AllowPrivate`.
- `test_normalize_and_validate_http_url`: repairs `"  https:// docs.rs/path#frag  "` -> `"https://docs.rs/path"`, rejects `file:///etc/passwd`, rejects `https://user:pass@example.com`, rejects `https://user@example.com`.
- `test_is_eligible_for_remote_extract`: accepts `"https://docs.rs/reqwest/latest"`, rejects `localhost`, `127.0.0.1`, `10.0.0.5`, `.localhost`, `?api_key=abc`, `?token=xyz`, `?X-Amz-Signature=deadbeef`, `/path/sk-ant-123456`, `/path/ghp_123456`, `/path/eyJhbGciOiJIUzI1NiJ9`.

Add unit test in `crates/mehh-ai/src/anthropic.rs`:
- `test_claude_code_name_roundtrip_snake_case`: `to_claude_code_name("web_search") == "WebSearch"`, `to_claude_code_name("web_fetch") == "WebFetch"`, `from_claude_code_name("WebSearch", &declared) == "web_search"`, `from_claude_code_name("WebFetch", &declared) == "web_fetch"`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-ai http_safety` and `cargo test -p mehh-ai test_claude_code_name_roundtrip_snake_case`
Expected: FAIL (module not yet created / `web_search` returns unchanged `"web_search"`).

- [ ] **Step 3: Implement `crates/mehh-ai/src/http_safety.rs`, export in `lib.rs`, update `anthropic.rs`, and migrate `mehh-mcp`**

- Implement `http_safety.rs` moving the IPv4/IPv6 checks from `crates/mehh-mcp/src/oauth.rs:163-237` and adding the cloud metadata floor, IPv4-translated (`::ffff:0:0:0/96`), IPv6 special-use (`100::/64`, `2001:2::/48`, `2001:20::/28`), `DnsPolicy`, `SafeDnsResolver`, `build_safe_http_client`, `normalize_and_validate_http_url`, and `is_eligible_for_remote_extract`.
- Re-export `pub mod http_safety; pub use http_safety::*;` in `crates/mehh-ai/src/lib.rs`.
- In `crates/mehh-ai/src/anthropic.rs`, compare tool names using a helper that ignores `_` ASCII-case-insensitively (preserving exact match precedence first).
- In `crates/mehh-mcp/src/oauth.rs`, delete the duplicate `SafeDnsResolver`, `is_loopback_host`, `is_local_or_internal_hostname`, and `is_private_or_local_ip`; import them from `mehh_ai::http_safety`; delegate `oauth_http_client()` to `build_safe_http_client(DnsPolicy::PublicAndLoopback, Duration::from_secs(15), Some(OAUTH_HTTP_TIMEOUT))`.
- In `crates/mehh-mcp/src/transport.rs:405`, replace `crate::oauth::SafeDnsResolver` with `mehh_ai::http_safety::SafeDnsResolver::new(mehh_ai::http_safety::DnsPolicy::PublicAndLoopback)`.

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-ai http_safety` and `cargo test -p mehh-ai test_claude_code_name_roundtrip_snake_case` and `cargo test -p mehh-mcp oauth`
Expected: PASS (all existing `mehh-mcp` OAuth tests stay green alongside new `mehh-ai` tests).

---

### Task 2: Web Domain Types & Bounded MCP JSON-RPC / SSE Transport

**Files:**
- Modify: `crates/mehh-tools/Cargo.toml`
- Create: `crates/mehh-tools/src/web/types.rs`
- Create: `crates/mehh-tools/src/web/mcp_rpc.rs`

**Interfaces:**
- Consumes: `reqwest`, `futures::StreamExt`, `tokio::select!`, `tokio_util::sync::CancellationToken`, `serde`, `serde_json`
- Produces:
  - `ProviderId` enum (`Exa`, `Parallel`, `Firecrawl`, `Tavily`, `TinyFish`, `Keenable`) with `as_str(&self) -> &'static str`, `display_name(&self) -> &'static str`, `from_str_id(s: &str) -> Option<Self>`, `ALL_SEARCH: [ProviderId; 6]`, `ALL_EXTRACT: [ProviderId; 4]` (`[Exa, Parallel, Firecrawl, Keenable]`)
  - `WebProviderChoice` enum (`Auto`, `Fixed(ProviderId)`, `Disabled`)
  - `SearchHit { pub url: String, pub title: Option<String>, pub published: Option<String>, pub content: Option<String> }`
  - `ExtractedPage { pub url: String, pub final_url: Option<String>, pub title: Option<String>, pub content: String }`
  - `ProviderError` enum (`RateLimited { retry_after: Option<Duration>, detail: String }`, `VendorUnavailable { status: Option<u16>, detail: String }`, `OriginBlocked { status: u16, detail: String }`, `BadRequest(String)`, `Cancelled`)
  - `McpToolResult { pub text: String, pub structured_content: Option<serde_json::Value> }`
  - `mcp_rpc::call_mcp_tool(client: &reqwest::Client, url: &str, tool_name: &str, arguments: serde_json::Value, extra_headers: &[(&str, &str)], timeout: Duration, cancel: &CancellationToken) -> Result<McpToolResult, ProviderError>`
  - `mcp_rpc::read_bounded_body(response: reqwest::Response, max_bytes: usize, cancel: &CancellationToken) -> Result<Vec<u8>, ProviderError>`
  - `mcp_rpc::parse_retry_after(header: Option<&str>, now: std::time::SystemTime) -> Option<Duration>`
  - `mcp_rpc::classify_http_status(status: u16, retry_after: Option<&str>, body_snippet: &str) -> ProviderError`

- [ ] **Step 1: Add workspace dependencies to `crates/mehh-tools/Cargo.toml` and write failing tests in `types.rs` & `mcp_rpc.rs`**

Add `reqwest = { workspace = true }`, `futures = { workspace = true }`, `uuid = { workspace = true }`, `tracing = { workspace = true }` to `crates/mehh-tools/Cargo.toml`.

Write tests in `mcp_rpc.rs`:
- `test_parse_mcp_plain_json_and_structured_content`: parses direct JSON-RPC `result.content[].text` and preserves `result.structuredContent`.
- `test_parse_mcp_sse_preserves_unicode_line_separators` *(Review Focus #4)*: SSE body containing `data: {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"line1\u{2028}line2\u{2029}end"}]}}\r\n\r\n` parses intact without splitting on `U+2028`/`U+2029`.
- `test_parse_mcp_is_error_rate_limit_classification`: `result.isError = true` with `"Rate limit exceeded (429)"` returns `ProviderError::RateLimited`; non-rate-limit `isError` returns `ProviderError::VendorUnavailable`.
- `test_parse_retry_after_clamps_bounds`: `"2"` -> `5s`, `"600"` -> `300s`, `"45"` -> `45s`, HTTP-date 90s in future -> `90s`, invalid -> `None`.
- `test_bounded_body_exceeds_512k_fails_cleanly`: mock `TcpListener` streaming > `512 KiB` returns `ProviderError::VendorUnavailable` without buffering entire stream.
- `test_cancellation_aborts_in_flight_mcp_call`: mock `TcpListener` holding socket open returns `ProviderError::Cancelled` immediately when `cancel.cancel()` fires.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-tools web::mcp_rpc`
Expected: FAIL (unimplemented).

- [ ] **Step 3: Implement `types.rs` and `mcp_rpc.rs`**

- Implement closed domain types in `types.rs`.
- Implement `mcp_rpc.rs`: POST JSON-RPC 2.0 `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":...,"arguments":...}}` with `Accept: application/json, text/event-stream` and `User-Agent: mehh-code/0.1`. Stream body up to `MAX_PROVIDER_RESPONSE_BYTES = 512 * 1024` under `tokio::select!` (`cancel.cancelled()`, `tokio::time::sleep(timeout)`, `stream.next()`). Split SSE lines strictly on `\r\n`, `\r`, or `\n` (never `str::lines()`).

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-tools web::types` and `cargo test -p mehh-tools web::mcp_rpc`
Expected: PASS.

---

### Task 3: 6 Keyless Search & 4 Keyless Extract Provider Adapters

**Files:**
- Create: `crates/mehh-tools/src/web/providers.rs`

**Interfaces:**
- Consumes: `types::{ProviderId, SearchHit, ExtractedPage, ProviderError}`, `mcp_rpc::{call_mcp_tool, read_bounded_body, classify_http_status, MAX_PROVIDER_RESPONSE_BYTES}`
- Produces:
  - `WebEndpoints` struct holding configurable base/endpoint URLs for all 6 providers (with `WebEndpoints::production()` defaults for production and per-field overrides for tests)
  - `providers::execute_search(client: &reqwest::Client, endpoints: &WebEndpoints, session_id: &str, provider: ProviderId, query: &str, timeout: Duration, cancel: &CancellationToken) -> Result<Vec<SearchHit>, ProviderError>`
  - `providers::execute_extract(client: &reqwest::Client, endpoints: &WebEndpoints, session_id: &str, provider: ProviderId, url: &str, timeout: Duration, cancel: &CancellationToken) -> Result<ExtractedPage, ProviderError>`

- [ ] **Step 1: Write failing mock-server tests in `crates/mehh-tools/src/web/providers.rs`**

Spin up local `tokio::net::TcpListener` mocks asserting exact outbound HTTP method, path, headers, JSON payload, and response parsing for each vendor:
- `test_exa_search_and_extract`: verifies `web_search_exa` `{ query, numResults: 8 }` parsing `---`-separated `Title:/URL:/Published:/Highlights:` blocks (SSE response); verifies `web_fetch_exa` `{ urls: [url] }` and verifies `CRAWL_HTTP_429` & `CRAWL_NOT_FOUND` map to `ProviderError::OriginBlocked` (429 / 404).
- `test_parallel_search_and_extract`: verifies `web_search` `{ objective, search_queries: [query], session_id }` reading `structuredContent.results[]` (and JSON text fallback); verifies `web_fetch` preferring `full_content` -> `content` -> `excerpts`, and `errors[0].http_status_code: 403` mapping to `OriginBlocked(403)`.
- `test_firecrawl_search_mcp_and_extract_rest`: verifies `firecrawl_search` MCP `{ query, limit: 8 }` parsing `data.web[]`; verifies REST `POST /v2/scrape` with NO `Authorization` header reading `data.markdown`, `data.metadata.title`, `data.metadata.url` (distinct from `sourceURL`) as `final_url`, and `data.metadata.statusCode: 403` mapping to `OriginBlocked(403)`.
- `test_tavily_keyless_search`: verifies headers `X-Tavily-Access-Mode: keyless` and `X-Client-Name: mehh-code`, body `{ query, search_depth: "basic", chunks_per_source: 3, max_results: 8 }`, parsing `results[]`.
- `test_tinyfish_keyless_search`: verifies header `X-TinyFish-Access-Mode: keyless` with NO `initialize` call, MCP tool `"search"` `{ query }`, parsing `results[]` (`url`, `title`, `snippet`).
- `test_keenable_search_and_extract`: verifies header `X-Keenable-Title: mehh-code` on `POST /v1/search/public` (`{ query, max_results: 8 }`) and `GET /v1/fetch/public?url=...`; verifies HTTP 403 with `{"message":"The target server returned 403"}` and HTTP 404 with `{"message":"The page does not exist"}` map to `OriginBlocked`, while HTTP 429 with `{"message":"Rate limit exceeded"}` maps to `RateLimited` and HTTP 403 with `{"message":"Anonymous quota exhausted"}` maps to `VendorUnavailable`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-tools web::providers`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/mehh-tools/src/web/providers.rs`**

Implement all 6 search adapters and 4 extract adapters strictly matching the verified wire protocols in Spec Section 5.2, sending `User-Agent: mehh-code/0.1` and zero user/machine/model identifiers.

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-tools web::providers`
Expected: PASS.

---

### Task 4: Sticky Failover Router, Cooldowns & Single-Flight Search Memo

**Files:**
- Create: `crates/mehh-tools/src/web/router.rs`

**Interfaces:**
- Consumes: `types::{ProviderId, WebProviderChoice, SearchHit, ExtractedPage, ProviderError}`, `providers::{WebEndpoints, execute_search, execute_extract}`
- Produces:
  - `SearchOutput { pub provider: ProviderId, pub hits: Vec<SearchHit>, pub cached: bool }`
  - `ExtractOutput { pub provider: ProviderId, pub page: ExtractedPage }`
  - `ProgressFn = dyn Fn(&str) + Send + Sync` (optional callback invoked with status lines)
  - `WebRouter`: thread-safe router owning `session_id: String`, sticky search/extract preferences, shared cooldown map (`HashMap<ProviderId, Instant>`), 15m/128-entry in-memory search memo, and single-flight slot map
  - `WebRouter::new(endpoints: WebEndpoints, client: reqwest::Client) -> Self`
  - `WebRouter::search(&self, choice: WebProviderChoice, query: &str, timeout: Duration, cancel: &CancellationToken, on_progress: Option<&ProgressFn>) -> Result<SearchOutput, ProviderError>`
  - `WebRouter::extract_fallback(&self, choice: WebProviderChoice, url: &str, timeout: Duration, cancel: &CancellationToken, on_progress: Option<&ProgressFn>) -> Result<ExtractOutput, ProviderError>`

- [ ] **Step 1: Write failing tests in `crates/mehh-tools/src/web/router.rs`**

Use mock HTTP backends (or an internal injectable `ProviderExecutor` trait defaulting to `providers::execute_*` for fast deterministic tests):
- `test_auto_search_is_sticky_until_cooldown`: two consecutive different queries hit the same chosen provider; when that provider returns `RateLimited(Retry-After: 60s)`, router rotates to another provider, updates sticky affinity, and skips the cooled-down provider on the next query.
- `test_auto_search_rotates_on_401_403_5xx_and_stops_on_bad_request`: `VendorUnavailable` rotates (setting 300s cooldown for 401/402/403, 30s for 5xx/timeout); `BadRequest` returns immediately without trying other vendors.
- `test_fixed_search_never_fails_over`: `WebProviderChoice::Fixed(ProviderId::Tavily)` failing with 429 returns error directly without calling any other provider.
- `test_extract_fallback_rotates_and_prioritizes_fixed_extractor`:
  - `Fixed(ProviderId::Exa)` tries `Exa` first; when `Exa` returns `RateLimited`, rotates to remaining extractors `[Parallel, Firecrawl, Keenable]`.
  - `Fixed(ProviderId::Tavily)` (search-only) rotates across `[Exa, Parallel, Firecrawl, Keenable]`.
  - `Disabled` returns error immediately without calling any extractor.
  - `OriginBlocked` stops immediately without cooling down or rotating vendor.
- `test_search_memo_normalizes_query_and_evicts_at_128`: `"  Rust   Reqwest "` and `"rust reqwest"` share one cache hit (`cached == true`); errors are never cached; inserting 129 distinct queries keeps len <= 128.
- `test_single_flight_deduplicates_and_isolates_cancellation` *(Review Focus #2)*: two concurrent calls for the same query trigger only 1 outbound executor call and both receive the result; in a second case where Caller 1 cancels mid-flight while Caller 2 is waiting, Caller 1 gets `ProviderError::Cancelled`, Caller 2 still completes cleanly with the search result, and the single-flight map is empty afterward.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-tools web::router`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/mehh-tools/src/web/router.rs`**

- Implement single-flight using an owned background/detached inner future or a shared `tokio::sync::watch` result channel whose worker task runs with a child/independent cancellation that only cancels if ALL waiters drop (so Caller 1 cancelling never aborts Caller 2), cleaning up the slot via an RAII guard on completion/drop.
- Implement sticky affinity + cooldown tracking (`RateLimited`: `Retry-After` clamped `5s..=300s`, default `60s`; `VendorUnavailable` 401/402/403: `300s`; 5xx/transport/timeout: `30s`). If all candidates are cooling down, try the earliest-expiring one once before returning the last error + attempted provider names.

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-tools web::router`
Expected: PASS.

---

### Task 5: Bounded Zero-Dep HTML-to-Markdown Converter & Output Truncation / Spill

**Files:**
- Create: `crates/mehh-tools/src/web/html_markdown.rs`
- Create: `crates/mehh-tools/src/web/output.rs`

**Interfaces:**
- Consumes: `mehh_tools::core::output::floor_char_boundary`, `reqwest::Url`, `uuid::Uuid`
- Produces:
  - `html_markdown::MAX_HTML_BYTES: usize` (`5 * 1024 * 1024`)
  - `html_markdown::MAX_NESTING_DEPTH: usize` (`256`)
  - `html_markdown::html_to_markdown(html: &str, base_url: Option<&reqwest::Url>) -> String`
  - `html_markdown::html_to_text(html: &str) -> String`
  - `html_markdown::strip_markdown_to_text(md: &str) -> String`
  - `html_markdown::looks_like_empty_spa_shell(raw_html: &str, rendered: &str) -> bool`
  - `output::FETCH_INLINE_MAX_CHARS: usize` (`20_000`)
  - `output::SEARCH_INLINE_MAX_CHARS: usize` (`16_000`)
  - `output::FormattedOutput { pub text: String, pub truncated: bool, pub spilled_path: Option<PathBuf> }`
  - `output::format_search_results(hits: &[SearchHit]) -> FormattedOutput`
  - `output::format_fetch_content(content: &str, extension: &str) -> FormattedOutput`

- [ ] **Step 1: Write failing tests in `html_markdown.rs` and `output.rs`**

Tests in `html_markdown.rs`:
- `test_html_to_markdown_core_elements`: converts `<h1>..<h3>`, `<p>`, `<strong>`, `<em>`, `<code>`, `<pre><code class="language-rust">`, `<a href="/guide">Guide</a>` resolved against `https://docs.rs/pkg/`, nested `<ul>`/`<ol>`, `<blockquote>`, `<hr>`, and a 2x2 `<table>` into clean GFM Markdown; decodes `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, `&nbsp;`, `&#x27;`.
- `test_html_to_markdown_adversarial_inputs` *(Review Focus #3)*:
  - 1,000 nested `<div>` tags complete without panic/stack overflow and preserve inner text up to depth cap.
  - Unclosed `<script>var secret = 1; alert(1);` emits zero script text.
  - `<img alt="arch diagram" src="data:image/png;base64,AAAA...">` renders as `[image: arch diagram]` without any base64 payload.
- `test_html_to_text_skips_noise_tags`: strips `<script>`, `<style>`, `<noscript>`, `<template>`, `<iframe>` and collapses whitespace.
- `test_looks_like_empty_spa_shell`: returns `true` for `<html><body><div id="root"></div><script src="/app.js"></script></body></html>` (rendered `< 200` non-whitespace chars + SPA marker), `false` for short static page with no `<script>` or for pages with >= 200 non-whitespace chars.

Tests in `output.rs`:
- `test_format_search_results_empty_and_markdown`: empty slice returns `"No search results found. Please try a different query."`; non-empty returns `## [Title](url)\nPublished: ...\n\ncontent` truncated at `16_000` chars on valid UTF-8 boundary.
- `test_format_fetch_content_head_tail_and_spill`: string of `25_000` chars (including multi-byte Vietnamese/emoji chars around cut points) preserves 75% head (`15_000` chars) + 25% tail (`5_000` chars), writes complete original UTF-8 (without BOM) to `<temp_dir>/mehh-web-fetch/<uuid>.md`, sets `truncated = true`, includes `spilled_path` in footer notice, and reading `spilled_path` via `ReadTool` returns the full uncorrupted text.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-tools web::html_markdown` and `cargo test -p mehh-tools web::output`
Expected: FAIL.

- [ ] **Step 3: Implement `html_markdown.rs` and `output.rs`**

- Implement a streaming character-state HTML tokenizer (iterative tag stack capped at `MAX_NESTING_DEPTH = 256`, no recursion) and Markdown/text emitters in `html_markdown.rs`.
- Implement `output.rs` using UTF-8 char-boundary slicing (`floor_char_boundary` or char-count index helper), creating `<temp_dir>/mehh-web-fetch` with restricted permissions on Unix (`0o700` dir / `0o600` file).

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-tools web::html_markdown` and `cargo test -p mehh-tools web::output`
Expected: PASS.

---

### Task 6: `WebSearchTool`, `WebFetchTool` (2-Stage Hybrid Pipeline) & `mehh-tools` Registration

**Files:**
- Create: `crates/mehh-tools/src/web/mod.rs`
- Create: `crates/mehh-tools/src/web/search_tool.rs`
- Create: `crates/mehh-tools/src/web/fetch_tool.rs`
- Modify: `crates/mehh-tools/src/lib.rs:1-145,540-585`

**Interfaces:**
- Consumes: `mehh_ai::http_safety::*`, `types::*`, `providers::WebEndpoints`, `router::WebRouter`, `html_markdown::*`, `output::*`, `mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode}`
- Produces:
  - `#[async_trait] pub trait UrlPolicyChecker: Send + Sync { async fn check_url(&self, url: &str) -> Result<(), String>; }`
  - `WebConfig { pub enabled: bool, pub provider: WebProviderChoice, pub allow_private_urls: bool, pub fetch_fallback: bool, pub timeout_seconds: u64, pub endpoints: WebEndpoints, pub url_policy_checker: Option<Arc<dyn UrlPolicyChecker>> }` (with `Default` = enabled, `Auto`, `allow_private_urls: false`, `fetch_fallback: true`, `timeout_seconds: 30`, plus builder `.with_url_policy_checker(...)`)
  - `WebRuntime` (holds `config: WebConfig`, `local_fetch_client: reqwest::Client`, `router: WebRouter`)
  - `WebSearchTool::new(runtime: Arc<WebRuntime>) -> Self` (`name() == "web_search"`, `is_read_only() == Some(true)`)
  - `WebFetchTool::new(runtime: Arc<WebRuntime>) -> Self` (`name() == "web_fetch"`, `is_read_only() == Some(true)`)
  - `mehh_tools::create_coding_tools_with_web(cwd: PathBuf, web: WebConfig) -> Vec<Arc<dyn AgentTool>>`
  - `mehh_tools::select_coding_tools_with_web(cwd: PathBuf, include: Option<&[String]>, exclude: &[String], web: WebConfig) -> anyhow::Result<Vec<Arc<dyn AgentTool>>>`

- [ ] **Step 1: Write failing tests in `search_tool.rs`, `fetch_tool.rs`, and `crates/mehh-tools/src/lib.rs`**

Tests in `search_tool.rs`:
- `test_web_search_tool_contract_and_progress`: `is_read_only() == Some(true)`, `name() == "web_search"`, calls mock provider, emits `"Searching via ..."` through `on_update`, returns Markdown + `details.provider` / `details.resultCount` / `details.cached`.
- `test_web_search_disabled_returns_error`: when `config.enabled == false` or `config.provider == Disabled`, `execute` returns clear `Err(...)`.

Tests in `fetch_tool.rs`:
- `test_local_fetch_markdown_text_html`: serves HTML and `text/markdown` from mock server (with `allow_private_urls: true` for loopback mock), verifies `Accept` header negotiation and `markdown` / `text` / `html` conversion.
- `test_cloudflare_challenge_retries_with_honest_ua`: first request returns `403` + `cf-mitigated: challenge`, retry checks `User-Agent == "mehh-code"` and succeeds `200`.
- `test_redirect_limit_and_ssrf_and_policy_blocks` *(Review Focus #1)*:
  - 11 redirects fail with `"Too many redirects"`.
  - Redirect from mock server to `http://169.254.169.254/latest/meta-data` or `//127.0.0.1:9/admin` (under `PublicOnly`) fails closed without dialing metadata and WITHOUT triggering Stage 2 fallback.
  - Redirect to a URL rejected by mock `UrlPolicyChecker` fails closed and does NOT trigger Stage 2 fallback.
- `test_stage2_fallback_on_403_and_spa_shell`:
  - Stage 1 `403` (without challenge recovery) triggers Stage 2 mock extractor (verified with a test helper overriding eligibility for loopback mock or using a dedicated public-eligibility flag/test hook cleanly) and returns `details.source == "<provider>"`.
  - Stage 1 empty SPA HTML (`<div id="root"></div><script></script>`) triggers Stage 2 fallback; `format == "html"`, HTTP `404`, HTTP `401`, and secret-bearing URL `?token=secret123` do NOT trigger Stage 2 fallback.
  - Stage 2 post-redirect `final_url` violating `PublicOnly` SSRF or `UrlPolicyChecker` is rejected before returning content.
- `test_mime_gate_and_5mib_cap`: `image/png` and `application/pdf` and binary sniffed `%PDF-` are rejected cleanly; response > `5 MiB` aborts streaming cleanly.

Tests in `crates/mehh-tools/src/lib.rs`:
- Update `test_create_coding_tools` (now 11 default tools including `"web_search"` and `"web_fetch"`) and `create_coding_tools_excludes_powershell_while_create_all_tools_includes_it` (11 default / 12 all / 10 without bash).
- Add `test_web_config_omits_disabled_tools`: `enabled: false` omits both `web_search` and `web_fetch` from defaults; `provider: Disabled` omits `web_search` while keeping `web_fetch`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-tools web` and `cargo test -p mehh-tools test_create_coding_tools`
Expected: FAIL.

- [ ] **Step 3: Implement `mod.rs`, `search_tool.rs`, `fetch_tool.rs`, and update `crates/mehh-tools/src/lib.rs`**

- Build `local_fetch_client` with `build_safe_http_client(if config.allow_private_urls { DnsPolicy::AllowPrivate } else { DnsPolicy::PublicOnly }, ...)` and `router_client` with `build_safe_http_client(DnsPolicy::PublicOnly, ...)` (allowing `AllowPrivate` only for test-overridden endpoints via a dedicated test constructor `WebRuntime::for_tests(...)` so production provider calls ALWAYS use `DnsPolicy::PublicOnly`).
- Implement `WebFetchTool` 3-stage pipeline strictly following Spec Section 5.3, observing `CancellationToken` and total deadline (`timeout.clamp(1, 120)` default `config.timeout_seconds`).
- Register `"web_search"` and `"web_fetch"` in `ALL_TOOL_NAMES` in `crates/mehh-tools/src/lib.rs`, sharing one `Arc<WebRuntime>` across both tools in `create_coding_tools_with_web` / `select_coding_tools_with_web`.

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-tools web` and `cargo test -p mehh-tools test_create_coding_tools` and `cargo test -p mehh-tools create_coding_tools_excludes_powershell`
Expected: PASS.

---

### Task 7: Native Settings, Project Escalation Guard & URL-Boundary Permissions

**Files:**
- Modify: `crates/mehh-coding-agent/src/settings.rs:170-360`
- Modify: `crates/mehh-coding-agent/src/permissions.rs:120-265,369-388`
- Modify: `crates/mehh-coding-agent/src/tool_inspection.rs:475-536,640-710`

**Interfaces:**
- Consumes: `mehh_tools::web::{WebConfig, WebProviderChoice, ProviderId, UrlPolicyChecker}`, `mehh_ai::http_safety::normalize_and_validate_http_url`
- Produces:
  - `WebSettings { pub enabled: Option<bool>, pub provider: Option<String>, pub allow_private_urls: Option<bool>, pub fetch_fallback: Option<bool>, pub timeout_seconds: Option<u64> }` (`#[serde(rename_all = "camelCase")]`)
  - `Settings.web: Option<WebSettings>`
  - `Settings::to_web_config(&self) -> mehh_tools::web::WebConfig` (fails closed on unrecognized `provider` string -> `WebProviderChoice::Disabled` or validates on deserialize via a typed `WebProviderSetting` enum — use a typed serde enum `WebProviderSetting` so invalid JSON values fail closed at `SettingsManager::load_file`!)
  - `ToolInspectionManager::check_url_not_denied(&self, url: &str) -> Result<(), String>`
  - `impl UrlPolicyChecker for ToolInspectionManager`

- [ ] **Step 1: Write failing tests in `settings.rs`, `permissions.rs`, and `tool_inspection.rs`**

Tests in `settings.rs`:
- `test_web_settings_roundtrip_and_merge`: camelCase JSON (`allowPrivateUrls`, `fetchFallback`, `timeoutSeconds`, `provider: "exa"`) round-trips; invalid `provider: "typo"` fails JSON parse (fail-closed); `SettingsManager::merge` merges field-by-field.
- `test_web_settings_project_escalation_guard`:
  - Repo `.mehh/settings.json` attempting `allowPrivateUrls: true` when global is unset/`false` is neutralized to `None` (`false`).
  - Repo attempting `enabled: true` or `fetchFallback: true` when global is `false` is neutralized.
  - Repo attempting `provider: "auto"` when global is `"disabled"` or `"exa"`, or `provider: "keenable"` when global is `"exa"`, is neutralized; repo tightening `"auto" -> "exa"`, `"auto" -> "disabled"`, or `"exa" -> "disabled"` is allowed.

Tests in `permissions.rs` *(Review Focus #5)*:
- `test_url_arg_prefix_boundary_matching`:
  - `AlwaysAllow` on `web_fetch` with `arg_prefix: "https://docs.rs"` matches `"https://docs.rs"`, `"https://docs.rs/reqwest"`, `"HTTPS://DOCS.RS:443/reqwest#sec"`; does NOT match `"https://docs.rs.evil.com"`, `"https://docs.rs:8443/x"`, `"http://docs.rs"`.
  - `NeverAllow` on `web_fetch` with `arg_prefix: "https://docs.rs/reqwest"` matches `"https://docs.rs/reqwest"` and `"https://docs.rs/reqwest/latest"`; does NOT match `"https://docs.rs/reqwest-evil"`.
  - Malformed `arg_prefix` on a `web_fetch` rule in `permissions.json` fails `read_rules_from_file` (so `load_fail_closed` denies all).

Tests in `tool_inspection.rs`:
- `test_web_tools_permission_modes_and_url_policy_checker`:
  - `web_search` and `web_fetch` (`is_read_only: Some(true)`) are `Allow` in `SmartApprove` and `Chat`, `RequireApproval` with URL/query `preview` in `Approve`.
  - `ToolInspectionManager::check_url_not_denied` returns `Err` for a `NeverAllow` `web_fetch` URL rule and `Ok(())` otherwise.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-coding-agent test_web_settings` and `cargo test -p mehh-coding-agent test_url_arg_prefix` and `cargo test -p mehh-coding-agent test_web_tools_permission`
Expected: FAIL.

- [ ] **Step 3: Implement changes in `settings.rs`, `permissions.rs`, and `tool_inspection.rs`**

- Define `WebProviderSetting` serde enum (`Auto`, `Disabled`, `Exa`, `Parallel`, `Firecrawl`, `Tavily`, `TinyFish`, `Keenable`, `#[serde(rename_all = "lowercase")]`) so unknown provider names fail closed on `load_file`. Implement `merge`, escalation guard in `try_load_from_paths`, and `Settings::to_web_config()`.
- In `permissions.rs`, implement `url_matches_prefix(raw_url: &str, raw_prefix: &str) -> bool` using `normalize_and_validate_http_url`, checking exact scheme + lowercased host + effective port (stripping `:80`/`:443`), path-segment boundary, and optional query prefix boundary. Validate `tool_name.eq_ignore_ascii_case("web_fetch")` `arg_prefix` entries inside `read_rules_from_file`. Include `url`/`query` in `AskBefore` preview.
- In `tool_inspection.rs`, include `url`/`query` in `Approve` preview, add `check_url_not_denied(&self, url: &str) -> Result<(), String>` evaluating `self.permission.lock().await.store().evaluate(&ctx)` for `tool_name = "web_fetch"`, and implement `mehh_tools::web::UrlPolicyChecker` for `ToolInspectionManager`.

- [ ] **Step 4: Run targeted tests to verify pass**

Run: `cargo test -p mehh-coding-agent settings::tests` and `cargo test -p mehh-coding-agent permissions::tests` and `cargo test -p mehh-coding-agent tool_inspection::tests`
Expected: PASS.

---

### Task 8: Session, Subagent, TUI & CLI Wiring

**Files:**
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:148-170`
- Modify: `crates/mehh-coding-agent/src/interactive/events.rs:125-145`
- Modify: `crates/mehh-cli/src/main.rs:215-270,845-865,955-975`

**Interfaces:**
- Consumes: `Settings::to_web_config()`, `mehh_tools::{create_coding_tools_with_web, select_coding_tools_with_web}`, `impl UrlPolicyChecker for ToolInspectionManager`
- Produces:
  - `mehh_cli::resolve_cli_tools_with_web(args: &CliArgs, cwd: PathBuf, web: WebConfig) -> anyhow::Result<Vec<Arc<dyn AgentTool>>>` (existing `resolve_cli_tools` delegates with `WebConfig::default()`)

- [ ] **Step 1: Write failing tests in `agent_session.rs`, `interactive/tests.rs` (or `events.rs`), and `crates/mehh-cli/src/main.rs`**

- In `agent_session.rs`: `test_agent_session_applies_web_settings`: creating `AgentSession::new_with_trust` with `web.enabled = Some(false)` excludes `web_search`/`web_fetch`; default settings include both and pass them to child subagents.
- In `interactive`: `test_tool_execution_start_summarizes_web_fetch_url`: `ToolExecutionStart` for `"web_fetch"` with `{"url": "https://docs.rs/reqwest"}` sets `args_summary == "https://docs.rs/reqwest"`.
- In `mehh-cli/src/main.rs`: update any tool-count assertions if needed and add `test_resolve_cli_tools_with_web_respects_disabled_settings` proving `--tools` / default selection honors `WebConfig` from `settings.json` and `--exclude-tools web_search,web_fetch` excludes both cleanly.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mehh-coding-agent test_agent_session_applies_web_settings` and `cargo test -p mehh-coding-agent test_tool_execution_start_summarizes_web_fetch_url` and `cargo test -p mehh-cli resolve_cli_tools`
Expected: FAIL.

- [ ] **Step 3: Wire `agent_session.rs`, `interactive/events.rs`, and `mehh-cli/src/main.rs`**

- In `AgentSession::new_with_trust`, construct `inspection_manager` BEFORE building coding tools, then call `mehh_tools::create_coding_tools_with_web(cwd.clone(), settings.to_web_config().with_url_policy_checker(inspection_manager.clone()))` so both the session and `SubagentTool` inherit the configured `Arc<WebRuntime>` and `UrlPolicyChecker`.
- In `crates/mehh-coding-agent/src/interactive/events.rs:131`, extend the `args_summary` chain with `.or_else(|| args.get("url"))`.
- In `crates/mehh-cli/src/main.rs`, add `resolve_cli_tools_with_web` (keep `resolve_cli_tools` delegating with `WebConfig::default()`) and pass `agent_session.settings.to_web_config().with_url_policy_checker(agent_session.inspection_manager.clone())` in `main` (line 849) and `configure_cli_subagent` (line 250).

- [ ] **Step 4: Run final cross-crate verification tests**

Run:
- `cargo test -p mehh-ai`
- `cargo test -p mehh-mcp oauth`
- `cargo test -p mehh-tools`
- `cargo test -p mehh-coding-agent settings permissions tool_inspection interactive agent_session`
- `cargo test -p mehh-cli`
Expected: PASS across all 5 crates with zero warnings/errors.
