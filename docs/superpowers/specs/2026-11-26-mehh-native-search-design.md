# MehhNative Search Engine Design

**Date:** 2026-11-26  
**Status:** Ready for Approval (Builder Check Completed)  
**Target:** `Cargo.toml`, `crates/mehh-tools`, and `crates/mehh-coding-agent`

---

## 1. Intent & Why

Mehhcode needs its own first-party, 100% MIT-licensed, in-process web search technology (`MehhNative`) compiled directly into `mehh.exe`, replacing the cloned AGPL-3.0 `searxng-rs/` codebase.

Because each Mehhcode user runs locally on their own machine and IP address, an in-process metasearch engine avoids:
- External daemon/port/MCP setup and process lifecycle overhead.
- AGPL-3.0 licensing conflicts with Mehhcode's MIT codebase.
- Shared-IP rate limits from centralized third-party proxies.
- Heavy redundant dependencies (`scraper`, `axum`, `tera`, duplicate `reqwest 0.11` / `native-tls`).

At the same time, `WebRouter::Auto` currently picks its initial search provider randomly via UUID seed and leaves `PER_ATTEMPT_TIMEOUT` at 12 seconds. Replacing random selection with deterministic quality-ranked priority and fast failover ensures `MehhNative` runs first, fails over within seconds if blocked or throttled, and automatically recovers when cooldown expires.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
1. **`ProviderId::Mehh` (`"mehh"`, display name `"Mehh Search"`) at Priority #1** in `WebProviderChoice::Auto`, with deterministic quality-ranked failover order:
   `Mehh` -> `Exa` -> `Parallel` -> `Firecrawl` -> `Tavily` -> `Keenable` -> `TinyFish`.
2. **Minimal, justified dependency footprint:** Reuse workspace `reqwest 0.12` (`rustls-tls`), `tokio`, `serde_json`, `regex`, `mehh-ai::http_safety` (`SafeDnsResolver`, URL/redirect/IP validation), and `mehh-tools::web::html_markdown`. **Approved `Cargo.toml` change:** enable `gzip` and `deflate` features on workspace `reqwest` (which pulls `flate2`/decompression) so the StackExchange API (unconditionally gzip-compressed) and compressed HTML pages in Stage 2 Auto-Enrich decode transparently; `reqwest` explicitly rejects `br`/`zstd` overhead by omitting them and only requesting `Accept-Encoding: gzip, deflate, identity`.
3. **Parallel multi-source metasearch:**
   - General web HTML sources: `DuckDuckGo` HTML + `Bing` HTML.
   - Official JSON developer & reference APIs: `Wikipedia`, `GitHub`, `crates.io`, `npm registry`, and `StackOverflow`, selectively activated by query classification alongside general web sources.
4. **Pure-function URL canonicalization & RRF fusion:** Strip tracking parameters (`utm_*`, `fbclid`, `gclid`, `ref`, etc.), normalize scheme/host (`www.`)/trailing slash/fragment, and merge duplicate hits across sources using Reciprocal Rank Fusion:
   `score = sum(source_weight / (60.0 + rank))`.
5. **Bounded Parallel Auto-Enrich:** Concurrently fetch up to the top 3 ranked HTTP/HTTPS URLs under a shared `2.5s` budget (`512 KiB` body cap per page, max 3 redirects, strict `DnsPolicy::PublicOnly` + `UrlPolicyChecker`), render HTML via `html_markdown::html_to_markdown`, extract the highest-scoring query-relevant passages (`<= 900` chars/hit), and fall back gracefully to the original SERP snippet on any timeout, SPA shell, or non-HTML/error response.
6. **Fast failover & cooldown recovery:** Reduce `WebRouter` `PER_ATTEMPT_TIMEOUT` from `12s` to `7s`. Use strict priority order instead of sticky random selection so once `#1` (`Mehh`) exits its cooldown window, subsequent queries return to `#1` automatically.
7. **Complete removal of `searxng-rs` integration (User Confirmed):** Delete `d:\Mehhcode\searxng-rs/` and cleanly remove `ProviderId::Searxng`, `WebEndpoints::searxng_url`, `searxng_client`, `WebProviderSetting::Searxng`, `WebSettings::searxng_url`, and `MEHH_SEARXNG_URL`. Strict fail-closed config validation applies (`"provider": "searxng"` in `settings.json` fails JSON parse cleanly with an error rather than silently remapping).

### Non-Goals
- No background web crawler or persistent local search index.
- No standalone HTTP server daemon, web UI, or standalone `--mcp` binary.
- No headless browser automation or CAPTCHA solving/bypassing.
- No specialized image, video, shopping, or biomedical search verticals.

### Anti-Goals
- **Silent empty success masking failure:** Returning `Ok(vec![])` when DuckDuckGo & Bing fail, time out, or serve CAPTCHAs MUST NOT happen; `Mehh` must return `ProviderError::VendorUnavailable` so `WebRouter` immediately fails over to `Exa`.
- **Weakening SSRF / policy boundaries during Auto-Enrich:** Search result URLs discovered dynamically from the web are untrusted input. Auto-Enrich MUST ALWAYS enforce `DnsPolicy::PublicOnly` and `UrlPolicyChecker` (if configured), regardless of `WebConfig::allow_private_urls`.
- **Adding bloat crates:** Do NOT add `scraper`, `html5ever`, `axum`, `tera`, or a second `reqwest`/TLS stack.
- **Latency tail blowup:** Never stall `web_search` past the shared `2.5s` Stage 2 enrich deadline waiting on a slow destination domain.

---

## 3. Constraints

- **License:** 100% MIT clean-room implementation. Zero code copied from `searxng-rs`.
- **Tool Contract:** Preserve `WebSearchTool` (`name = "web_search"`, `query` parameter, `format_search_results` Markdown output, `details` JSON containing `provider`, `resultCount`, `cached`) so LLM prompts and TUI consumers require zero breaking changes.
- **Development Discipline (`AGENTS.md` + `ub-mode`):**
  - Keep pure domain logic (query classification, HTML SERP token extraction, URL canonicalization, RRF ranking, passage extraction) cleanly separated from the async `reqwest`/Tokio I/O shell.
  - Validate untrusted external inputs (URLs, redirects, JSON, HTML) once at boundaries; enforce bounded byte reads and UTF-8 char-boundary truncation.
  - Cross-platform support (Windows, Linux, macOS).
  - Verify locally with offline deterministic unit tests and `TcpListener` HTTP mocks (no live network dependencies in tests).

---

## 4. Approaches Considered

| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **1. Ranked `Exa -> Parallel -> MehhNative`** | Put Exa/Parallel first; `MehhNative` runs without Auto-Enrich as #3 self-hosted fallback. | Lowest latency when Exa works, clean core, but depends on third-party AI vendors for #1 traffic. | Rejected |
| **2. `MehhNative + Auto-Enrich` at #1** | Build `MehhNative` with RRF + bounded top-3 Auto-Enrich as priority #1, followed by quality-ranked remote fallbacks (`Exa -> Parallel -> ...`). | First-party search runs by default for every user/IP with deep page context comparable to Exa; adds ~2.5s cap for Stage 2 enrich and HTML DOM maintenance for DDG/Bing. | **Selected by user** |
| **3. `MehhNative` Only (Drop Remote Vendors)** | Remove Exa, Parallel, Firecrawl, Tavily, TinyFish, Keenable entirely. | Minimal code, zero third-party endpoints, but leaves zero fallback if a user's network/IP blocks DDG and Bing. | Rejected |

---

## 5. Architecture & Concrete Decisions

### 5.1 Module Placement (Architectural Call)
Implement `MehhNative` directly inside `crates/mehh-tools/src/web/native/` rather than creating an extra `crates/mehh-search` crate:
- **Reason:** Directly shares `html_markdown` (MIT zero-dep streaming HTML scanner), `mcp_rpc` bounded HTTP helpers (`read_bounded_body`, `send_cancellable_request`), `SearchHit`, `ProviderError`, `UrlPolicyChecker`, and `WebEndpoints` without extra workspace boilerplate or cyclic dependency workarounds (`mehh-tools` would otherwise have to depend on `mehh-search` while duplicating HTML/HTTP safety utilities).
- **Layout:**
  - `crates/mehh-tools/src/web/native/mod.rs` — Orchestration (`search_mehh`) coordinating Stage 1 metasearch, URL fusion/ranking, and Stage 2 Auto-Enrich.
  - `crates/mehh-tools/src/web/native/sources.rs` — Source requests, response parsing wrappers, redirect unwrapping (`DDG /l/?uddg=`, `Bing /ck/a?...&u=a1...`), and CAPTCHA/rate-limit detection.
  - `crates/mehh-tools/src/web/native/html_parse.rs` — Clean-room state-machine SERP extractors for DuckDuckGo HTML and Bing HTML (no `scraper` crate).
  - `crates/mehh-tools/src/web/native/rank.rs` — Query classifier (`QueryProfile`), URL canonicalizer, RRF ranker (`fuse_and_rank`), and BM25/keyword passage extractor (`extract_relevant_passages`).
  - `crates/mehh-tools/src/web/native/enrich.rs` — Bounded concurrent Stage 2 page fetcher & Markdown passage enricher.

### 5.2 Pure Domain Core (`rank.rs` & `html_parse.rs`)
- **`QueryProfile` & Source Selection:**
  - `QueryProfile` classifies normalized query tokens into optional specialist intents:
    - `rust_pkg` (`cargo`, `crate`, `crates.io`, `rust`, `tokio`, `serde`, etc.) -> activates `CratesIo`
    - `js_pkg` (`npm`, `node`, `typescript`, `react`, `vite`, etc.) -> activates `Npm`
    - `code_repo` (`github`, `repo`, `library`, `sdk`, `open source`, `issue`, etc.) -> activates `GitHub`
    - `qa` (`error`, `exception`, `how to`, `why does`, `fix`, stack-trace signals) -> activates `StackOverflow`
    - `reference` (definitional/factual queries, `"what is"`, `"who"`, `"history"`, etc.) -> activates `Wikipedia`
  - `DuckDuckGo` and `Bing` ALWAYS run for every query. Specialist sources are capped so total concurrent Stage 1 requests stay bounded (2 general + up to 3 specialist/reference sources = max 5 requests).
- **Sources & Base RRF Weights (`SourceId`):**
  - `DuckDuckGo` (`1.0`), `Bing` (`1.0`), `Wikipedia` (`0.85`), `GitHub` (`0.95` when code_repo), `CratesIo` (`1.0` when rust_pkg), `Npm` (`1.0` when js_pkg), `StackOverflow` (`0.95` when qa).
- **URL Canonicalization:**
  - Parse via `reqwest::Url`; accept only `http`/`https`.
  - Lowercase host, strip leading `www.`, map `http` vs `https` canonically for dedup key (preserve highest-ranked HTTPS URL as display URL), drop `#fragment`, strip trailing `/` on non-root paths, remove tracking query parameters (`utm_*`, `fbclid`, `gclid`, `mc_cid`, `mc_eid`, `ref`, `igshid`), and sort remaining query params.
- **RRF Fusion (`k = 60.0`):**
  - Merge `RawHit` items sharing a canonical URL: sum `weight / (60.0 + rank as f64)` across distinct sources, retain the best non-empty title, longest clean snippet, and published date, then sort descending by score (tie-break by best single-source rank, then canonical URL for determinism) and take top `10`.
- **Passage Extraction (`extract_relevant_passages(query, markdown, max_chars = 900)`):**
  - Tokenize `query` (lowercased alphanumeric tokens, dropping tiny stopwords).
  - Split Markdown into paragraph/section blocks preserving headings.
  - Score each block by unique query-term coverage, term frequency (diminishing saturation), and heading/code relevance boost while penalizing boilerplate/nav-link-heavy lines.
  - Greedily pick top-scoring blocks within `max_chars`, emit them in original document order separated by `\n\n...\n\n`, bounded safely at UTF-8 char boundaries.

### 5.3 Execution Pipeline & Hard Timeouts (`mod.rs`, `sources.rs`, `enrich.rs`)
- **Stage 1: Parallel Metasearch (`<= 3.5s` source budget):**
  - Spawn source tasks into a `JoinSet`, each clamped to `remaining.min(3.5s)` and `512 KiB` response limit.
  - Detect CAPTCHA/block markers (`g-recaptcha`, `cf-challenge`, `anomaly-modal`, `unusual traffic`) or HTTP `403`/`429`/`5xx` per source without crashing healthy sources.
  - **Failover rule:** If `0` total hits remain AND at least one general web source failed/blocked/timed out -> return `Err(ProviderError::VendorUnavailable { .. })` (triggers `WebRouter` cooldown & immediate failover to `Exa`).
- **Stage 2: Bounded Auto-Enrich (`<= 2.5s` shared budget):**
  - Skip Stage 2 if caller `remaining` budget after Stage 1 is `< 600ms`.
  - Select up to **3** top-ranked hits whose URL has `http`/`https`, is not a binary/media extension (`.pdf`, `.zip`, `.tar`, `.gz`, `.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, `.svg`, `.mp3`, `.mp4`, `.exe`, `.wasm`), and whose SERP snippet is `< 500` chars (skip Wikipedia/JSON API entries that already have rich summaries if snippet >= 500 chars).
  - Fetch concurrently via a dedicated `PublicOnly` `reqwest::Client` (with `redirect::Policy::none()` + manual redirect validation up to 3 hops checking `check_url_host_policy` + `UrlPolicyChecker`, matching `fetch_tool.rs` fail-closed guarantees).
  - Cap each response to `512 KiB`; accept only HTML/Markdown/plain text MIME types; ignore/skip `looks_like_empty_spa_shell`; convert HTML via `html_to_markdown`; extract passages; upgrade `hit.content` only if extracted text is non-empty and longer/more informative than the existing snippet.
  - Per-URL timeout, redirect block, policy rejection, or body error silently retains the original SERP hit without failing the whole search.

### 5.4 `WebRouter` Deterministic Priority & Failover (`router.rs`)
- Replace random `search_sticky` initialization and sticky fallback sticking:
  - `ProviderId::ALL_SEARCH = [Mehh, Exa, Parallel, Firecrawl, Tavily, Keenable, TinyFish]`.
  - In `WebProviderChoice::Auto`, iterate `ProviderId::ALL_SEARCH` in strict priority order, skipping providers currently in `cooldowns`. If every provider is in cooldown, pick the single earliest-expiring provider for one probe attempt.
  - On cooldown expiry, subsequent `Auto` searches naturally try `Mehh` (#1) again because priority order is evaluated on each fresh `Auto` search (no stale `search_sticky` pinning traffic permanently onto a lower-tier fallback).
  - Update `ALL_EXTRACT = [Exa, Parallel, Firecrawl, Keenable]` to also use deterministic priority order for consistency.
  - Reduce `PER_ATTEMPT_TIMEOUT` from `12s` to `7s` (`Mehh` Stage 1 `3.5s` + Stage 2 `2.5s` fits cleanly under `7s`, while slow external endpoints trigger failover nearly twice as fast).

### 5.5 Endpoints, Policy & Settings Integration (`mod.rs`, `providers.rs`, `settings.rs`)
- **`WebRuntime` Clients:**
  - `router_client` stays strictly `DnsPolicy::PublicOnly`.
  - Pass `url_policy_checker: Option<Arc<dyn UrlPolicyChecker>>` into `WebRouter` / `HttpProviderExecutor` so Stage 2 Auto-Enrich enforces user/workspace URL policy rules before connecting to any search-result host or redirect hop.
- **`WebEndpoints`:**
  - Remove `searxng_url: Option<String>`.
  - Add overrideable endpoint fields for native sources (`ddg_html`, `bing_html`, `wikipedia_api`, `github_api`, `crates_api`, `npm_api`, `stackoverflow_api`) defaulting to official production URLs so all tests run deterministically against local `TcpListener` mocks.
- **`Settings` (`crates/mehh-coding-agent/src/settings.rs`):**
  - Add `WebProviderSetting::Mehh` (`"mehh"`).
  - Remove `WebProviderSetting::Searxng`, `WebSettings::searxng_url`, `MEHH_SEARXNG_URL`, and `d:\Mehhcode\searxng-rs/`.

---

## 6. Architectural Calls Made (For Final Check)

Calls made directly in this design (confirmed unless you want to override):
1. Placed `MehhNative` inside `crates/mehh-tools/src/web/native/` rather than adding a new workspace crate, to share `html_markdown`, `http_safety`, `mcp_rpc` bounded readers, and `WebRouter` directly with zero duplication.
2. Enforced `DnsPolicy::PublicOnly` unconditionally for Stage 2 Auto-Enrich (never fetching loopback/LAN search hits automatically even when `allow_private_urls: true`).
3. Set `WebRouter::PER_ATTEMPT_TIMEOUT = 7s`, Stage 1 per-source budget `3.5s`, Stage 2 shared Auto-Enrich budget `2.5s` (top 3 hits, `512 KiB`/page, max 3 redirects), and RRF constant `k = 60.0` returning top `10` hits.
4. Enabled `gzip` + `deflate` features on workspace `reqwest` (confirmed by user) and excluded `scraper`, `axum`, and `tera`.
5. Removed `d:\Mehhcode\searxng-rs/` and all `ProviderId::Searxng` / `searxngUrl` wiring with strict fail-closed rejection on stale `"searxng"` config (confirmed by user).

## 7. What Is Left to the Builder

- Tuning `QueryProfile` keyword lists and BM25/passage-scoring weights with unit test cases.
- Implementing clean-room HTML token scanning in `html_parse.rs` for DuckDuckGo and Bing HTML SERP responses, backed by realistic HTML fixture unit tests.
- Choosing concise status strings for `on_progress` updates (`"Searching via Mehh Search..."`, `"Enriching top results..."`, etc.).
- Updating existing `router.rs` tests that asserted old `search_sticky` behavior to match deterministic priority + cooldown recovery semantics.
