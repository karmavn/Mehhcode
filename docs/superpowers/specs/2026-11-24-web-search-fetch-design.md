# Web Search & Web Fetch Design for Mehhcode

**Date:** 2026-11-24
**Status:** Draft (pending user approval)
**Sizing:** Medium multi-crate project (`mehh-ai`, `mehh-mcp`, `mehh-tools`, `mehh-coding-agent`, `mehh-cli`)
**Selected Approach:** Approach 1 — Lean Keyless Core + Hybrid Local-First Fetch

---

## 1. Intent & Why

Mehhcode currently ships 10 local built-in tools (`read`, `write`, `edit`, `undo_edit`, `bash`, `grep`, `find`, `ls`, `analyze`, `powershell`) and no built-in web tools. Users need the agent to look up up-to-date documentation, APIs, and web pages out of the box without configuring API keys or external MCP servers.

The user wants the experience to feel as smooth as OpenCode (install and use immediately, concise model-friendly schemas, fast local HTML-to-Markdown fetch, automatic rotation across free providers), while fitting Mehhcode's own architecture natively (`ToolInspectionManager`, `PermissionStore`, `Settings`, `SafeDnsResolver`, TUI tool cards) and keeping the implementation minimal and low-dependency (`/ub-mode`).

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
- Provide built-in `web_search` and `web_fetch` tools in `mehh-tools`, available by default in interactive TUI, print/headless (`-p`), RPC, and read-only subagents (`research`/`review`).
- Run `web_search` with zero configuration across 6 verified keyless HTTP/JSON-RPC endpoints (Exa, Parallel, Firecrawl, Tavily keyless, TinyFish keyless, Keenable) using session-sticky routing, cooldown tracking, and automatic failover.
- Run `web_fetch` local-first over direct HTTP with local HTML-to-Markdown conversion, automatically falling back to the 4 extract-capable keyless providers (Exa, Parallel, Firecrawl, Keenable) when a public page is blocked (403/WAF) or renders an empty JS SPA shell.
- Enforce Mehhcode's security invariants fail-closed: connect-time SSRF protection shared with `mehh-mcp`, always-blocked cloud metadata floor, redirect-hop policy checks, secret-in-URL exfiltration guard before remote fallback, and project-settings escalation guard.
- Keep context usage bounded via response byte caps, HTML parser depth caps, in-memory search memoization with single-flight coalescing, and head+tail truncation with temp-file spill readable via `read`.

### Non-Goals (Deferred from v1)
- BYOK API keys (`EXA_API_KEY`, `BRAVE_API_KEY`, `PERPLEXITY_API_KEY`, etc.) and self-hosted search backends (`SEARXNG_URL`) — user explicitly selected Approach 1 (keyless core only).
- Native LLM-hosted web search protocols in `mehh-ai` (Anthropic `web_search_20250305`, OpenAI `web_search_preview`, Google Search grounding).
- Multi-URL batch `web_fetch` (`urls: Vec<String>`) and persistent disk cache (`~/.mehh/cache/web`).
- OpenCode-style interactive first-use consent modal dialog (conflicts with Mehhcode's native `PermissionMode` model and headless `-p` workflows).
- Binary/PDF/raster-image fetching or OCR (rejected cleanly by MIME gate).

### Anti-Goals
- Silently failing over `web_search` to another vendor when the user explicitly pinned a specific provider in `settings.json` (pinned providers apply strictly to `web_search`; `web_fetch` Stage 2 extract fallback still rotates across extract-capable keyless providers unless `provider: "disabled"` or `fetchFallback: false`).
- Sending localhost/private URLs or URLs containing secrets/credentials/tokens to third-party extract endpoints.
- Adding heavy external crates (`html5ever`, `scraper`, browser automation) that bloat CI build times.
- Letting a repository `.mehh/settings.json` loosen global security settings (e.g., turning on `allowPrivateUrls: true` or re-enabling web tools disabled globally).
- Blocking non-interactive CLI invocations (`mehh -p`) with interactive UI prompts.

---

## 3. Constraints

- **Zero new external crates outside root `Cargo.toml`**: reuse existing workspace crates (`reqwest` with `rustls-tls`/`stream`/`json`, `futures`, `tokio`, `tokio-util`, `serde`, `serde_json`, `regex`, `uuid`, `anyhow`, `async-trait`, `tracing`). Note: `reqwest` has `default-features = false` and does **not** enable `gzip`/`brotli`, so requests must not manually advertise `Accept-Encoding: gzip, br` (let `reqwest` request uncompressed responses).
- **Cross-platform**: identical behavior on Windows, macOS, and Linux; UTF-8 without BOM for all written files.
- **Cancellation & timeouts**: every HTTP call and streaming body read must observe `CancellationToken` via `tokio::select!` alongside per-request timeouts so Ctrl+C / subagent cancellation aborts immediately.
- **Fail-closed security & config**: distinguish `ENOENT` from invalid JSON (strip BOM); never silently drop permission rules or fall back to an insecure HTTP client if the hardened client fails to build.
- **Offline CI verification**: all unit and integration tests run against local `tokio::net::TcpListener` mock servers and fixture payloads — zero live internet calls in tests.

---

## 4. Approaches Considered

1. **Approach 1 — Lean Keyless Core + Hybrid Fetch (Selected)**: Move `SafeDnsResolver` into `mehh-ai`; implement `web_search` and `web_fetch` in `mehh-tools` with the 6 keyless providers, sticky failover router, in-memory memo, zero-dep HTML-to-Markdown, and automatic keyless extract fallback for public blocked/SPA pages. Minimal code and zero new external dependencies. Trade-off: no built-in SearXNG/BYOK escape hatch in v1.
2. **Approach 2 — Unified Keyless + Self-Hosted / BYOK (Not Selected)**: Same core as Approach 1 plus optional SearXNG, Brave, Perplexity, and keyed modes for the 6 providers via env/`settings.json`. Trade-off: additional config/env precedence complexity and extra thin adapters.
3. **Approach 3 — Full Suite with Native LLM Search & Disk Cache (Not Selected)**: Approach 2 plus native Anthropic/OpenAI/Google hosted search streaming protocols, persistent disk cache, and batch multi-URL `web_fetch`. Trade-off: deep `mehh-ai` wire-protocol changes and oversized v1 scope.

The user selected **Approach 1** to keep v1 lean and focused.

---

## 5. Architecture & Component Design

Decisions explicitly settled with the user are stated directly; implementation defaults filled in to make the spec self-contained are marked `*(Default call)*`.

### 5.1 Shared Network & SSRF Safety (`crates/mehh-ai/src/http_safety.rs`)

Consolidate SSRF protection in `mehh-ai` (already depended on by `mehh-mcp`, `mehh-tools`, and `mehh-coding-agent`) so `mehh-mcp` and `mehh-tools` share one source of truth.

- **Move from `crates/mehh-mcp/src/oauth.rs`**: `SafeDnsResolver`, `is_private_or_local_ip`, `is_local_or_internal_hostname`, `is_loopback_host`. Update `crates/mehh-mcp/src/oauth.rs` and `crates/mehh-mcp/src/transport.rs` to import from `mehh_ai::http_safety` (preserving existing OAuth loopback discovery exception in `oauth.rs`).
- **Always-Blocked Cloud Metadata Floor** *(Default call)*:
  - Always blocked regardless of config: hostnames `metadata.google.internal`, `metadata.goog` (case-insensitive, stripping trailing dots); IPv4 `169.254.0.0/16` (covers AWS/GCP/Azure `169.254.169.254`, ECS `169.254.170.2`, Azure IMDS `169.254.169.253`) and `100.100.100.200` (Alibaba metadata); IPv6 `fd00:ec2::254` (AWS IPv6 metadata); plus IPv4-mapped (`::ffff:0:0/96`), IPv4-compatible (`::/96`), IPv4-translated (`::ffff:0:0:0/96`), and NAT64 (`64:ff9b::/96`) forms of blocked IPv4 ranges.
  - Also block IPv6 special-use ranges (`100::/64`, `2001:2::/48`, `2001:20::/28`) alongside existing ULA/link-local/site-local/6to4/Teredo/CGNAT checks.
- **`DnsPolicy` enum**:
  - `DnsPolicy::PublicOnly` (default): blocks private/loopback/local/reserved/special-use IPs and local hostnames + always-blocked metadata floor.
  - `DnsPolicy::AllowPrivate`: permits loopback and RFC 1918/ULA private ranges (for local dev servers when `web.allowPrivateUrls = true`), while still enforcing the always-blocked cloud metadata floor.
- **`SafeDnsResolver::new(policy: DnsPolicy)`**: implements `reqwest::dns::Resolve`, checking resolved IPs at connect time (prevents DNS-rebinding TOCTOU) and returning an error if any/all resolved addresses violate policy.
- **`build_safe_http_client(policy: DnsPolicy, connect_timeout: Duration, request_timeout: Duration) -> anyhow::Result<reqwest::Client>`**: configures `redirect::Policy::none()` and `dns_resolver(Arc::new(SafeDnsResolver::new(policy)))`. Fails closed if client construction fails.
- **URL Validation & Exfiltration Guard**:
  - `normalize_and_validate_http_url(raw: &str) -> Result<reqwest::Url, String>`: trims whitespace, repairs whitespace immediately after `://` (e.g., `https:// docs.rs` → `https://docs.rs`), enforces `http`/`https` scheme only, rejects embedded credentials (`username` or `password` non-empty), and strips `#fragment`.
  - `is_eligible_for_remote_extract(url: &reqwest::Url) -> Result<(), &'static str>` *(Default call)*:
    1. Rejects any local/internal hostname or private/loopback/reserved IP literal (even when `allowPrivateUrls = true`, private targets are never sent to external vendors).
    2. Rejects URLs whose query parameter keys match sensitive names (`token`, `key`, `api_key`, `apikey`, `secret`, `password`, `passwd`, `auth`, `access_token`, `refresh_token`, `sig`, `signature`, `x-amz-signature`, `x-amz-credential`, `session`, `session_id`, `jwt`, `bearer`) or whose query/path values contain recognizable token prefixes (`sk-`, `ghp_`, `gho_`, `AKIA`, `eyJ`).

### 5.2 Domain Model & Module Layout (`crates/mehh-tools/src/web/`)

Add `reqwest = { workspace = true }`, `futures = { workspace = true }`, `uuid = { workspace = true }`, `tracing = { workspace = true }` to `crates/mehh-tools/Cargo.toml` (all already in workspace `Cargo.toml`).

Module breakdown:
- `mod.rs`: `WebConfig`, `WebProviderChoice`, `WebRuntime` (shared via `Arc` between `WebSearchTool`, `WebFetchTool`, and inherited subagents), exports.
- `types.rs`:
  - `ProviderId` closed enum: `Exa`, `Parallel`, `Firecrawl`, `Tavily`, `TinyFish`, `Keenable` (`ProviderId::ALL_SEARCH`, `ProviderId::ALL_EXTRACT = [Exa, Parallel, Firecrawl, Keenable]`).
  - `WebProviderChoice` enum: `Auto`, `Fixed(ProviderId)`, `Disabled`.
  - `SearchHit { url: String, title: Option<String>, published: Option<String>, content: Option<String> }`.
  - `ExtractedPage { url: String, final_url: Option<String>, title: Option<String>, content: String }`.
  - `ProviderError` enum separating failover-eligible vendor failures from terminal/origin failures:
    - `RateLimited { retry_after: Option<Duration>, detail: String }` → failover + cooldown (`Retry-After` clamped to `5s..=300s`, default `60s`).
    - `VendorUnavailable { status: Option<u16>, detail: String }` → failover + cooldown (`300s` for HTTP 401/402/403; `30s` for 5xx/transport/timeout/parse errors).
    - `OriginBlocked { status: u16, detail: String }` → target site refused the request (e.g., Exa `CRAWL_HTTP_429` or Keenable relayed origin error); do **not** cool down or rotate vendor.
    - `BadRequest(String)` → terminal error (HTTP 400); do **not** rotate.
    - `Cancelled` → user/session cancelled; stop immediately.
- `mcp_rpc.rs`:
  - Shared JSON-RPC 2.0 `tools/call` client over `reqwest` (`Accept: application/json, text/event-stream`).
  - Bounded response reading (`MAX_PROVIDER_RESPONSE_BYTES = 512 * 1024`), UTF-8 decoding.
  - Parses either direct JSON object body or SSE `data: {...}` lines split strictly on `\r\n`, `\r`, or `\n` (not Unicode line separators that can appear inside CJK text).
  - Surfaces JSON-RPC `error` and MCP `result.isError` cleanly; classifies rate-limit messages (`429`, `rate limit`, `too many requests`, `quota exceeded`, `slow down`) into `ProviderError::RateLimited`.
- `providers.rs`:
  - All 6 keyless endpoints use honest `User-Agent: mehh-code/0.1` and send no user, machine, workspace, or LLM model identifiers.
  - Endpoint map is configurable on `WebRuntime` (defaulting to production constants) so unit tests inject local mock server URLs cleanly.
  - Exact wire protocols:
    1. **Exa** (`https://mcp.exa.ai/mcp`):
       - Search: MCP `web_search_exa` `{ query, numResults: 8 }`. Parses `---`-separated blocks (`Title:`, `URL:`, `Published:`, `Highlights:` / `Text:`).
       - Extract: MCP `web_fetch_exa` `{ urls: [url] }`. Detects `CRAWL_HTTP_<status>` or `CRAWL_NOT_FOUND` (status 404) at the end of error text as `OriginBlocked`.
    2. **Parallel** (`https://search.parallel.ai/mcp`):
       - Search: MCP `web_search` `{ objective: query, search_queries: [query], session_id: <random per-runtime hex> }`. Reads `result.structuredContent.results[]` (falling back to JSON-parsed `result.content[].text`).
       - Extract: MCP `web_fetch` `{ urls: [url], objective: "Full page content", session_id }`. Reads `results[0]` (`full_content` -> `content` -> joined `excerpts`); maps `errors[0].http_status_code` to `OriginBlocked`.
    3. **Firecrawl**:
       - Search: MCP `firecrawl_search` `{ query, limit: 8 }` at `https://mcp.firecrawl.dev/v2/mcp`. Parses JSON text `data.web[]` (`url`, `title`, `description`).
       - Extract: REST `POST https://api.firecrawl.dev/v2/scrape` (no `Authorization` header), body `{ url, formats: ["markdown"] }`. Reads `data.markdown` (fallback `data.html`), `data.metadata.title`, and `data.metadata.url` (not `sourceURL`, which echoes the requested URL) as post-redirect `final_url`; maps `data.metadata.statusCode` / `status_code >= 400` to `OriginBlocked`.
    4. **Tavily** (`https://api.tavily.com/search`, search only):
       - REST `POST` with headers `X-Tavily-Access-Mode: keyless`, `X-Client-Name: mehh-code`, body `{ query, search_depth: "basic", chunks_per_source: 3, max_results: 8 }`. Parses `results[]` (`url`, `title`, `content`).
    5. **TinyFish** (`https://agent.tinyfish.ai/mcp`, search only):
       - MCP `search` `{ query }` with header `X-TinyFish-Access-Mode: keyless` (no `initialize` / `Mcp-Session-Id` handshake required in keyless mode). Parses JSON text `results[]` (`url`, `title`, `snippet`).
    6. **Keenable** (`https://api.keenable.ai`):
       - Header on both: `X-Keenable-Title: mehh-code`.
       - Search: REST `POST /v1/search/public`, body `{ query, max_results: 8 }`. Parses `results[]` (`url`, `title`, `snippet` / `description`).
       - Extract: REST `GET /v1/fetch/public?url=<encoded_url>`. Parses `url`, `title`, `content`. On HTTP `>= 400` with JSON `{"message": "..."}`, maps to `OriginBlocked` with that HTTP status when `message` starts (case-insensitive) with `"the target server"` or `"the page does not exist"`; classifies all other HTTP `>= 400` responses as `RateLimited` (429) or `VendorUnavailable` (401/402/403/5xx).
- `router.rs`:
  - **Session-sticky selection & failover**:
    - `WebProviderChoice::Disabled` → immediately returns disabled error.
    - `WebProviderChoice::Fixed(id)` → for `web_search`, calls only `id` and never silently rotates on error; for `web_fetch` Stage 2 extract fallback, rotates across `ALL_EXTRACT` (prioritizing `id` first when `id` is in `ALL_EXTRACT`, then rotating to the remaining extractors on vendor failure).
    - `WebProviderChoice::Auto` → initializes sticky preference randomly across `ALL_SEARCH` (or `ALL_EXTRACT` for fallback) once per `WebRuntime` so traffic spreads across vendors while staying stable within a session. On `RateLimited` or `VendorUnavailable`, records cooldown (`Instant` expiry) in the shared map, clears/moves sticky affinity to the next healthy non-attempted provider, and retries until one succeeds or all candidates have been tried once. If every provider is currently cooling down, attempts the provider with the earliest cooldown expiry once before returning the last error plus a concise list of attempted providers.
  - **In-memory search memo & single-flight coalescing** *(Default call)*:
    - Key: `(WebProviderChoice, normalized_query)` where `normalized_query` trims, lowercases, and collapses ASCII whitespace.
    - TTL: 15 minutes, max 128 entries (evict expired first, then oldest). Errors are never cached.
    - Single-flight lock map ensures concurrent identical queries (e.g., parallel subagents) run one outbound request and share the result.
- `html_markdown.rs`:
  - Self-contained, bounded, zero-dependency streaming HTML tokenizer + emitter supporting `html_to_markdown(html, base_url)` and `html_to_text(html)`.
  - Skips non-content subtrees: `script`, `style`, `noscript`, `template`, `iframe`, `object`, `embed`, `meta`, `link`.
  - Converts headings (`h1`–`h6`), paragraphs, `<br>`, inline emphasis (`strong`/`b`, `em`/`i`, `code`, `del`/`s`), fenced code blocks (`pre`/`code` preserving `language-*` class when present), links (`a[href]` resolved against `base_url`), nested ordered/unordered lists, blockquotes, horizontal rules, and simple HTML tables to GFM tables.
  - Decodes standard named entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, `&apos;`, `&nbsp;`, `&mdash;`, `&ndash;`, `&hellip;`, `&copy;`) and numeric entities (`&#...;`, `&#x...;`).
  - Strips `data:image/...;base64,...` image URIs into `[image: <alt>]` so inline base64 payloads never exhaust LLM context.
  - Hard limits: max input/output budget `5 MiB`, max tag nesting depth `256`, collapses runs of 3+ blank lines to 2.
- `output.rs` *(Default call)*:
  - Inline char budgets: `20_000` chars for `web_fetch` (head 75% = `15_000` + tail 25% = `5_000`) and `16_000` chars for `web_search` (head truncation), always cutting on UTF-8 char boundaries via `mehh_tools::core::output::floor_char_boundary`.
  - When `web_fetch` exceeds `20_000` chars, spills the complete UTF-8 content to `<temp_dir>/mehh-web-fetch/<uuid>.<ext>` (restricted file mode on Unix) and appends:
    `\n\n[Output truncated: showing head and tail of <total> chars. Full content saved to <path> — use read with offset/limit to inspect specific sections.]`

### 5.3 Tool Contracts

#### `web_search` (`crates/mehh-tools/src/web/search_tool.rs`)
- **Name**: `"web_search"`
- **Flags**: `is_read_only() -> Some(true)`, `execution_mode() -> ToolExecutionMode::Parallel`
- **Parameters**:
  ```json
  {
    "type": "object",
    "properties": {
      "query": { "type": "string", "description": "Web search query" }
    },
    "required": ["query"]
  }
  ```
- **Description**: Concise guidance for searching current information beyond knowledge cutoff, including the current year dynamically (via system clock / fallback constant).
- **Progress updates (`on_update`)**: Emits `"Searching via <Provider>..."` and on failover `"<Prev> unavailable, retrying via <Next>..."`.
- **Output format**: OpenCode-style Markdown blocks (`## [Title](url)\nPublished: ...\n\n<content>`), or `"No search results found. Please try a different query."`. `details` JSON records `{ "provider": "<id>", "resultCount": N, "cached": bool }`.

#### `web_fetch` (`crates/mehh-tools/src/web/fetch_tool.rs`)
- **Name**: `"web_fetch"`
- **Flags**: `is_read_only() -> Some(true)`, `execution_mode() -> ToolExecutionMode::Parallel`
- **Parameters**:
  ```json
  {
    "type": "object",
    "properties": {
      "url": { "type": "string", "description": "HTTP or HTTPS URL to fetch" },
      "format": {
        "type": "string",
        "enum": ["markdown", "text", "html"],
        "description": "Output format (default: markdown)"
      },
      "timeout": {
        "type": "integer",
        "description": "Optional timeout in seconds (default: 30, max: 120)"
      }
    },
    "required": ["url"]
  }
  ```
- **Pipeline**:
  1. **Validate URL** (`normalize_and_validate_http_url`).
  2. **Stage 1 — Local Direct HTTP Fetch**:
     - Sends `GET` with `Accept` negotiated by `format` (`text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1` for `markdown`), `User-Agent: Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; MehhCode/0.1`, `Accept-Language: en-US,en;q=0.9`.
     - Manual redirect loop (max `10` hops across `301/302/303/307/308`): validates every target URL, enforces `DnsPolicy`, and invokes `WebConfig::url_policy_check` (so `PermissionStore` `NeverAllow` rules cannot be bypassed via redirect).
     - Cloudflare challenge retry: if HTTP `403` with header `cf-mitigated: challenge`, retries once with plain `User-Agent: mehh-code`.
     - MIME gate: rejects raster images (`image/*` except `image/svg+xml`) and binary payloads (`application/pdf`, `application/octet-stream`, `audio/*`, `video/*`, archives). Accepts `text/*`, `application/json`, `application/xml`, `application/xhtml+xml`, `application/javascript`, `image/svg+xml`, or missing MIME (sniffed for null bytes / binary magic headers `%PDF-`, `\x89PNG`, `\xff\xd8\xff`, `PK\x03\x04`).
     - Bounded streaming read (`MAX_FETCH_BYTES = 5 * 1024 * 1024`) respecting `CancellationToken` and total timeout.
     - Converts HTML bodies according to `format`; returns `text/markdown`, `text/plain`, `application/json`, etc. directly.
  3. **Stage 2 — Automatic Keyless Extract Fallback** (only when `config.fetch_fallback == true`, `config.provider != Disabled`, `format != "html"`, and `is_eligible_for_remote_extract(&url)` passes):
     - Triggered **only** when Stage 1 either:
       - fails with HTTP `403`, `429`, or `503` (WAF/Cloudflare/bot block after the honest-UA retry), **or**
       - returns HTML whose converted output has `< 200` non-whitespace characters **and** the raw HTML contains `<script` or SPA shell markers (`enable javascript`, `__NEXT_DATA__`, `id="root"`, `id="app"`, `id="__next"`).
     - Never triggered on `404`/`410`, `401`, SSRF rejection, binary MIME rejection, `Cancelled`, or `format == "html"`.
     - Walks the 4 extract-capable keyless providers (`Exa`, `Parallel`, `Firecrawl`, `Keenable`) via `WebRuntime`'s failover router (starting with the pinned `web.provider` first when it is one of those 4, then rotating across the rest on vendor failure), emitting `on_update` progress lines.
     - Re-checks any reported post-redirect `final_url` against `PublicOnly` SSRF policy and `url_policy_check` before returning content.
     - If `format == "text"`, lightly strips Markdown formatting; if all extractors fail, returns the Stage 1 error plus a concise fallback failure summary.
  4. **Stage 3 — Truncate & Spill**: applies `output.rs` (`20_000` char head+tail budget + temp-file spill). `details` JSON records `{ "source": "local" | "<provider>", "contentType": "...", "truncated": bool, "spilledPath": ... }`.

### 5.4 Mehhcode Native Integration

- **`crates/mehh-tools/src/lib.rs`**:
  - Add `"web_search"` and `"web_fetch"` to `ALL_TOOL_NAMES` (included by default in `create_coding_tools`, `create_all_tools`, and `select_coding_tools`).
  - Add `create_coding_tools_with_web(cwd: PathBuf, web: WebConfig)` and `select_coding_tools_with_web(cwd: PathBuf, include: Option<&[String]>, exclude: &[String], web: WebConfig)` sharing one `Arc<WebRuntime>` per session.
  - When `web.enabled == false`, omit both `web_search` and `web_fetch` from default tool creation (if explicitly requested via `--tools`, executing returns a clear disabled error). When `web.provider == WebProviderChoice::Disabled`, omit `web_search` from default tool creation and disable remote extract fallback in `web_fetch`.
- **`crates/mehh-coding-agent/src/settings.rs`**:
  - Add optional `web: Option<WebSettings>` (`#[serde(rename_all = "camelCase")]`) to `Settings`:
    ```json
    {
      "web": {
        "enabled": true,
        "provider": "auto",
        "allowPrivateUrls": false,
        "fetchFallback": true,
        "timeoutSeconds": 30
      }
    }
    ```
  - Merge field-by-field in `SettingsManager::merge`.
  - Add **project escalation guard** in `SettingsManager::try_load_from_paths`: a trusted repo's `.mehh/settings.json` may tighten (`enabled: false`, `allowPrivateUrls: false`, `fetchFallback: false`) but must never widen global defaults (`allowPrivateUrls: true` when global is unset/`false`, `enabled: true` when global is `false`, or `fetchFallback: true` when global is `false`). For `web.provider` *(Default call)*, monotonicity ranks `Disabled < Fixed(id) < Auto`: a project file may narrow (`Auto -> Fixed(id)` / `Disabled`, `Fixed(id) -> Disabled`) or keep the same `Fixed(id)`, but overrides that loosen (`Disabled -> any`, `Fixed(id) -> Auto`) or switch vendors (`Fixed(x) -> Fixed(y)` where `x != y`) are neutralized to `None`.
- **`crates/mehh-coding-agent/src/permissions.rs` & `tool_inspection.rs`**:
  - Extend `PermissionStore::evaluate` `arg_prefix` matching *(Default call)*: when `command` and `path` are absent, parse and normalize `ctx.args.get("url")` and `rule.arg_prefix` as credential-free HTTP(S) URLs (trim, strip `#fragment`, lowercase scheme+host, normalize/strip default `:80`/`:443` ports). Match only on **identical** scheme, host, and effective port (so `"https://docs.rs"` never matches `"https://docs.rs.evil.com"`), plus path-segment prefix matching (if `rule` includes a non-root path, `target_path` must equal `rule_path`, start with `rule_path` when `rule_path` ends with `/`, or continue immediately with `/` or `?`; if `rule` includes `?query`, compare normalized prefix on `&`/end boundary). Validate `tool_name == "web_fetch"` `arg_prefix` entries in `PermissionStore::read_rules_from_file` so malformed `NeverAllow` URL rules fail closed via `load_fail_closed`.
  - Extend `PermissionInspector::inspect` (`Approve` mode preview) to show `url` or `query` when `command`/`path` are absent.
  - Wire `WebConfig::url_policy_check` from `AgentSession` to check `PermissionStore` `NeverAllow` rules on every `web_fetch` redirect hop and remote-extract `final_url` (dependency inversion — no `mehh-tools` → `mehh-coding-agent` cycle).
- **`crates/mehh-coding-agent/src/agent_session.rs` & `crates/mehh-cli/src/main.rs`**:
  - `AgentSession::new_with_trust` builds coding tools with `settings.to_web_config()` + `url_policy_check`. Subagents inherit the same tools and `Arc<WebRuntime>` automatically.
  - `resolve_cli_tools` and `configure_cli_subagent` in `mehh-cli` pass `settings.to_web_config()` so `--tools` / `--exclude-tools` honor `settings.json`.
- **`crates/mehh-coding-agent/src/interactive/events.rs`**:
  - Add `.or_else(|| args.get("url"))` to `args_summary` in `ToolExecutionStart` so `web_fetch` cards display the target URL (`web_search` already displays `query`, and `ToolExecutionUpdate` already renders `on_update` lines).
- **`crates/mehh-ai/src/anthropic.rs`**:
  - Update `to_claude_code_name` and `from_claude_code_name` to match ignoring `_` case-insensitively so `"web_search"` ↔ `"WebSearch"` and `"web_fetch"` ↔ `"WebFetch"` round-trip cleanly under Anthropic OAuth.

---

## 6. Verification Strategy

All tests must run offline without hitting live third-party endpoints:
- **SSRF & URL safety (`mehh-ai`)**: IPv4/IPv6 private, CGNAT, NAT64, IPv4-mapped/translated, cloud metadata floor (even with `DnsPolicy::AllowPrivate`), credential-in-URL rejection, secret-in-query/path exfiltration guard.
- **Wire protocols (`mehh-tools`)**: Mock `TcpListener` tests for all 6 keyless search endpoints and 4 extract endpoints covering JSON + SSE `data:` framing (including CJK text with U+2028/U+2029), Exa `---` parsing, `CRAWL_HTTP_429` as `OriginBlocked`, Parallel `structuredContent`, Firecrawl MCP + REST scrape, Tavily keyless header, TinyFish keyless header, Keenable headers + relayed origin errors.
- **Router & memo (`mehh-tools`)**: Session stickiness, HTTP 429 + `Retry-After` cooldown, 401/403/5xx rotation, `Fixed(id)` strict non-failover, `OriginBlocked` non-rotation, memo TTL hit, and single-flight deduplication under concurrent calls.
- **Fetch pipeline (`mehh-tools`)**: Direct Markdown/HTML/text fetch, manual redirect limit (10 hops) + redirect-to-private-IP block + redirect `url_policy_check` block, Cloudflare `cf-mitigated: challenge` UA retry, 403 and SPA-shell fallback to keyless extract, 404/401/HTML-format non-fallback, binary MIME rejection, 5 MiB streaming body cap, cancellation mid-stream, HTML-to-Markdown (headings, fenced code, relative links, tables, entities, base64 image stripping, depth cap), head+tail truncation + temp-file spill readable by `ReadTool`.
- **Integration (`mehh-coding-agent` & `mehh-cli`)**: `Settings` roundtrip/merge/escalation guard, `PermissionStore` URL `arg_prefix` rules, `SmartApprove`/`Chat`/`Approve` behavior for `web_search`/`web_fetch`, TUI `args_summary` for `web_fetch`, `to_claude_code_name` / `from_claude_code_name` round-trip, and updating existing tool-count tests in `crates/mehh-tools/src/lib.rs`.

---

## 7. What's Left to the Builder

- Exact internal helper decomposition and struct field visibility inside `crates/mehh-tools/src/web/`.
- Exact HTML entity table coverage and Markdown table alignment/escaping nuances inside `html_markdown.rs`.
- Temp-file naming format and best-effort stale temp-file pruning inside `<temp_dir>/mehh-web-fetch/`.
- Exact error message phrasing (so long as errors clearly state the cause and remain concise for LLM consumption).
- Exact test fixture organization and mock server helper utilities.
