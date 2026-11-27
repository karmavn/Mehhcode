# Development Rules

## Conversational Style

- Keep answers short and concise
- No emojis in commits, issues, PR comments, or code
- No fluff or cheerful filler text (e.g., "Thanks @user" not "Thanks so much @user!")
- Technical prose only, be direct
- When proposing choices or trade-offs, lead with one recommended option in plain user-impact terms (what improves or breaks) before technical details.
- Use concise, clear, simple language. Define unavoidable jargon before using it.
- Explain non-trivial designs and problems as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- Prefer concrete behavior and small illustrations over abstract summaries, dense terminology, or unexplained lists of changes.
- When the user asks a question, answer it first before making edits or running implementation commands.
- When responding to user feedback or an analysis, explicitly say whether you agree or disagree before saying what you changed.

## Code Quality

- Read files in full before wide-ranging changes, before editing files you have not fully inspected, and when asked to investigate or audit. Do not rely on search snippets for broad changes.
- Do not lower lint or type gates, or silence warnings without an explicit reason.
- Validate unknown input once at decided trust boundaries (CLI args, wire/API payloads, schema checks, I/O) and pass typed values inward.
- Security, permission, and path checks fail closed, never open. Distinguish `ENOENT` from syntax/schema errors when loading config (strip BOM); never silently discard permission or trust rules into empty defaults.
- Avoid defensive error swallowing around internal logic: let errors propagate (`Result`/`?` in Rust) to boundary handlers instead of returning `null`, `{}`, `.ok()`, or `unwrap_or_default()` without a local recovery strategy.
- Inline single-line helpers that have only one call site unless hiding a complex boundary. Reuse existing helpers rather than adding a parallel mechanism.
- Prefer the simplest implementation that meets current requirements; avoid speculative abstractions or unsolicited refactors for hypothetical future needs.
- Never remove or downgrade code to fix type errors from outdated deps; upgrade the dep instead.
- Always ask before removing functionality or code that appears intentional.
- Do not preserve backward compatibility unless the user asks for it.
- Never hardcode raw key checks in interactive handlers. Register defaults in `TuiAction` / `Keybindings` (`crates/mehh-tui/src/keys.rs`) so they stay configurable.

## Code Review Focus

- Default code reviews are **functional-first** and prioritize real bugs users hit:
  1. Crashes, panics, hangs, deadlocks, and leaked child processes or background tasks.
  2. Logic and behavioral bugs in everyday user flows (CLI flags, interactive TUI keys/mouse/rendering, slash commands, provider streaming, tools, sessions/resume/reload, config/auth).
  3. Cross-platform breakage (Windows vs Linux/macOS path/process/env behavior).
  4. State corruption, dropped user data, or silently swallowed errors at boundaries.
- Do not spend review cycles on academic or contrived local security edge cases (e.g., millisecond local symlink races, prototype tampering, NTFS alternate data streams, theoretical Bidi spoofing) unless the user explicitly asks for a security audit.
- Review end-to-end user journeys in full context, not only security patches in the recent diff.

## Contracts, Keys, and Dead Code

- Before changing a shared function's signature, return type, or behavior, grep all callers across `crates/` and fix it in the shared place rather than patching callers piecemeal.
- Never key caches, guards, or maps on raw user- or model-supplied strings (paths, session IDs, config keys). Normalize first (`path.resolve` plus lower-casing on case-insensitive filesystems such as Windows).
- Dead code deletion:
  - Feature / bugfix turns (surgical): only delete dead code created by your own changes or inside the immediate modified unit. Mention unrelated dead code in your report instead of expanding diff scope.
  - Before deleting any symbol or file, prove it is unused: (1) search a symbol that definitely exists to verify the search tool works, (2) never treat empty output as proof if the command printed errors, (3) use a real recursive tool (`git grep`, `Get-ChildItem -Recurse | Select-String`, or `rg`/`grep` when installed; PowerShell `**` is unreliable), and (4) scan the full tree (`crates/`, tests, configs, string references).
- When splitting a monolithic module (`foo.rs` -> `foo/{mod.rs, ...}`):
  - Preserve all `pub` re-exports in `mod.rs`, import non-child modules explicitly (`crate::...` rather than relying on parent `use super::*;` globs), keep doc comments attached directly to their target items, and update `FEATURE_MAP.md` ownership entries in the same change.

## Testing and Anti-Thrashing

- Prove new bugfix or feature tests fail (`RED`) before applying the fix, or verify that breaking the guard turns the test red.
- Keep test logic flat and avoid tautological assertions: assert on observable state or output rather than trivial mocks, anchor regex patterns, and verify cut/slice bounds (`idx > -1`).
- Isolate `HOME`, `USERPROFILE`, and provider API-key env vars behind a shared crate-wide mutex (`crate::ENV_MUTEX` in `mehh-coding-agent`, `crate::bootstrap::tests::ENV_LOCK` in `mehh-cli`) in any test within that binary that mutates or reads them so host `~/.mehh/` credentials never leak and parallel tests never race.
- For `FauxProvider` / `MEHH_FAUX_SCRIPT` tests and E2E suites, execute at least one prompt turn and assert on observable state (emitted `--mode json` / RPC events, returned messages, session files, or disk side effects) rather than inspecting static config fields alone.
- For `render_interactive_frame` tests, assert `lines.len() == height`, `visible_width(&strip_ansi(line)) <= width`, and pass `show_hardware_cursor = false` to `TuiRenderer::render_frame`.
- Aim edits at root causes. If 2 consecutive fix attempts fail, stop and report the roadblock instead of churning files.
- When a proposed fix fails an existing test that asserts intentional behavior, stop and report the conflict before weakening or rewriting the test.
- Never report "verified" or "done" without running the relevant checks after the final edit and citing the exact command output. For Rust changes, verify locally with targeted tests (`cargo test -p <crate>`) or `cargo check -p <crate>`. Full release builds and release binary generation (`mehh.exe`) are delegated to GitHub Actions (`.github/workflows/ci.yml`).

## Commands

- **Rust workspace (`crates/*`):**
  - **Local testing & check:** Targeted commands only: `cargo check -p <crate>`, `cargo test -p <crate> <test_name>`.
  - **Isolated crate Clippy:** Use `cargo clippy -p <crate> --no-deps --all-targets -- -D warnings` so pre-existing lints in dependency crates do not mask target crate results.
  - **Workspace lints & production guards:** `[workspace.lints.clippy]` in `Cargo.toml` denies `dbg_macro`, `todo`, `unimplemented`, and `await_holding_lock` across all crates (`clippy.toml` permits test-only helpers). CI (`.github/workflows/ci.yml`) additionally enforces `-D clippy::unwrap_used -D clippy::panic` across all workspace crates via `cargo clippy --locked --workspace --no-deps --lib --bins -- -D clippy::unwrap_used -D clippy::panic`.
  - **CI step safety:** In `.github/workflows/ci.yml`, put each `cargo` check in its own single-command step (never multi-line `run: |`, since `windows-latest` `pwsh` masks earlier native failures) and always pass `--no-deps` on `-D clippy::unwrap_used` / `-D clippy::panic` guards so external dependency crates are not linted with strict flags.
  - **Architecture & hygiene guardrails:** `cargo test -p mehh-cli --test workspace_guardrails` enforces the 5-layer crate dependency boundaries, `[lints] workspace = true`, UTF-8 without BOM, `AGENTS.md` accuracy, and CI production guards.
  - **Dependency & license audit:** `deny.toml` is checked in CI via `cargo-deny`.
  - **Heavy builds & release:** Do NOT run full-workspace or release builds (`cargo build --release`) locally on low-resource machines. Windows release binary builds (`mehh.exe`) and full CI checks are handled by GitHub Actions (`.github/workflows/ci.yml`).
  - **Crate dependency asymmetry:** Do not assume every crate depends on `tracing` (e.g., `mehh-mcp` does, while `mehh-coding-agent` and `mehh-cli` do not); verify each crate's `Cargo.toml` before introducing logging/macro imports.
- **Windows PowerShell 5.1 shell:** Chain commands with `;` or `; if ($?) { ... }`, never `&&`.
  - `rg` may not be installed on Windows; use `git grep` or `Get-ChildItem -Recurse -Filter *.rs | Select-String` instead.
- Never run full workspace test suites locally unless requested by the user. Run targeted tests for modified files and iterate until they pass; use `mehh_ai::FauxProvider` for offline provider tests (no real API keys or paid tokens).
- For GitHub issue regression tests, add a comment with the issue number next to the test.
- For ad-hoc scripts, write them to a temp file, run, and remove when done; do not embed multi-line scripts in shell commands.
- Never commit unless the user asks.

## File Hygiene (Windows / UTF-8)

- Source files must be UTF-8 without BOM (`0xEF 0xBB 0xBF`). Never use default PowerShell `Out-File` or `Set-Content` (both inject BOM).
- After bulk edits, run `git diff --stat` and check for files rewritten wholesale without real content changes.
- On Windows, never issue parallel `edit_file` calls targeting the exact same file (causes file-lock errors); batch edits across distinct files and sequence multiple edits per file.

## Dependency and Install Security

- Treat `Cargo.toml` and `Cargo.lock` changes as reviewed code (`Cargo.toml` uses workspace version ranges).
- Before writing new utilities or adding a dependency, check existing workspace crates and declared deps in `Cargo.toml`; do not add new packages without explicit approval.

## Git, Issues, and PRs

Multiple agent sessions may run in this workspace simultaneously, each modifying different files. Git commands that touch files outside your changes can destroy other sessions' work.

Committing:

- Only commit files YOU changed in THIS session.
- Stage explicit paths (`git add <path1> <path2>`); never `git add -A` / `git add .`.
- Before committing, run `git status` and verify you are only staging your files (and that new files were read before staging).
- Message format: `{feat,fix,docs,refactor,chore}[(scope)]: <ASCII subject>` (optionally multiple lines; ASCII-only subject).
- Closing issues via commit: repeat `fixes #<n>` or `closes #<n>` per issue (`closes #1, closes #2`).

Never run:

- `git reset --hard`, `git checkout .`, `git checkout -- .`, `git restore .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`, `git commit --no-verify`, or `git push --force`.

Rebase conflicts:

- Resolve conflicts only in files you modified; if a conflict is in another file, abort and ask the user.

Reviewing PRs and posting comments:

- Do not run `gh pr checkout`, `git switch`, or switch branches unless the user explicitly asks; inspect with `gh pr view`, `gh pr diff`, `gh api`, or `git show`.
- Write issue/PR comments to a temp file and post with `gh issue/pr comment --body-file` (never multi-line markdown via `--body`).

## Workspace Map

- Feature & verification map: see `FEATURE_MAP.md` before searching across the codebase.
- Rust crates (`crates/`): `mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-tui`, `mehh-mcp`, `mehh-codemode`, `mehh-execpolicy`, `mehh-sandbox`, `mehh-cli`

## User Override

If the user's instructions conflict with any rule in this document, ask for explicit confirmation before overriding. Only then execute their instructions.
