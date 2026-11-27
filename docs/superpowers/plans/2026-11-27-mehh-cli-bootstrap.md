# Phase 2.1 Implementation Plan: Unified CLI Session Bootstrap

**Goal:** Extract CLI session assembly out of `crates/mehh-cli/src/main.rs` into `crates/mehh-cli/src/bootstrap.rs` with unit tests covering all 4 `CliMode` variants.

**Spec:** `docs/superpowers/specs/2026-11-27-mehh-cli-bootstrap-design.md`

- [x] **Step 1: Write `crates/mehh-cli/src/bootstrap.rs` with `BootstrappedSession`, `bootstrap_session`, pure helpers, and isolated unit tests**
- [x] **Step 2: Wire `crates/mehh-cli/src/main.rs` (`mod bootstrap;`) to call `bootstrap::bootstrap_session` and `bootstrap::close_mcp`**
- [x] **Step 3: Update `FEATURE_MAP.md` with `crates/mehh-cli/src/bootstrap.rs`**
- [x] **Step 4: Verify `cargo test -p mehh-cli` and strict `--no-deps` Clippy checks**
