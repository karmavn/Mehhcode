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
