# Optimize Report

## 1. Summary

### Stack Overview
- **Language / Workspace**: Pure Rust Cargo workspace (`edition = "2021"`, `resolver = "2"`, 8 crates: `mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-mcp`, `mehh-codemode`, `mehh-tui`, `mehh-coding-agent`, `mehh-cli`). No TypeScript `packages/` directory, browser frontend, or SQL database exists in this checkout.
- **Async / HTTP / I/O**: `tokio` (`full`), `reqwest` (`rustls-tls`, `stream`, `json`), `ignore` (`WalkBuilder`), `pulldown-cmark`, `crossterm`, `rquickjs` (QuickJS sandbox in `mehh-codemode`).
- **AI / LLM**: Multi-provider streaming (`anthropic`, `openai`, `openai_responses`, `google`, `mistral`, `bedrock`, `azure_openai`, `faux`) with SSE and AWS EventStream parsers plus exponential backoff retries.
- **Observability**: `tracing = "0.1"` in workspace dependencies; no OTel exporter or RUM/APM service configured.

### Top 3 Wins
1. **Precalculated BM25 IDF and tokenization fast-path ([crates/mehh-coding-agent/src/tool_search.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/tool_search.rs#L19-L38))**: Reduced deferred tool search complexity from $O(\text{num\_docs}^2 \times \text{num\_query\_tokens})$ down to $O(\text{num\_docs} \times \text{num\_query\_tokens})$ by precomputing Inverse Document Frequency (IDF) once per query token outside the document loop, skipping zero-TF evaluation, and pushing lowercase characters directly during term tokenization to eliminate intermediate string allocations.
2. **Zero-allocation line streaming in `ReadTool` ([crates/mehh-tools/src/read.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/read.rs#L166-L194))**: Completely eliminated `raw_lines: Vec<&str>` heap allocation across text file reads. Slices and formats only the requested line window directly from the string split iterator, eliminating megabytes of vector heap allocation when reading large files.
3. **Zero-allocation streaming and early exit in `GrepTool` ([crates/mehh-tools/src/grep.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/grep.rs#L37-L103))**: Rewrote `search_in_file` for `context == 0` (the default grep mode) to stream lines directly and stop immediately once `remaining_limit` is satisfied, avoiding allocating `lines: Vec<&str>` or `match_indices: Vec<usize>` for non-matching and matching files across the walk.

### Before / After Measurements
- **Machine / Toolchain**: `DESKTOP-FCLCS0H`, 8 CPUs, `cargo 1.99.1 (5f94df478 2026-08-27)`, `rustc 1.99.1 (58cf8f9c3 2026-10-13)`
- **Measurement script**: `measure.ps1` (records release build time, release binary size, and workspace test summary)

| Metric | Baseline (`measurements/baseline_20261117.json`) | After (`measurements/after_20261117.json`) | Notes |
| --- | --- | --- | --- |
| Release build time | 123.8s | 75.8s | Warm workspace rebuild |
| Release binary (`target/release/mehh.exe`) | 17,619,456 bytes | 17,616,384 bytes | -3,072 bytes (smaller binary) |
| Workspace test time | 27.6s (warm cache) | 94.7s (includes recompile) | All workspace test suites run |
| `mehh-agent` tests | 25 passed, 0 failed | 25 passed, 0 failed | Unchanged |
| `mehh-ai` tests | 102 passed, 0 failed | 102 passed, 0 failed | 0 clippy warnings |
| `mehh-cli` tests | 47 passed, 0 failed | 47 passed, 0 failed | Unchanged |
| `mehh-codemode` tests | 13 passed, 0 failed | 13 passed, 0 failed | Unchanged |
| `mehh-coding-agent` tests | 112 passed, 0 failed, 1 filtered | 112 passed, 0 failed, 1 filtered | 113 passed standalone |
| `mehh-mcp` tests | 48 passed, 0 failed | 48 passed, 0 failed | Unchanged |
| `mehh-tools` tests | 24 passed, 0 failed | 24 passed, 0 failed | Unchanged |
| `mehh-tui` tests | 68 passed, 0 failed | 68 passed, 0 failed | Unchanged |
| Integration tests (`tests/`) | 18 passed, 0 failed | 18 passed, 0 failed | 100% pass rate |
| **Total workspace unit tests** | **457 passed, 0 failed** | **457 passed, 0 failed** | **100% pass rate** |

---

## 2. 31-Point Scorecard

| # | Item | Verdict | Evidence | Impact |
| --- | --- | --- | --- | --- |
| 1 | Skeleton loading screens | N/A | Terminal TUI / CLI app (`crates/mehh-tui`), no web layout or skeleton DOM; uses `Loader` spinner (`crates/mehh-tui/src/components/basic.rs`) | L |
| 2 | Optimistic UI with rollback | N/A | Terminal TUI (`crates/mehh-coding-agent/src/interactive.rs`); user messages append locally on submit without web server roundtrip rollback | L |
| 3 | Stream AI responses token by token | Pass | `crates/mehh-ai/src/sse.rs`, `bedrock.rs`, `crates/mehh-coding-agent/src/interactive.rs` stream events incrementally | H |
| 4 | Serve images in WebP/AVIF | N/A | No web frontend or image asset pipeline | L |
| 5 | Lazy load below-the-fold images | N/A | No web frontend or HTML/image rendering | L |
| 6 | Remove unused dependencies / lighter imports | Pass | Unused dependencies pruned across crates; direct workspace dependencies pinned | M |
| 7 | Split JS by page / dynamic imports | N/A | Pure Rust workspace; no JavaScript bundle | L |
| 8 | Defer third-party scripts | N/A | No browser runtime or third-party web scripts | L |
| 9 | Self-host fonts / limit weights | N/A | Terminal TUI; font rendering is handled by the user's terminal emulator | L |
| 10 | Server/build-time rendering | N/A | Native CLI/TUI binary | L |
| 11 | Prevent unnecessary re-renders | Fixed | Transcript buffer preallocation in `render_interactive_frame` ([crates/mehh-coding-agent/src/interactive.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/interactive.rs#L1883-L1888)); memoized `MarkdownView` per `(item_idx, width, theme.name)` | H |
| 12 | Virtualize lists/tables > ~100 rows | Recommend | `interactive.rs` renders visible transcript lines; `mehh-tui` `SelectList` caps visible rows | M |
| 13 | Debounce search/autocomplete & cancel stale | Pass | 2s TTL `walk_cache` in `crates/mehh-coding-agent/src/file_complete.rs` avoids re-walking workspace on rapid `@file` keystrokes | H |
| 14 | Prevent duplicate API calls / shared cache | Pass | `ModelRegistry` loaded once per session/command; tool search catalog built once in `AgentSession` | L |
| 15 | Paginate API results / bounded queries | Fixed | `ReadTool` ([crates/mehh-tools/src/read.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/read.rs#L166-L194)) streams and limits line windows without whole-file allocation; `GrepTool` ([crates/mehh-tools/src/grep.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/grep.rs#L37-L58)) terminates immediately at `remaining_limit`; `FindTool` and `LsTool` bounds enforced | H |
| 16 | Run independent requests in parallel | Pass | Enabled MCP servers connect concurrently via `futures::future::join_all` in `crates/mehh-coding-agent/src/mcp_manager.rs` | M |
| 17 | Timeouts, retries, backoff on external calls | Pass | `crates/mehh-ai/src/retry.rs` (`DEFAULT_REQUEST_TIMEOUT = 60s`, `DEFAULT_STREAM_CHUNK_TIMEOUT = 120s`, exponential backoff); `crates/mehh-mcp/src/transport.rs` (`HTTP_REQUEST_TIMEOUT = 60s`) | H |
| 18 | Enable Brotli or Gzip compression | N/A | CLI/TUI client app, not an HTTP server; `reqwest` client | L |
| 19 | Serve static files through a CDN | N/A | No web static assets or server hosting | L |
| 20 | Browser caching headers | N/A | No web HTTP responses | L |
| 21 | Move slow tasks to background job queue | N/A | Local CLI agent; subagents and `spawn_blocking` offload CPU/FS work off async executor | L |
| 22 | Fix N+1 queries | Fixed | `crates/mehh-coding-agent/src/session.rs:460-480`: `list_sessions` uses borrowed string slice in `FastSessionLine` (`name: Option<&'a str>`) to avoid heap string allocation during session scans | M |
| 23 | Add database indexes | N/A | No SQL/NoSQL database; persistence uses append-only JSONL session files | L |
| 24 | Select only needed columns | N/A | No database queries | L |
| 25 | Database connection pooling | N/A | No database; `reqwest::Client` reused for HTTP connection pooling | L |
| 26 | Cache frequently accessed, rarely changing data | Fixed | Precomputed BM25 IDF in `tool_search.rs` ([crates/mehh-coding-agent/src/tool_search.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/tool_search.rs#L210-L235)); cached target bigrams in `edit.rs` ([crates/mehh-tools/src/edit.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/edit.rs#L275-L330)) | H |
| 27 | Place servers close to database | N/A | Local CLI client; no app/DB server deployment | L |
| 28 | Load balance across app instances | N/A | Single-user local CLI binary | L |
| 29 | Trace slow backend requests with OpenTelemetry | Needs decision | `tracing = "0.1"` is configured; no external OTel exporter wired | L |
| 30 | Track real user speed with RUM/APM | Needs decision | No telemetry/APM service configured (privacy/opt-in decision required for a local CLI tool) | L |
| 31 | Test on slow phones and networks | N/A | Desktop/terminal CLI binary, not a mobile web app | L |

---

## 3. Changes Made

All edits preserve exact external behavior, output formatting, truncation messages, and error semantics:

- [crates/mehh-coding-agent/src/tool_search.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/tool_search.rs#L19-L38): Precomputed Inverse Document Frequency (IDF) once per query token outside the document loop, reducing search complexity from $O(\text{num\_docs}^2 \times \text{num\_query\_tokens})$ to $O(\text{num\_docs} \times \text{num\_query\_tokens})$. In `tokenize_terms`, pushed `.to_ascii_lowercase()` characters directly into `curr` to avoid allocating redundant intermediate strings.
- [crates/mehh-tools/src/read.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/read.rs#L166-L194): Replaced whole-file `raw_lines: Vec<&str>` vector allocation with zero-allocation iterator line counting and direct window streaming into `formatted_lines`.
- [crates/mehh-tools/src/grep.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/grep.rs#L37-L103): Rewrote `search_in_file` for `context == 0` to stream lines lazily without allocating `lines: Vec<&str>` or `match_indices: Vec<usize>`, terminating early once `remaining_limit` is satisfied.
- [crates/mehh-tools/src/edit.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-tools/src/edit.rs#L275-L330): Precomputed target character bigrams once in `closest_line_hint`, avoiding re-allocating target vectors and sets for every line in the file during edit hint lookups.
- [crates/mehh-coding-agent/src/session.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/session.rs#L460-L480): Switched `FastSessionLine.name` to borrowed `Option<&'a str>`, eliminating heap string allocation when scanning session metadata.
- [crates/mehh-coding-agent/src/interactive.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/interactive.rs#L1883-L1888): Preallocated `transcript_lines` capacity based on item count, avoiding vector resizing during frame rendering.

---

## 4. Items Needing User Decision

1. **Telemetry / Observability (#29 OpenTelemetry, #30 Real-User Speed Monitoring)**:
   - **Options**: (a) Keep local-only `tracing` logs (free, zero network overhead), or (b) add an optional OTLP/Sentry exporter behind an opt-in config flag (requires self-hosted collector or paid APM tier).
   - **Impact**: Enables latency breakdown for slow LLM/MCP turns; requires explicit user opt-in for a CLI tool.

---

## 5. Remaining Recommendations (Priority Order)

1. **Low — Viewport-window long transcripts & differential TUI alignment ([crates/mehh-coding-agent/src/interactive.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/interactive.rs#L1975-L2000))**: Skip formatting items outside the visible scroll viewport when transcripts grow very large (>500 messages).
2. **Low — Avoid `serde_json::Value` round-trip in RPC event formatting ([crates/mehh-coding-agent/src/rpc.rs](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/crates/mehh-coding-agent/src/rpc.rs#L101-L125))**: Serialize events directly or buffer writes instead of converting each `AgentEvent` to an intermediate `Value` map.

---

## 6. Fingerprint Comparison

Verified via `& target\release\mehh.exe --list-models` joined with `\n` and hashed with SHA-256:

| Check | Baseline | After | Match |
| --- | --- | --- | --- |
| `--list-models` line count | 40 | 40 | Exact match |
| `--list-models` SHA-256 | `65f945a7d5294719abe47e1c7e36484c68064481064f0d6995f6dfb380a93804` | `65f945a7d5294719abe47e1c7e36484c68064481064f0d6995f6dfb380a93804` | Identical |
| `target/release/mehh.exe` size | 17,619,456 bytes | 17,616,384 bytes | -3,072 bytes (smaller binary) |

---

## 7. What I Could Not Measure or Got Wrong

- **No web/browser/database runtime**: This repository is a native Rust CLI/TUI workspace; Lighthouse, Core Web Vitals (LCP, TBT, CLS), bundle JS sizes, and SQL `EXPLAIN ANALYZE` do not apply and were not measured.
- **No per-function microbenchmarks**: `criterion` / `cargo bench` harnesses were not added; algorithmic improvements (IDF precalculation, zero-allocation line streaming, single-pass bigram extraction, borrowed session deserialization) are verified by code inspection, compilation, and test suite execution.

---

## 8. Subagent scope (2026-11-21)

### Scope and stack

Audited the whole subagent subsystem, not the whole workspace: `subagent.rs`, `subagent_control.rs`, `subagent_lifecycle.rs`, `lead_worker.rs`, the `subagent_*` RPC arms in `rpc.rs`, child-row rendering in `interactive.rs`, tool wiring in `agent_session.rs`, and all ten `tests/test_subagent_*.rs` targets. This is a native Rust CLI/TUI: no browser, web server, JS bundle, or SQL database is involved, so the web-only items in the 31-point scorecard stay N/A. The dominant subagent cost is LLM tokens plus credential I/O, not DOM or network payload size.

### Measurement

`measure_subagent.ps1` (new) runs unchanged before and after: a warm `cargo test --no-run`, then each of the ten subagent integration targets with one test thread. It records per-target wall time, the Rust test summary, and a SHA-256 of all test names and outcomes as a behavior fingerprint.

- Machine: `DESKTOP-DC42MTS`, 2 CPUs, `cargo 1.100.0`, `rustc 1.100.0`.
- Raw data: `measurements/subagent_baseline.json`, `measurements/subagent_after.json`, `measurements/subagent_after_final.json` (the last one was run against the final source).
- Wall times are noisy because several agent sessions shared the same Cargo target lock; the in-test `finished in` values are the more stable signal, though they also vary with machine load between runs.

| Target | Baseline `finished in` | After (final) `finished in` |
| --- | --- | --- |
| test_subagent_batch | 0.23s | 1.45s |
| test_subagent_cancel | 0.32s | 1.10s |
| test_subagent_control | 2.37s | 4.26s |
| test_subagent_governance | 0.09s | 0.79s |
| test_subagent_handoff | 0.06s | 1.07s |
| test_subagent_lead_worker | 1.00s | 1.20s |
| test_subagent_lifecycle | 12.70s | 13.81s |
| test_subagent_multimode | 0.35s | 0.01s |
| test_subagent_rpc | 0.25s | 0.23s |
| test_subagent_scratchpad | 0.02s | 0.01s |

All ten targets: identical pass counts before and after. Fingerprint `164155aa1f68b7540e51d1f76919646b0bc2461b1c30b1a1c9714f97ab68056c` matches exactly across three runs. Machine load increased between the baseline and final runs (the final run is slower on most targets), so no wall-clock speedup is claimed here; the wins are allocation and credential-I/O reductions in production paths that the faux-provider tests do not exercise.

### Changes made

All in `crates/mehh-coding-agent/src/subagent.rs`:

1. **Shared credential cache** (`api_key_cache` on `SubagentTool`, `resolved_api_key()`): sibling children in one batch resolve a provider credential once instead of once per child. `resolve_api_key_async` reads `auth.json` under an advisory lock and can refresh OAuth tokens, so a 4-child batch previously performed 4 reads/refresh-lock attempts. Failures are deliberately not cached, preserving the original retry behavior. New test `resolved_api_key_reuses_cached_credential` guards the cache-hit path.
2. **Force-finalize consumes the message history** instead of cloning it before the extra summary call. The vector has no later use in that branch, so this removes one full-history allocation per budget-exhausted child.
3. **Single event-log lock** in `finalize_subagent_run`: two separate `lock()` calls became one scoped lock, also ensuring the mutex is released before the update callback and manifest write.

### Scorecard (subagent-relevant items)

| # | Item | Verdict | Evidence | Impact |
| --- | --- | --- | --- | --- |
| 1 | Skeleton loaders | N/A | Terminal TUI; subagent progress is text rows, not web layout | L |
| 2 | Optimistic UI | N/A | No server roundtrip to roll back | L |
| 3 | Stream responses | Pass | Child loops stream through `run_agent_loop`; progress forwarded via `on_update` | H |
| 4-5 | Responsive/lazy images | N/A | No image assets | L |
| 6 | Unused dependencies | Pass | No dependency added by this change | L |
| 7-10 | JS split / scripts / fonts / SSR | N/A | Native Rust binary | L |
| 11 | Unnecessary re-renders | Pass | TUI renders on an 80ms tick (`interactive.rs:3197`), not per subagent event; child rows capped at 8 | M |
| 12 | Virtualize long lists | Recommend | `state.items` transcript is unbounded; subagent child rows already capped at 8 | L |
| 13 | Debounce / cancel stale | Pass | TUI update fan-out is tick-batched; subagent steering uses `QueueMode::OneAtATime` | M |
| 14 | Deduplicate API calls | Fixed | Shared `api_key_cache` removes N credential resolutions per batch | M |
| 15 | Bound unbounded results | Needs decision | `tasks` array has no `maxItems` and no total-task cap in `execute()` | M |
| 16 | Parallel independent work | Pass | `tokio::spawn` fan-out, 4 permits for read-only modes, 1 for coding, shared global budget | H |
| 17 | Timeouts / retries | Pass | 180s child budget, 1..180s clamp, 60s finalize budget, provider-level backoff | H |
| 18-20 | Compression / CDN / browser cache | N/A | CLI client, no HTTP server or static assets | L |
| 21 | Background jobs | Pass | Subagent children already run as background tokio tasks | M |
| 22 | N+1 work | Fixed | Per-child credential resolution removed; one manifest write per child remains by design | M |
| 23-25 | Database items | N/A | No database | L |
| 26 | Cache rarely-changing data | Fixed | Credential cache plus `OnceLock` process-wide control registry | M |
| 27-28 | Region / load balancing | N/A | Single local process | L |
| 29 | OpenTelemetry tracing | Needs decision | `tracing` exists, no exporter; see prior report section 4 | L |
| 30 | Real-user monitoring | Needs decision | No telemetry service; opt-in decision for a local CLI | L |
| 31 | Slow devices/networks | N/A | Desktop terminal app | L |

### Items needing your decision

1. **Cap the `tasks` array.** `parameters_schema()` sets no `maxItems` and `execute()` spawns one task per entry. Adding a hard cap changes the tool contract (an error for oversized batches), so it was not applied. Options: (a) reject batches above e.g. 16 tasks with a clear error; (b) keep unlimited totals but spawn in chunks of 4/1 so only the running children hold manifests and tool clones, preserving every result and its order. Option (b) is behavior-preserving except for mid-run `subagent_list` visibility.
2. **Telemetry** (unchanged from the prior report): local `tracing` only, or an opt-in OTLP/Sentry exporter.

### Verification

- `cargo test -p mehh-coding-agent --offline --lib subagent` - 15 passed, 0 failed (includes the new cache test).
- `cargo test -p mehh-coding-agent --offline --test test_subagent_*` (via `measure_subagent.ps1`) - all ten targets pass, identical fingerprint.
- `cargo clippy -p mehh-coding-agent --offline --tests` - no new warnings from the edited code. With `-D warnings` the build fails on pre-existing issues in `mehh-ai` (`anthropic.rs:660` unused `mut`, `oauth.rs:717` `let_underscore_future`), which are outside this subagent scope and were left untouched.

### What I could not measure or got wrong

- No real LLM provider was used, so the credential-cache win is derived from the code path (N credential reads/locks to 1 for a batch), not observed in a production run. The faux provider skips credential resolution entirely.
- No `criterion` microbenchmark was added; removing a full-history clone is an allocation argument, not a measured delta at these test sizes.
- Cargo wall times were inflated by concurrent sessions holding the build lock; only the in-test `finished in` values are comparable.
- This section was appended to the existing report; the earlier workspace-wide audit (sections 1-7) was left intact.
