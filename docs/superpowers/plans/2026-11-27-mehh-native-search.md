# MehhNative Search Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Mehhcode's 100% MIT, in-process metasearch and bounded Auto-Enrich engine (`MehhNative` / `ProviderId::Mehh`) as priority #1 in `WebRouter`, replace `searxng-rs/` cleanly, and enforce deterministic quality-ranked failover.

**Architecture:** `MehhNative` lives in `crates/mehh-tools/src/web/native/` (`rank.rs`, `html_parse.rs`, `sources.rs`, `enrich.rs`, `mod.rs`), reusing `reqwest 0.12` (`rustls-tls`, `gzip`, `deflate`), `html_markdown` streaming HTML scanning, `mehh-ai::http_safety`, and `WebRouter`. `WebRouter::Auto` iterates `[Mehh, Exa, Parallel, Firecrawl, Tavily, Keenable, TinyFish]` in strict quality-ranked order with a `7s` per-attempt timeout and automatic priority recovery after cooldown.

**Tech Stack:** Rust, Tokio, `reqwest 0.12`, `serde_json`, `regex`, `base64 0.22`, `mehh-ai::http_safety`, `mehh-tools::web`.

**Spec:** [2026-11-26-mehh-native-search-design.md](file:///d:/Mehhcode/docs/superpowers/specs/2026-11-26-mehh-native-search-design.md)

## Global Constraints
- 100% MIT clean-room code; zero code copied from `searxng-rs`, and remove `d:\Mehhcode\searxng-rs/` plus all `ProviderId::Searxng` / `searxngUrl` references (fail-closed if config contains `"searxng"`).
- No new heavy crates (`scraper`, `axum`, `tera`, duplicate TLS/HTTP stack); only enable `gzip` and `deflate` on workspace `reqwest` in `Cargo.toml`.
- Stage 2 Auto-Enrich MUST ALWAYS enforce `DnsPolicy::PublicOnly` + `UrlPolicyChecker` (even if `WebConfig::allow_private_urls` is true), max `3` hops, `<= 512 KiB` per page, and `<= 2.5s` shared budget.
- Returning `0` hits when general web sources failed/blocked/timed out MUST return `ProviderError::VendorUnavailable` so `WebRouter` immediately fails over to `Exa`.
- Follow `AGENTS.md` + `ub-mode`: Windows PowerShell `;` chaining (no `&&`), no local `cargo build --release`, UTF-8 without BOM, pure domain logic separated from async I/O, TDD, do not commit unless requested.

## Review Focus
1. **Both general web sources blocked/CAPTCHA'd (`Ok(vec![])` trap):** Returning empty results would stall fallback to `Exa`; Stage 1 failure + 0 hits MUST return `VendorUnavailable` (pinned in `native::tests::test_search_mehh_fails_over_when_web_sources_captcha_or_fail`).
2. **Untrusted search hit pointing to localhost/LAN or private redirect:** Auto-Enrich MUST enforce `DnsPolicy::PublicOnly` + `UrlPolicyChecker` per redirect hop even when `allow_private_urls: true` (pinned in `enrich::tests::test_enrich_enforces_url_policy_and_dns_policy_fail_closed` and `web::tests::test_try_new_enrich_blocks_loopback_ssrf`).
3. **StackExchange / target server returning gzip-compressed responses:** `reqwest` `gzip` feature must decode compressed JSON/HTML transparently (pinned by real gzip fixture test in `sources::tests`).
4. **Slow destination server during Auto-Enrich stalling `web_search`:** Stage 2 shared `2.5s` deadline must cancel stragglers and keep original SERP snippets (pinned in `enrich::tests::test_enrich_upgrades_snippets_and_skips_binary_and_spa`).
5. **UTF-8 multi-byte / Vietnamese text & DuckDuckGo/Bing redirect unwrapping:** Truncation, HTML entity decoding, DDG `/l/?uddg=`, and Bing `/ck/a?...&u=a1...` base64url unwrapping must never panic or corrupt UTF-8 (pinned in `rank::tests` and `html_parse::tests`).

---

### Task 1: Workspace Dependencies, Cleanup of `searxng-rs` & `ProviderId::Searxng`, Add `ProviderId::Mehh`
- **Files:** `Cargo.toml`, delete `searxng-rs/`, `crates/mehh-tools/src/web/types.rs`, `crates/mehh-coding-agent/src/settings.rs`
- [ ] Enable `gzip` and `deflate` on `reqwest` in `Cargo.toml`
- [ ] Delete `d:\Mehhcode\searxng-rs`
- [ ] Update `ProviderId` (`Mehh` as `ALL_SEARCH[0]`, `"mehh"`, `"Mehh Search"`) and `WebProviderSetting` (`Mehh`, fail-closed on `"searxng"`, remove `searxng_url`)

### Task 2: Pure Domain Core (`crates/mehh-tools/src/web/native/rank.rs`)
- **Files:** `crates/mehh-tools/src/web/native/rank.rs`
- [ ] Define `SourceId`, `RawHit`, `classify_query`, `canonicalize_url`, `fuse_and_rank` (RRF `k = 60.0`), `extract_relevant_passages`
- [ ] Write & pass unit tests for routing, canonicalization, RRF consensus fusion, and Unicode-safe passage extraction

### Task 3: Zero-Dep SERP HTML Parser (`html_markdown.rs` + `native/html_parse.rs`)
- **Files:** `crates/mehh-tools/src/web/html_markdown.rs`, `crates/mehh-tools/src/web/native/html_parse.rs`
- [ ] Expose `pub(crate)` `HtmlSink`, `scan_html`, `find_attr_decoded`, `decode_entities`
- [ ] Implement `unwrap_ddg_url`, `unwrap_bing_url`, `looks_like_captcha`, `parse_ddg_html`, `parse_bing_html`
- [ ] Write & pass unit tests with realistic DDG/Bing HTML fixtures and redirect/CAPTCHA cases

### Task 4: Multi-Source Async Adapters (`crates/mehh-tools/src/web/native/sources.rs`)
- **Files:** `crates/mehh-tools/src/web/native/sources.rs`
- [ ] Implement `NativeEndpoints`, `SourceOutcome`, and `fetch_source` across all 7 sources (`DuckDuckGo`, `Bing`, `Wikipedia`, `GitHub`, `CratesIo`, `Npm`, `StackOverflow`)
- [ ] Write & pass mock HTTP tests covering all 7 sources, real gzip decoding, CAPTCHA handling, and 4xx/5xx status classification

### Task 5: Bounded Parallel Auto-Enrich (`enrich.rs`) & Orchestrator (`native/mod.rs`)
- **Files:** `crates/mehh-tools/src/web/native/enrich.rs`, `crates/mehh-tools/src/web/native/mod.rs`
- [ ] Implement `is_enrich_candidate`, `enrich_top_hits`, and `search_mehh`
- [ ] Write & pass unit + mock integration tests for enrichment budgets, binary/SPA skipping, strict SSRF/policy enforcement, and Stage 1 failover triggers

### Task 6: Wire `MehhNative` into `providers.rs`, `router.rs`, and `mod.rs`
- **Files:** `crates/mehh-tools/src/web/providers.rs`, `crates/mehh-tools/src/web/router.rs`, `crates/mehh-tools/src/web/mod.rs`
- [ ] Wire `NativeEndpoints` into `WebEndpoints` and `ProviderId::Mehh` into `execute_search` / `HttpProviderExecutor`
- [ ] Replace random `search_sticky` / `extract_sticky` with deterministic priority order and `PER_ATTEMPT_TIMEOUT = 7s`
- [ ] Enforce `DnsPolicy::PublicOnly` + `UrlPolicyChecker` in `WebRuntime::try_new` for Auto-Enrich; update & pass router/runtime tests

### Task 7: Verification & Hygiene Check
- [ ] Run `cargo test -p mehh-tools`, `cargo test -p mehh-coding-agent settings::tests::`, `cargo check -p mehh-coding-agent -p mehh-cli`, and `cargo clippy -p mehh-tools --no-deps --all-targets -- -D warnings`
- [ ] Verify `searxng-rs/` is removed and check `git diff --stat`
