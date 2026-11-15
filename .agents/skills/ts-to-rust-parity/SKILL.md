---
name: ts-to-rust-parity
description: Use when porting features from Pi TypeScript reference to Rust crates, auditing crates/mehh-* against packages/*, or verifying TS-to-Rust behavioral parity before or after implementation.
---

# TS-to-Rust Parity

## Overview

Porting from TypeScript (`packages/*`) to Rust (`crates/mehh-*`) must preserve **behavioral and caller parity**, not just core happy paths. Every port requires full source, caller, test, and target-crate inspection before writing Rust code.

**Violating the letter of these steps is violating the spirit of parity.**

## Package-to-Crate Mapping

| TypeScript (`packages/`) | Rust (`crates/`) |
|---|---|
| `ai` | `mehh-ai` |
| `agent` | `mehh-agent` |
| `coding-agent` | `mehh-coding-agent`, `mehh-tools`, `mehh-cli` |
| `tui` | `mehh-tui` |
| `mcp` | `mehh-mcp` |
| `codemode` | `mehh-codemode` |
| `schema` | `mehh-ai/src/validation.rs` (serde + schema checks) |
| `telemetry` | `tracing` workspace wiring |

If `packages/` is absent from the workspace, ask for or locate the TS reference checkout before guessing TS behavior.

## Required Porting Contract (In Order)

Do not skip steps, even under time pressure:

1. **Read TS Source + Callers + Tests in Full**
   - Read target TS modules completely (`packages/<pkg>/src/...`).
   - Grep all TS callers across `packages/` to capture callbacks, options, and post-processing calls.
   - Read matching TS tests (`packages/<pkg>/test/...`) to extract edge cases and issue regressions.

2. **Audit Target Rust Crates Before Designing**
   - Read target Rust modules and grep for existing helpers or partial implementations to **reuse or replace** (never add a parallel helper alongside an existing one).
   - Map every Rust caller across `crates/` that must invoke the ported code.

3. **Write a Parity Matrix**
   Classify every behavior, callback, error case, and caller:
   - **Port 1:1** — exact Rust contract, edge cases, and caller wiring.
   - **Adapt to Rust** — idiomatic mapping (`Result`/`?`, `enum`/struct) preserving observable behavior.
   - **Deliberately Omitted** — JS/TS-only mechanics (dynamic `import()`, TS type-level builders) or explicit non-goals. Never silently drop an edge case.

4. **Implement with `AGENTS.md` Discipline**
   - Propagate errors (`Result`/`?`); no `.ok()` or `unwrap_or_default()` swallowing internal failures.
   - Fail closed on trust, path, and config checks (`ENOENT` vs syntax/schema errors).
   - Wire all mapped Rust callers and remove superseded inline workarounds.

5. **Port Tests & Verify After Final Edit**
   - Port TS test cases into `cargo test` (`RED` -> `GREEN` for new tests/fixes).
   - Run `cargo check --workspace`, `cargo clippy -p <crate> --all-targets`, and `cargo test -p <crate>` after the final edit.

## Rationalizations & Red Flags

| Excuse | Reality |
|---|---|
| "I'll port the core function first and wire callers later" | Unwired code gives false parity. Map and wire callers together. |
| "Reading the main `.ts` file is enough" | Callers supply critical callbacks; `packages/*/test/` holds edge cases. |
| "I'll add a fresh helper without grepping `crates/`" | Parallel helpers violate `AGENTS.md` and drift on bugfixes. |
| "`unwrap_or_default()` keeps Rust clean" | Masks parity bugs and violates fail-closed error propagation. |

**STOP if you catch yourself:** guessing TS caller callbacks, skipping `packages/*/test/`, inventing parallel Rust helpers without grepping `crates/`, or silently dropping edge cases.
