# Phase 3 Design: FEATURE_MAP.md & Offline E2E Verification Harness

## Intent & Why

Even with compile-time Clippy lints and crate boundary tests from Phase 1, AI agents working on Mehhcode still face two recurring failure modes:
1. Searching across ~112k lines of Rust blindly instead of jumping directly to the owning module and its verification command.
2. Shipping changes that compile cleanly (`cargo check` / `clippy`) yet break real CLI flags, JSON/RPC stdout contracts, session persistence, permission gating, or TUI frame bounds (`render_interactive_frame`), leaving the human user to test manually.

Phase 3 adds a living `FEATURE_MAP.md` (verified against disk in `workspace_guardrails.rs`) and an offline, deterministic E2E verification harness driven by `mehh --provider faux` and `MockTerminal`.

## Goals, Non-Goals, and Anti-Goals

- **Goals:**
  - Create `FEATURE_MAP.md` mapping every mode, slash command, keybinding, built-in tool, governance/sandbox module, session/provider/MCP/subagent component, and verification command 1-1 to exact `crates/*` paths.
  - Enforce `FEATURE_MAP.md` path accuracy in `crates/mehh-cli/tests/workspace_guardrails.rs` and point `AGENTS.md` to it.
  - Extend `--provider faux` with `MEHH_FAUX_SCRIPT` JSON scripting (fail-closed on malformed JSON) so real `mehh` binary E2E tests can exercise multi-turn tool calls offline.
  - Add real binary E2E tests in `crates/mehh-cli/tests/e2e_cli_rpc.rs` covering CLI flags, text/JSON print modes, piped stdin, stdio JSON-RPC, session reload, and `chat` vs `auto` permission enforcement.
  - Add headless TUI frame E2E tests in `crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs` using `MockTerminal` and `TuiRenderer` to verify startup, slash commands, tool execution lifecycle, and Unicode/code-block resize bounds.
- **Non-Goals:**
  - Adding OS-level PTY crates (e.g. `portable-pty`) or testing Windows ConPTY raw input quirks.
  - Refactoring `mehh-coding-agent` or `mehh-cli` large files (deferred to Phase 2).
- **Anti-Goals:**
  - Calling live LLM APIs, using paid tokens, spawning external MCP servers, or mutating the user's real `~/.mehh/` directory in any test.

## Constraints

- Zero new third-party dependencies in `Cargo.toml` / `Cargo.lock` (only `serde_json.workspace = true` wired into `crates/mehh-cli/Cargo.toml`).
- `mehh-cli` production code (`--lib --bins`) is strictly checked under `-D clippy::unwrap_used -D clippy::panic`.
- All created/modified files must be UTF-8 without BOM.

## Approaches Considered

- **Approach A (Real binary E2E for CLI/RPC + `MockTerminal` for TUI — Selected):** Runs the actual `mehh` binary via `CARGO_BIN_EXE_mehh` for CLI, print, RPC, session, and permission flows, and drives `render_interactive_frame` + `TuiRenderer` + `MockTerminal` for TUI frame verification. Zero new dependencies, fast, and deterministic across Windows/Linux/macOS CI.
- **Approach B (Add `portable-pty` OS PTY tests):** Adds an extra external crate and risks flaky timing/ConPTY issues on Windows CI.

## What Is Left to Builder

- Keep `MEHH_FAUX_SCRIPT` minimal (`[{"text": ..., "tool_calls": [{"id": ..., "name": ..., "arguments": {...}}]}]`) and fail closed on invalid JSON or non-object tool arguments.
- Ensure every E2E test isolates `HOME`/`USERPROFILE` to a temp directory and passes `--no-mcp`.
