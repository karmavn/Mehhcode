# Mehhcode Phase 3 Implementation Plan: FEATURE_MAP.md & Offline E2E Harness

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`/`- [ ]`) syntax for tracking.

**Goal:** Give agents a verified 1-1 `FEATURE_MAP.md` and an offline E2E test suite covering the real `mehh` binary (CLI/text/JSON/RPC/session/permissions) and headless TUI rendering via `MockTerminal`.

**Architecture:** Extend `workspace_guardrails.rs` to validate every path in `FEATURE_MAP.md`, add fail-closed `MEHH_FAUX_SCRIPT` parsing in `mehh-cli`, test the compiled `mehh` binary end-to-end in `crates/mehh-cli/tests/e2e_cli_rpc.rs`, and test interactive TUI state + `TuiRenderer` + `MockTerminal` frame invariants in `crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs`.

**Tech Stack:** Rust (Edition 2021), `std::process::Command` (`CARGO_BIN_EXE_mehh`), `mehh_ai::FauxProvider`, `mehh_tui::{MockTerminal, TuiRenderer}`, `serde_json`.

**Spec:** `docs/superpowers/specs/2026-11-27-mehh-e2e-feature-map-design.md`

## Global Constraints

- Zero new third-party crates; only reuse `serde_json.workspace = true` in `crates/mehh-cli/Cargo.toml`.
- Preserve all existing uncommitted work in the working tree; do not run `cargo fmt --all`.
- `mehh-cli` production code (`--lib --bins`) must pass `-D clippy::unwrap_used -D clippy::panic -D warnings`.
- Every E2E test must isolate `HOME` and `USERPROFILE` to a temp directory and pass `--no-mcp`.
- PowerShell 5.1 shell: chain commands with `;`, never `&&`. All created/edited files must be UTF-8 without BOM.

## Review Focus

- `extract_repo_paths_from_markdown` must skip glob patterns (`crates/*/Cargo.toml`) while validating every real file path and stripping `#L...` / `:line` suffixes.
- `parse_faux_script_json` must reject empty arrays, turns missing both text and tool calls, empty tool names, and non-object `arguments` without panicking or unwrapping.
- Binary E2E tests must strip inherited `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, and `MEHH_FAUX_SCRIPT` from the child process environment unless explicitly set by that test.
- Permission E2E test must prove `--permission-mode chat` leaves disk clean (blocks `write`) while `--permission-mode auto` writes the expected file.
- Headless TUI E2E tests must verify `lines.len() == height` and `visible_width(line) <= width` across resize widths including narrow viewports (`30x10`) and multi-byte Vietnamese text.

---

### Task 1: `FEATURE_MAP.md` & Automated Path Guard (TDD)

**Files:**
- Modify: `crates/mehh-cli/tests/workspace_guardrails.rs`
- Create: `FEATURE_MAP.md`
- Modify: `AGENTS.md`

- [x] **Step 1: Add `extract_repo_paths_from_markdown`, unit test, and `feature_map_exists_and_all_referenced_paths_exist` to `crates/mehh-cli/tests/workspace_guardrails.rs`**
- [x] **Step 2: Run `cargo test -p mehh-cli --test workspace_guardrails` and verify RED failure on `feature_map_exists_and_all_referenced_paths_exist`**
- [x] **Step 3: Create `FEATURE_MAP.md` and add pointer in `AGENTS.md` under `## Workspace Map`**
- [x] **Step 4: Run `cargo test -p mehh-cli --test workspace_guardrails` and verify GREEN**

---

### Task 2: Scripted Offline Faux Provider (`MEHH_FAUX_SCRIPT`)

**Files:**
- Modify: `crates/mehh-cli/Cargo.toml`
- Modify: `crates/mehh-cli/src/lib.rs`
- Modify: `crates/mehh-cli/src/main.rs:668-675`

- [x] **Step 1: Add `serde_json.workspace = true` to `crates/mehh-cli/Cargo.toml`**
- [x] **Step 2: Write `parse_faux_script_json` and unit tests in `crates/mehh-cli/src/lib.rs`, wire `MEHH_FAUX_SCRIPT` in `crates/mehh-cli/src/main.rs`**
- [x] **Step 3: Verify `cargo test -p mehh-cli parse_faux_script` and strict `clippy` on `mehh-cli`**

---

### Task 3: Real Binary E2E Suite (`crates/mehh-cli/tests/e2e_cli_rpc.rs`)

**Files:**
- Create: `crates/mehh-cli/tests/e2e_cli_rpc.rs`

- [x] **Step 1: Inspect `rpc.rs` command names and response payload shapes**
- [x] **Step 2: Create `crates/mehh-cli/tests/e2e_cli_rpc.rs` with 7 isolated offline binary E2E tests**
- [x] **Step 3: Run `cargo test -p mehh-cli --test e2e_cli_rpc` and verify all 7 pass**

---

### Task 4: Headless TUI Frame E2E Suite (`crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs`)

**Files:**
- Create: `crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs`
- Modify: `FEATURE_MAP.md`

- [x] **Step 1: Create `crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs` with 4 headless `MockTerminal` + `TuiRenderer` E2E tests**
- [x] **Step 2: Add both new E2E test files to `FEATURE_MAP.md`**
- [x] **Step 3: Run full final verification suite across `workspace_guardrails`, `e2e_cli_rpc`, `test_tui_headless_e2e`, and `clippy`**
