# Phase 2.2 Implementation Plan: Split `interactive/commands.rs` & `interactive/events.rs`

**Goal:** Split `commands.rs` (3,211 lines) and `events.rs` (3,599 lines) into focused submodules with zero behavioral change.

**Spec:** `docs/superpowers/specs/2026-11-27-mehh-interactive-split-design.md`

- [x] **Step 1: Split `commands.rs` into `commands/{mod,auth,settings_ui,autocomplete}.rs` and remove `commands.rs`**
- [x] **Step 2: Split `events.rs` into `events/{mod,signals,tasks,input}.rs` and remove `events.rs`**
- [x] **Step 3: Update `FEATURE_MAP.md` paths**
- [x] **Step 4: Run interactive unit tests, `test_tui_headless_e2e`, `test_interactive_approvals`, `workspace_guardrails`, and strict Clippy**
