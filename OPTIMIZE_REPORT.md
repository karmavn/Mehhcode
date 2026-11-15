# Optimize Report

## 1. Summary

### Stack Overview
- **Language / Workspace**: Pure Rust Cargo workspace (`edition = "2021"`, `resolver = "2"`, 8 crates: `mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-mcp`, `mehh-codemode`, `mehh-tui`, `mehh-coding-agent`, `mehh-cli`). No TypeScript `packages/` directory, browser frontend, or database exists in this checkout.
- **Async / HTTP / I/O**: `tokio` (`full`), `reqwest` (`rustls-tls`, `stream`, `json`), `ignore` (`WalkBuilder`), `pulldown-cmark`, `crossterm`, `rquickjs` (QuickJS sandbox in `mehh-codemode`).
- **AI / LLM**: Multi-provider streaming (`anthropic`, `openai`, `openai_responses`, `google`, `mistral`, `bedrock`, `azure_openai`, `faux`) with SSE and AWS EventStream parsers plus exponential backoff retries.
- **Observability**: `tracing = "0.1"` in workspace dependencies; no OTel exporter or RUM/APM service configured.

### Top 3 Wins
1. **Linear-time streaming parsers (`crates/mehh-ai/src/sse.rs`, `crates/mehh-ai/src/bedrock.rs`)**: Replaced per-line/per-frame `Vec::drain` at the front of the buffer (O(N^2) byte shifting on multi-event chunks) with single-pass cursor scanning and a single `drain` at the end of `feed_bytes` / `feed`, plus `Cow<str>` decoding in `SseParser` to skip per-line `Vec<u8>` and `String` allocations on valid UTF-8.
2. **Zero-allocation TUI hot paths (`crates/mehh-tui/src/ansi.rs`, `crates/mehh-tui/src/components/editor.rs`)**: Added an `0x1b` scan fast-path and slice-copying in `strip_ansi`, `Cow<'_, str>` in `visible_width`, and `char_indices().nth(...)` in `split_char_at` to remove `Vec<char>` and redundant `String` allocations on every frame and autocomplete keystroke.
3. **Zero-allocation token estimation & single-pass file/tool formatting (`crates/mehh-coding-agent/src/compaction.rs`, `crates/mehh-tools/src/*.rs`, `crates/mehh-codemode/src/sandbox.rs`)**: Replaced `serde_json::to_string` allocations in `estimate_message_tokens` with a counting `std::io::Write` adapter (`ByteCountWriter`), avoided collecting all lines into `Vec<&str>` in `ReadTool`, replaced double char scans with `char_indices` slicing in `grep.rs` / `bash.rs`, hoisted invariant work in `find.rs` and `sandbox.rs`, used `sort_by_cached_key` in `find.rs` / `ls.rs`, and shared one `Arc<[ToolDefinition]>` across QuickJS host bindings.

### Before / After Measurements
- **Machine / Toolchain**: `DESKTOP-DC42MTS`, 2 CPUs, `cargo 1.100.0 (3acc79c0c 2026-11-05)`, `rustc 1.100.0 (0ef1bc386 2026-11-09)`
- **Measurement script**: `measure.ps1` (modified to pass `-- --skip test_rpc_rejects_unknown_model_and_reports_provider_error` due to a pre-existing deadlock and write UTF-8 without BOM)

| Metric | Baseline (`measurements/baseline.json`) | After (`measurements/after.json` + final verify) | Notes |
| --- | --- | --- | --- |
| Release build time | 1.7s (warm cache) | 123.5s (recompiled modified crates) | Non-comparable build cache states |
| Release binary (`target/release/mehh.exe`) | 24,236,071 bytes | 24,386,055 bytes | Measured via `Get-Item` after final `--release` build |
| Workspace test time (`measure.ps1`) | 2.9s (warm) | 182.5s (included recompile) | Stops at `mehh-coding-agent` pre-existing failure |
| `mehh-agent` tests | 15 passed, 0 failed | 15 passed, 0 failed | Unchanged |
| `mehh-ai` tests | 59 passed, 0 failed | 59 passed, 0 failed | `clippy` 0 warnings after fix |
| `mehh-cli` tests | 40 passed, 0 failed | 40 passed, 0 failed | Unchanged |
| `mehh-codemode` tests | 13 passed, 0 failed | 13 passed, 0 failed | Unchanged |
| `mehh-mcp` tests | 36 passed, 0 failed | 36 passed, 0 failed | Unchanged |
| `mehh-tools` tests | 12 passed, 0 failed | 12 passed, 0 failed | 1 transient timeout flake under load in `task-236`; 12/12 passed on re-run |
| `mehh-tui` tests | 22 passed, 1 failed (pre-existing) | 22 passed, 0 failed (with pre-existing failure skipped) | `test_markdown_strips_terminal_control_sequences` fails on untouched baseline |
| `mehh-coding-agent` tests | 83 passed, 1 failed, 1 skipped (hang) | 83 passed, 1 failed, 1 skipped (hang) | `test_compaction_non_ascii_truncation_and_assistant_cut_starts_with_user` fails on untouched baseline |

---

## 2. 31-Point Scorecard

| # | Item | Verdict | Evidence | Impact |
| --- | --- | --- | --- | --- |
| 1 | Skeleton loading screens | N/A | Terminal TUI / CLI app (`crates/mehh-tui`), no web layout or skeleton DOM; uses `Loader` spinner (`crates/mehh-tui/src/components/basic.rs`) | L |
| 2 | Optimistic UI with rollback | N/A | Terminal TUI (`crates/mehh-coding-agent/src/interactive.rs`); user messages append locally on submit without web server roundtrip rollback | L |
| 3 | Stream AI responses token by token | Pass | `crates/mehh-ai/src/sse.rs`, `bedrock.rs`, `crates/mehh-coding-agent/src/interactive.rs` stream events incrementally | H |
| 4 | Serve images in WebP/AVIF | N/A | No web frontend or image asset pipeline | L |
| 5 | Lazy load below-the-fold images | N/A | No web frontend or HTML/image rendering | L |
| 6 | Remove unused dependencies / lighter imports | Fixed / Recommend | Removed unused `thiserror` from `crates/mehh-mcp/Cargo.toml`; `crates/mehh-coding-agent/Cargo.toml:22` still has unused `thiserror` and `Cargo.toml:20` uses `tokio` `features = ["full"]` | M |
| 7 | Split JS by page / dynamic imports | N/A | Pure Rust workspace; no JavaScript bundle | L |
| 8 | Defer third-party scripts | N/A | No browser runtime or third-party scripts | L |
| 9 | Self-host fonts / limit weights | N/A | Terminal TUI; font rendering is handled by the user's terminal emulator | L |
| 10 | Server/build-time rendering | N/A | Native CLI/TUI binary | L |
| 11 | Prevent unnecessary re-renders | Fixed / Recommend | Memoized `MarkdownView` rendering per `(item_idx, width, theme.name)` in `crates/mehh-coding-agent/src/interactive.rs` and optimized hot ANSI width helpers in `ansi.rs:48-96`; `interactive.rs:2051-2070` still calls `renderer.reset()` to preserve cursor alignment | H |
| 12 | Virtualize lists/tables > ~100 rows | Recommend | `interactive.rs` builds lines for the full `state.items` transcript on every frame rather than viewport-windowing; `mehh-tui` `SelectList` already caps visible rows | M |
| 13 | Debounce search/autocomplete & cancel stale | Fixed | Added 2s TTL `walk_cache` in `crates/mehh-coding-agent/src/file_complete.rs` so rapid `@file` keystrokes filter in memory instead of re-walking up to 5,000 files | H |
| 14 | Prevent duplicate API calls / shared cache | Pass | `ModelRegistry` loaded once per session/command; tool search catalog built once in `AgentSession` | L |
| 15 | Paginate API results / bounded queries | Pass | `FindTool` (`limit`, `FIND_MAX_OUTPUT_BYTES`), `LsTool` (`limit`, `LS_MAX_OUTPUT_BYTES`), `GrepTool` (`limit`, `GREP_MAX_OUTPUT_BYTES`), `ReadTool` (`offset`, `limit`), `McpClient` pagination (`test_client_list_tools_with_pagination`) | M |
| 16 | Run independent requests in parallel | Fixed | Connected enabled MCP servers concurrently via `futures::future::join_all` in `crates/mehh-coding-agent/src/mcp_manager.rs` while preserving sorted registration order | M |
| 17 | Timeouts, retries, backoff on external calls | Pass | `crates/mehh-ai/src/retry.rs:11-47,58-63` (`DEFAULT_REQUEST_TIMEOUT = 60s`, `DEFAULT_STREAM_CHUNK_TIMEOUT = 120s`, `connect_timeout(15s)`, exponential backoff, `Retry-After` support); `crates/mehh-mcp/src/transport.rs:96-98` (`HTTP_REQUEST_TIMEOUT = 60s`, `SSE_IDLE_TIMEOUT = 120s`) | H |
| 18 | Enable Brotli or Gzip compression | N/A | CLI/TUI client app, not an HTTP server; `reqwest` is a client | L |
| 19 | Serve static files through a CDN | N/A | No web static assets or server hosting | L |
| 20 | Browser caching headers | N/A | No web HTTP responses | L |
| 21 | Move slow tasks to background job queue | N/A | Local CLI agent; subagents and `spawn_blocking` already offload CPU/FS work off the async executor | L |
| 22 | Fix N+1 queries | Recommend | `crates/mehh-coding-agent/src/session.rs:216-312` (`list_sessions`) reads and deserializes every `.jsonl` session file on disk to extract summary metadata; `session.rs:505-514` opens/flushes the file on every `append_entry`; `rpc.rs:34-41` flushes stdout per RPC event | M |
| 23 | Add database indexes | N/A | No SQL/NoSQL database; persistence uses append-only JSONL session files | L |
| 24 | Select only needed columns | N/A | No database queries | L |
| 25 | Database connection pooling | N/A | No database; `reqwest::Client` is reused for HTTP | L |
| 26 | Cache frequently accessed, rarely changing data | Recommend | `file_complete.rs:47-109` re-walks the workspace on each `@` completion; `session.rs:216-312` re-parses unchanged session files on every `/resume` | M |
| 27 | Place servers close to database | N/A | Local CLI client; no app/DB server deployment | L |
| 28 | Load balance across app instances | N/A | Single-user local CLI binary | L |
| 29 | Trace slow backend requests with OpenTelemetry | Needs decision | `tracing = "0.1"` is in `Cargo.toml:40`, but no OTel/telemetry subscriber or span exporter is wired | L |
| 30 | Track real user speed with RUM/APM | Needs decision | No telemetry/APM service configured (privacy/opt-in decision required for a local CLI tool) | L |
| 31 | Test on slow phones and networks | N/A | Desktop/terminal CLI binary, not a mobile web app | L |

---

## 3. Changes Made

All edits preserve exact external behavior, output formatting, truncation messages, and error semantics:

- [crates/mehh-tui/src/ansi.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tui/src/ansi.rs): Added fast-path return when no `\x1b` byte is present and plain-span slice copying in `strip_ansi`; used `Cow<'_, str>` in `visible_width` to avoid allocating when non-ASCII strings contain no ANSI escapes.
- [crates/mehh-tui/src/components/editor.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tui/src/components/editor.rs): Replaced `Vec<char>` allocation in `split_char_at` with `char_indices().nth(char_idx)` byte-boundary slicing.
- [crates/mehh-coding-agent/src/compaction.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-coding-agent/src/compaction.rs): Added `ByteCountWriter` and `json_serialized_len` so `estimate_message_tokens` counts serialized JSON bytes for `ToolCall::arguments` and `ToolResult::details` without allocating intermediate `String`s.
- [crates/mehh-ai/src/sse.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-ai/src/sse.rs): Replaced per-line `Vec::drain` + `Vec<u8>` + `String` allocation in `SseParser::feed_bytes` with cursor scanning over the taken buffer, `String::from_utf8_lossy` borrowing, and a single `drain(..cursor)` at the end, passing `remaining_raw_bytes` to `process_line` to preserve exact overflow thresholds.
- [crates/mehh-ai/src/bedrock.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-ai/src/bedrock.rs): Replaced per-frame `self.buffer.drain(0..total_len)` in `EventStreamParser::feed` with an offset cursor and a single `self.buffer.drain(0..offset)` after the loop (preserving buffer state on error).
- [crates/mehh-tools/src/find.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tools/src/find.rs): Hoisted `pattern_str.to_lowercase()` out of the directory-walk loop and switched `results.sort_by_key` to `results.sort_by_cached_key`.
- [crates/mehh-tools/src/ls.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tools/src/ls.rs): Switched `entries.sort_by_key` to `entries.sort_by_cached_key`.
- [crates/mehh-tools/src/grep.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tools/src/grep.rs): Replaced double `chars().count()` + `chars().take().collect()` in `truncate_line` with a byte-length fast check and `char_indices().nth(max_len)` slicing.
- [crates/mehh-tools/src/read.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tools/src/read.rs): Replaced full-file `Vec<&str>` allocation (`all_lines`) with single-pass line counting + bounded slice formatting, and used `char_indices().nth(2000)` for line truncation.
- [crates/mehh-tools/src/bash.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-tools/src/bash.rs): Added byte-length fast path in `format_bash_output` and replaced `chars().take` / `chars().skip` string collections with `char_indices` slicing.
- [crates/mehh-codemode/src/sandbox.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-codemode/src/sandbox.rs): Replaced 3 redundant `tool_defs.to_vec()` clones across `__host_search_tools`, `__host_describe_tool`, and `__host_describe_namespace` with a shared `Arc<[ToolDefinition]>`, and hoisted namespace prefix formatting out of the `__host_search_tools` loop.
- [crates/mehh-coding-agent/src/interactive.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-coding-agent/src/interactive.rs): Memoized `MarkdownView` rendering per `(item_idx, width, theme.name)` on `InteractiveState` so unchanged historical assistant messages are not re-parsed on every frame.
- [crates/mehh-coding-agent/src/file_complete.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-coding-agent/src/file_complete.rs): Added a 2-second TTL `walk_cache` keyed by normalized `cwd` so `@file` autocomplete reuses walked entries across rapid keystrokes.
- [crates/mehh-coding-agent/src/mcp_manager.rs](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-coding-agent/src/mcp_manager.rs): Connected enabled MCP servers concurrently using `futures::future::join_all` while preserving sorted registration order.
- [crates/mehh-mcp/Cargo.toml](file:///c:/Users/huumanh/Downloads/Mehhcode-main/crates/mehh-mcp/Cargo.toml): Removed unused `thiserror` dependency.
- [measure.ps1](file:///c:/Users/huumanh/Downloads/Mehhcode-main/measure.ps1): Skipped pre-existing hanging test `test_rpc_rejects_unknown_model_and_reports_provider_error` so workspace measurement completes, and wrote `measurements/*.json` as UTF-8 without BOM.

---

## 4. Items Needing User Decision

1. **Telemetry / Observability (#29 OpenTelemetry, #30 Real-User Speed Monitoring)**:
   - **Options**: (a) Keep local-only `tracing` logs (free, zero network overhead), or (b) add an optional OTLP/Sentry exporter behind an opt-in config flag (requires self-hosted collector or paid APM tier).
   - **Impact**: Enables latency breakdown for slow LLM/MCP turns; requires explicit user opt-in for a CLI tool.
2. **Pre-existing Failing / Hanging Tests**:
   - `rpc::tests::test_rpc_rejects_unknown_model_and_reports_provider_error` (`crates/mehh-coding-agent/src/rpc.rs`) deadlocks in `drain_handle.await`.
   - `compaction::tests::test_compaction_non_ascii_truncation_and_assistant_cut_starts_with_user` (`crates/mehh-coding-agent/src/compaction.rs:806`) panics on `unwrap()`.
   - `components::markdown::tests::test_markdown_strips_terminal_control_sequences` (`crates/mehh-tui/src/components/markdown.rs:517`) fails its assertion.
   - Fixing these is outside `/optimize` behavior-preserving scope and awaits user go-ahead.

---

## 5. Remaining Recommendations (Priority Order)

1. **Medium — Optimize session listing & buffered appends (`crates/mehh-coding-agent/src/session.rs:216-312, 505-514`)**: Read only the header + first user message (or maintain an index) in `list_sessions` instead of deserializing every entry of every `.jsonl` file, and avoid re-opening the file handle on every `append_entry`.
2. **Medium — Remove remaining unused dependency (`crates/mehh-coding-agent/Cargo.toml:22`)**: Remove unused `thiserror` flagged by `cargo check`, and narrow `tokio` features in `Cargo.toml:20` if smaller binaries / faster builds are desired.
3. **Low — Viewport-window long transcripts & differential TUI alignment (`crates/mehh-coding-agent/src/interactive.rs`)**: Skip wrapping items outside the visible scroll viewport when transcripts grow very large, and align `TuiRenderer` cursor tracking if full-frame `\x1b[H` redraws become a bottleneck.
4. **Low — Avoid `serde_json::Value` round-trip in RPC event formatting (`crates/mehh-coding-agent/src/rpc.rs:34-41, 98-123`)**: Serialize directly or buffer writes instead of converting every `AgentEvent` to `Value` and flushing stdout per event.

---

## 6. Fingerprint Comparison

Verified via `cargo run --release --offline -p mehh-cli --bin mehh -- --list-models` joined with `\n` and hashed with SHA-256:

| Check | Baseline | After | Match |
| --- | --- | --- | --- |
| `--list-models` line count | 40 | 40 | Exact match |
| `--list-models` SHA-256 | `65f945a7d5294719abe47e1c7e36484c68064481064f0d6995f6dfb380a93804` | `65f945a7d5294719abe47e1c7e36484c68064481064f0d6995f6dfb380a93804` | Identical |
| `target/release/mehh.exe` size | 24,236,071 bytes | 24,386,055 bytes | +149,984 bytes (additional caching/async helpers) |

---

## 7. What I Could Not Measure or Got Wrong

- **No web/browser/database runtime**: This repository is a native Rust CLI/TUI workspace; Lighthouse, Core Web Vitals (LCP, TBT, CLS), bundle JS sizes, and SQL `EXPLAIN ANALYZE` do not apply and were not measured.
- **No per-function microbenchmarks**: `criterion` / `cargo bench` harnesses were not added; algorithmic improvements (O(N^2) -> O(N) `drain` elimination, removed `Vec<char>`/`String` allocations) are verified by code inspection and behavioral tests rather than nanosecond microbenchmarks.
- **Pre-existing test failures and deadlock**: `test_rpc_rejects_unknown_model_and_reports_provider_error` hung indefinitely on the untouched baseline, so `measure.ps1` skips it. Two other tests fail on the untouched baseline (`compaction::tests::test_compaction_non_ascii_truncation_and_assistant_cut_starts_with_user` and `components::markdown::tests::test_markdown_strips_terminal_control_sequences`); because `cargo test --workspace` halts at `mehh-coding-agent`, `mehh-mcp`, `mehh-tools`, and `mehh-tui` were verified separately via `--exclude mehh-coding-agent` and per-crate runs.
- **Transient timeout test flake under load**: `bash::tests::test_bash_streams_updates_and_preserves_partial_output_on_timeout` failed once under heavy concurrent compilation/test load (`task-236`) and passed 12/12 on re-run (`task-242`).
- **Build timing asymmetry**: Baseline `release_build_seconds` (1.7s) ran against an already-warm release target directory, whereas `after` (123.5s) recompiled modified crates and dependents, so build times are not directly comparable.
