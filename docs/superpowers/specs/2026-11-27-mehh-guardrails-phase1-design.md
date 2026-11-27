# Phase 1 Design: Mehhcode Hard Static Guardrails & CI

## Intent & Why

Mehhcode's recent commit history shows repeated follow-up `fix:` chains after feature work because key constraints (avoiding `.unwrap()`/`panic!` in production, keeping crate dependency layers clean, maintaining UTF-8 without BOM, and keeping `AGENTS.md` synced with the Rust workspace) were enforced only as soft prose rules in `AGENTS.md`. Turning those rules into compile-time lints, workspace tests, and CI checks stops AI agents from introducing silent regressions while keeping the 115 uncommitted dirty `.rs` files untouched.

## Goals, Non-Goals, and Anti-Goals

- **Goals:**
  - Enforce workspace-wide Clippy deny lints (`dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`) via `[workspace.lints.clippy]` and uniform `[lints] workspace = true` across all 10 member crates, with `clippy.toml` permitting test-only helpers.
  - Enforce production-only `-D clippy::unwrap_used -D clippy::panic` on `--lib --bins` in CI for the 7 strict crates (`mehh-sandbox`, `mehh-execpolicy`, `mehh-agent`, `mehh-mcp`, `mehh-codemode`, `mehh-coding-agent`, `mehh-cli`) and `-D clippy::panic` on `--lib` for `mehh-ai`, `mehh-tui`, `mehh-tools` (keeping `--all-targets` green across `crates/*/tests/*.rs` integration tests whose helper functions are not exempted by `clippy.toml`).
  - Lock the 5-layer internal crate dependency boundaries, `[lints] workspace = true` wiring, UTF-8 without BOM hygiene, CI production guard presence, and `AGENTS.md` accuracy in an automated test (`crates/mehh-cli/tests/workspace_guardrails.rs`).
  - Add `deny.toml` and a `cargo-deny` job in `.github/workflows/ci.yml` for dependency advisories, licenses, bans, and sources.
  - Remove stale TypeScript / `packages/` instructions from `AGENTS.md` and document the Rust guardrails.
- **Non-Goals:**
  - Running `cargo fmt --all` or formatting the 111 unformatted `.rs` files.
  - Fixing all 51 legacy production `.unwrap()` calls in `mehh-ai`, `mehh-tui`, and `mehh-tools`.
  - Refactoring large files or changing runtime CLI/TUI behavior (deferred to Phases 2 and 3).
- **Anti-Goals:**
  - Modifying unrelated dirty `.rs` files or reverting existing uncommitted edits in `Cargo.toml`, `.github/workflows/ci.yml`, or `AGENTS.md`.
  - Requiring local installation of `cargo-deny`, `cargo-nextest`, or `cargo-machete`.

## Constraints

- Working tree contains 115 modified `.rs` files plus uncommitted changes in `Cargo.toml`, `.github/workflows/ci.yml`, and `AGENTS.md`; all existing edits must be preserved.
- Local environment runs `rustc 1.99.1`, `clippy 0.1.99`, and `rustfmt` on Windows PowerShell 5.1.
- All created and modified files must be UTF-8 without BOM.

## Approaches Considered

- **Approach A (Minimal uniform lints):** Enable only lints already at zero violations across all 10 crates (`dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`) without `unwrap_used` or `panic`. Rejected because it fails to prevent new production `.unwrap()`/`panic!` calls in core crates.
- **Approach B (Per-crate ratchet with CI production guard — Selected):** Put the 4 all-targets-safe deny lints (`dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`) in `[workspace.lints.clippy]` inherited by all 10 crates via `[lints] workspace = true`, and enforce `-D clippy::unwrap_used -D clippy::panic` on `--lib --bins` in CI for the 7 clean crates (with 2 surgical fixes in `clipboard.rs` and `rpc.rs`) plus `-D clippy::panic` on `--lib` for `mehh-ai`, `mehh-tui`, and `mehh-tools`. Locks the highest-churn production code immediately without breaking integration test helpers under `--all-targets` or touching legacy `.rs` files.
- **Approach C (Strict everywhere immediately):** Fix all 51 production `.unwrap()` calls across `mehh-ai`, `mehh-tui`, and `mehh-tools` plus all integration test helpers right away. Rejected because it mixes guardrail setup into dirty in-progress `.rs` edits.

## Architecture & 5-Layer Crate Matrix

1. **Layer 0 (Foundations — zero `mehh-*` deps):** `mehh-ai`, `mehh-execpolicy`, `mehh-sandbox`, `mehh-tui`.
2. **Layer 1 (Core agent loop):** `mehh-agent` -> `[mehh-ai]`.
3. **Layer 2 (Capabilities — no cross-deps, no `mehh-tui` or `mehh-coding-agent`):**
   - `mehh-codemode` -> `[mehh-agent, mehh-ai]`
   - `mehh-mcp` -> `[mehh-agent, mehh-ai, mehh-sandbox]`
   - `mehh-tools` -> `[mehh-agent, mehh-ai, mehh-sandbox]`
4. **Layer 3 (Session/TUI/RPC orchestration):** `mehh-coding-agent` -> `[mehh-agent, mehh-ai, mehh-codemode, mehh-execpolicy, mehh-mcp, mehh-sandbox, mehh-tools, mehh-tui]`.
5. **Layer 4 (CLI entrypoint):** `mehh-cli` -> `[mehh-agent, mehh-ai, mehh-coding-agent, mehh-tools]`.

## What Is Left to Builder

- Populate `deny.toml` `licenses.allow` directly from `cargo metadata` SPDX licenses present in `Cargo.lock`.
- Verify Clippy 0.1.99 accepts all four `clippy.toml` test-exemption keys and confirm zero `deny` errors across all 10 crates.
