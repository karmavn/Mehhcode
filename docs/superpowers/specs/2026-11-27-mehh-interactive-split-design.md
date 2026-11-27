# Phase 2.2 Design: Split `interactive/commands.rs` & `interactive/events.rs`

## Intent & Why

`crates/mehh-coding-agent/src/interactive/commands.rs` (3,211 lines) and `events.rs` (3,599 lines) were the top-churned files in the repository and exceeded comfortable single-read context size, causing AI edits in one corner to miss state transitions elsewhere. Splitting them into focused submodules (`commands/{mod,auth,settings_ui,autocomplete}.rs` and `events/{mod,signals,tasks,input}.rs`) while preserving all exports in `commands/mod.rs` and `events/mod.rs` isolates each concern with zero behavior change.

## Goals & Non-Goals

- **Goals:**
  - Extract `commands/{auth,settings_ui,autocomplete}.rs` + `commands/mod.rs`.
  - Extract `events/{signals,tasks,input}.rs` + `events/mod.rs`.
  - Update `FEATURE_MAP.md` to point directly to the new module paths.
  - Pass all interactive tests, headless TUI E2E tests, `workspace_guardrails`, and `-- -D warnings` / strict production Clippy checks.
- **Non-Goals:**
  - Changing any interactive behavior, keybindings, or public APIs in `interactive/mod.rs` or `interactive/tests.rs`.
  - Splitting `agent_session.rs`, `session.rs`, `subagent.rs`, or `rpc.rs` (deferred to Phase 2.3+).
