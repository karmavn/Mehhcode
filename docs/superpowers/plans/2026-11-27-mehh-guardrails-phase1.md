# Mehhcode Phase 1 Hard Guardrails & CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`/`- [ ]`) syntax for tracking.

**Goal:** Enforce compile-time Clippy guardrails, 5-layer crate boundaries, UTF-8 no-BOM hygiene, CI dependency checks, and accurate Rust-only `AGENTS.md` guidance across the Mehhcode workspace without disturbing existing dirty `.rs` work.

**Architecture:** Inherit 4 all-targets-safe deny lints (`dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`) uniformly across all 10 crates via `[workspace.lints.clippy]` and `[lints] workspace = true`, enforce production `-D clippy::unwrap_used -D clippy::panic` on `--lib --bins` in CI for the 7 strict crates (`-D clippy::panic` on `--lib` for the 3 legacy crates), pair with a zero-dependency integration test in `mehh-cli`, and run `cargo-deny` in GitHub Actions.

**Tech Stack:** Rust (Edition 2021, `rustc 1.99.1`, `clippy 0.1.99`), Cargo workspace lints, `cargo-deny` (CI), GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-27-mehh-guardrails-phase1-design.md`

## Global Constraints

- Preserve all existing uncommitted changes in `Cargo.toml`, `.github/workflows/ci.yml`, `AGENTS.md`, and the 115 modified `.rs` files; do not run `cargo fmt --all` or touch unrelated `.rs` files.
- Local machine lacks `cargo-deny`, `cargo-nextest`, and `cargo-machete`; `cargo-deny` runs in GitHub Actions only.
- PowerShell 5.1 shell: chain commands with `;`, never `&&`.
- Run targeted Cargo checks only (`cargo check -p`, `cargo test -p`, `cargo clippy -p ... --no-deps`); never run `cargo build --release`.
- All created and modified files must be UTF-8 without BOM.

## Review Focus

- Upward or cross-layer dependencies hidden inside `[target.'cfg(windows)'.dependencies]` rather than `[dependencies]` must still be caught by `parse_internal_deps`, while `[dev-dependencies]` remain ignored.
- Unlisted new crates added under `crates/` must fail closed rather than silently passing the layer boundary check.
- Integration test helpers in `crates/*/tests/*.rs` must not break under `--all-targets` while production `--lib --bins` still hard-denies `unwrap_used` and `panic`.
- Stale TypeScript references in `AGENTS.md` and missing CI production guards in `.github/workflows/ci.yml` must fail the guardrail test before those files are updated (RED -> GREEN).
- `deny.toml` must accept every SPDX license currently present in `Cargo.lock` without unverified exceptions.

---

### Task 1: Workspace Clippy Lints, `clippy.toml`, and 2 Surgical Fixes

**Files:**
- Create: `clippy.toml`
- Modify: `Cargo.toml`
- Modify: `crates/*/Cargo.toml` (all 10 member crates)
- Modify: `crates/mehh-coding-agent/src/clipboard.rs:158-167`
- Modify: `crates/mehh-coding-agent/src/rpc.rs:2483-2488`

- [x] **Step 1: Create `clippy.toml` with test-exemption flags**
- [x] **Step 2: Add `[workspace.lints.clippy]` (`dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`) to root `Cargo.toml` and `[lints] workspace = true` to all 10 member `Cargo.toml` files**
- [x] **Step 3: Fix `clipboard.rs:158-167` (`unwrap_err`) and `rpc.rs:2483-2488` (`await_holding_lock` in test)**
- [x] **Step 4: Run targeted Clippy and unit tests across all 10 crates to verify zero deny errors**

---

### Task 2: Workspace Architecture & Hygiene Guardrails Test (TDD)

**Files:**
- Create: `crates/mehh-cli/tests/workspace_guardrails.rs`

- [x] **Step 1: Write `crates/mehh-cli/tests/workspace_guardrails.rs` with unit + integration tests**
- [x] **Step 2: Run `cargo test -p mehh-cli --test workspace_guardrails` to verify RED**
  - Observed RED: `agents_md_has_no_stale_typescript_workspace_references` and `ci_enforces_production_unwrap_and_panic_guard` FAILED; remaining 7 tests PASSED.

---

### Task 3: `deny.toml` and GitHub Actions `deny` + Production Clippy Guard

**Files:**
- Create: `deny.toml`
- Modify: `.github/workflows/ci.yml`

- [x] **Step 1: Inspect `cargo metadata` SPDX licenses (zero license-less crates found)**
- [x] **Step 2: Create `deny.toml` allowing exact SPDX licenses from `Cargo.lock` and denying unknown registries/git/wildcards**
- [x] **Step 3: Add production `--lib --bins` unwrap/panic Clippy step and `deny` job to `.github/workflows/ci.yml` without altering existing `ci` job steps**

---

### Task 4: Clean `AGENTS.md` and Final Verification

**Files:**
- Modify: `AGENTS.md`

- [x] **Step 1: Remove stale TypeScript / `packages/` rules from `AGENTS.md`, update keybindings path to `crates/mehh-tui/src/keys.rs`, document Phase 1 guardrails, and update `## Workspace Map` to the 10 Rust crates**
- [x] **Step 2: Run final verification suite (GREEN)**
