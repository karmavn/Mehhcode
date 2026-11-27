---
name: mehh-verify
description: >-
  Runs Mehhcode's offline verification workflow using FEATURE_MAP.md,
  MEHH_FAUX_SCRIPT binary E2E tests, MockTerminal headless TUI tests,
  workspace_guardrails, and isolated --no-deps Clippy checks. Use after editing
  CLI, RPC, permission, TUI, or crate-boundary code in Mehhcode.
---

# Mehhcode Offline Verification

Verify Mehhcode changes end-to-end without real LLM API keys or modifying `~/.mehh/`.

## Steps

1. **Locate affected targets:** Check `FEATURE_MAP.md` first for the owning source files and targeted `cargo test` commands before grepping across `crates/`. After moving or splitting a module, verify each `FEATURE_MAP.md` row names the actual owning file (not merely an existing path).
2. **Verify CLI, print, RPC, session, and permission behavior (real binary):**
   ```powershell
   cargo test -p mehh-cli --test e2e_cli_rpc
   ```
   - Uses `MEHH_FAUX_SCRIPT` and isolated temp `HOME`/`USERPROFILE` + `--no-mcp`.
3. **Verify TUI layout, slash commands, and resize bounds (headless):**
   ```powershell
   cargo test -p mehh-coding-agent --test test_tui_headless_e2e
   ```
   - Checks `lines.len() == height`, `visible_width(&strip_ansi(line)) <= width`, and `show_hardware_cursor = false`.
4. **Verify architecture boundaries, hygiene, and strict lints:**
   ```powershell
   cargo test -p mehh-cli --test workspace_guardrails
   cargo clippy -p <changed-crate> --no-deps --all-targets -- -D warnings
   cargo clippy --locked --workspace --no-deps --lib --bins -- -D clippy::unwrap_used -D clippy::panic
   ```
   - On Windows PowerShell 5.1, chain commands with `;`, never `&&`.
   - If the user explicitly asks to skip test/build runs (e.g., static-only code review), do not invoke `cargo` and inspect code/guardrails statically instead.
