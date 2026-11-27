# Phase 2.1 Design: Unified CLI Session Bootstrap (`crates/mehh-cli/src/bootstrap.rs`)

## Intent & Why

Previously, `run_async_cli` in `crates/mehh-cli/src/main.rs` mixed ~300 lines of imperative session assembly (trust resolution, settings/permission mode defaults, model/faux resolution, tool selection, MCP startup, OAuth/API key resolution, thinking sync, subagent attachment, and custom system prompts) directly with mode dispatch (`interactive`, `rpc`, `text`, `json`). Keeping that pipeline private inside `main.rs` made it impossible to unit-test CLI bootstrap invariants directly across all 4 `CliMode` variants and contributed to cross-mode regressions such as RPC permission mode mismatches.

## Goals & Non-Goals

- **Goals:**
  - Extract `BootstrappedSession`, `bootstrap_session`, `resolve_trust_and_settings`, `resolve_startup_model_and_provider`, and `close_mcp` into `crates/mehh-cli/src/bootstrap.rs`.
  - Shrink `run_async_cli` in `crates/mehh-cli/src/main.rs` to: bootstrap -> dispatch mode -> `close_mcp`.
  - Add isolated unit tests in `bootstrap.rs` verifying uniform session configuration across `Interactive`, `Rpc`, `Text`, and `Json` (including `--permission-mode chat` preservation in RPC, thinking overrides, `todo`/`subagent` tool wiring, and `MEHH_FAUX_SCRIPT`).
- **Non-Goals:**
  - Splitting `mehh-coding-agent` files (`events.rs`, `commands.rs`, `agent_session.rs`, `rpc.rs`) in this slice (deferred to Phase 2.2+).
  - Changing any CLI flag semantics or `pub fn` signatures in `main.rs`.
