# Development Rules

## Conversational Style

- Keep answers short and concise
- No emojis in commits, issues, PR comments, or code
- No fluff or cheerful filler text (e.g., "Thanks @user" not "Thanks so much @user!")
- Technical prose only, be direct
- Use concise, clear, simple language. Define unavoidable jargon before using it.
- Explain non-trivial designs and problems as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- Prefer concrete behavior and small illustrations over abstract summaries, dense terminology, or unexplained lists of changes.
- When the user asks a question, answer it first before making edits or running implementation commands.
- When responding to user feedback or an analysis, explicitly say whether you agree or disagree before saying what you changed.

## Code Quality

- Read files in full before wide-ranging changes, before editing files you have not fully inspected, and when asked to investigate or audit. Do not rely on search snippets for broad changes.
- No `any` unless absolutely necessary. Do not lower the gate: `@ts-ignore` is banned, `@ts-expect-error` requires a reason on the same line, no `as any` to silence `tsc`, and no disabling lint rules.
- Validate unknown input once at decided trust boundaries (CLI args, wire/API payloads, schema checks, I/O) and pass typed values inward. No runtime checks of values whose types TypeScript already guarantees (`typeof`/`instanceof`/`Array.isArray`/shape guards on typed parameters, typed callback results, or typed declarations).
- Security, permission, and path checks fail closed, never open. Distinguish `ENOENT` from syntax/schema errors when loading config (strip BOM); never silently discard permission or trust rules into empty defaults.
- Avoid defensive error swallowing around internal logic: let errors propagate (`throw` in TS, `Result`/`?` in Rust) to boundary handlers instead of returning `null`, `{}`, `.ok()`, or `unwrap_or_default()` without a local recovery strategy.
- Inline single-line helpers that have only one call site unless hiding a complex boundary. Reuse existing helpers rather than adding a parallel mechanism.
- Check `node_modules` for external API types; don't guess.
- **No inline imports** (`await import()`, `import("pkg").Type`, dynamic type imports). Top-level imports only.
- Use only erasable TypeScript syntax (`erasableSyntaxOnly` in `tsconfig.base.json`): no parameter properties, `enum`, `namespace`/`module`, `import =`, `export =`, or other constructs needing JS emit. Use explicit fields with constructor assignments.
- Never remove or downgrade code to fix type errors from outdated deps; upgrade the dep instead.
- Always ask before removing functionality or code that appears intentional.
- Do not preserve backward compatibility unless the user asks for it.
- In `packages/coding-agent`, resolve package assets through helpers in `src/config.ts`. Do not use `__dirname` directly; the helpers account for source checkouts, npm installations, and standalone binaries.
- Never hardcode key checks (e.g. `matchesKey(keyData, "ctrl+x")`). Register defaults in `TUI_KEYBINDINGS` (`packages/tui/src/keybindings.ts`) or `APP_KEYBINDINGS` (`packages/coding-agent/src/core/keybindings.ts`) so they stay configurable.
- Never modify `packages/ai/src/models.generated.ts` directly; update `packages/ai/catalog/` instead, then regenerate (note: `npm run generate-models` currently requires restoring `packages/ai/scripts/model-data.ts`). Generated data under `packages/ai/src/providers/data/` is gitignored: never stage it or look for it in git history.

## Contracts, Keys, and Dead Code

- Before changing a shared function's signature, return type, or behavior, grep all callers across both `packages/` and `crates/` and fix it in the shared place rather than patching callers piecemeal.
- Never key caches, guards, or maps on raw user- or model-supplied strings (paths, session IDs, config keys). Normalize first (`path.resolve` plus lower-casing on case-insensitive filesystems such as Windows).
- Dead code deletion:
  - Feature / bugfix turns (surgical): only delete dead code created by your own changes or inside the immediate modified unit. Mention unrelated dead code in your report instead of expanding diff scope.
  - Before deleting any symbol or file, prove it is unused: (1) search a symbol that definitely exists to verify the search tool works, (2) never treat empty output as proof if the command printed errors, (3) use a real recursive tool (`git grep`, `Get-ChildItem -Recurse | Select-String`, or `rg`/`grep` when installed; PowerShell `**` is unreliable), and (4) scan the full tree (`packages/`, `crates/`, tests, configs, string references).

## Testing and Anti-Thrashing

- Prove new bugfix or feature tests fail (`RED`) before applying the fix, or verify that breaking the guard turns the test red.
- Keep test logic flat and avoid tautological assertions: assert on observable state or output rather than trivial mocks, anchor regex patterns, and verify cut/slice bounds (`idx > -1`).
- Aim edits at root causes. If 2 consecutive fix attempts fail, stop and report the roadblock instead of churning files.
- Never report "verified" or "done" without running the relevant checks after the final edit and citing the exact command output. For Rust changes, verify locally with targeted tests (`cargo test -p <crate>`) or `cargo check -p <crate>`. Full release builds and release binary generation (`mehh.exe`) are delegated to GitHub Actions (`.github/workflows/ci.yml`).

## Commands

- **Rust workspace (`crates/*`):**
  - **Local testing & check:** Targeted commands only: `cargo check -p <crate>`, `cargo test -p <crate> <test_name>`.
  - **Heavy builds & release:** Do NOT run full-workspace or release builds (`cargo build --release`) locally on low-resource machines. Windows release binary builds (`mehh.exe`) and full CI checks are handled by GitHub Actions (`.github/workflows/ci.yml`).
- `packages/tui` (`node:test`, run from `packages/tui` for tests without unhydrated npm deps): `node --test test/specific.test.ts`.
- TypeScript checks requiring hydrated `node_modules` (currently absent until workspace package naming/lockfile setup is finished):
  - Typecheck: `npx tsc --noEmit` (uses root `tsconfig.json`).
  - Vitest (`packages/*` except `tui`, run from package root): `node "$(git rev-parse --show-toplevel)/node_modules/vitest/dist/cli.js" --run test/specific.test.ts`.
  - Lint/format: do not run `npx biome check` until `biome.json` is restored (running without it uses Biome defaults that conflict with tab indentation and repo rules).
  - Build: `npm run build` currently fails while `scripts/` helpers are absent from the checkout; use `tsc -p tsconfig.build.json` inside standalone packages when `node_modules` is present.
- Never run `npm test` (or full workspace test suites) unless requested by the user. Run targeted tests for modified files and iterate until they pass.
- For `packages/coding-agent/test/suite/`, use `test/suite/harness.ts` + the faux provider. No real provider APIs, keys, or paid tokens.
- For GitHub issue regression tests, add a comment with the issue number next to the test.
- For ad-hoc scripts, write them to a temp file, run, and remove when done; do not embed multi-line scripts in shell commands.
- Never commit unless the user asks.

## File Hygiene (Windows / UTF-8)

- Source files must be UTF-8 without BOM (`0xEF 0xBB 0xBF`). Never use default PowerShell `Out-File` or `Set-Content` (both inject BOM).
- After bulk edits, run `git diff --stat` and check for files rewritten wholesale without real content changes.

## Dependency and Install Security

- Treat npm/Cargo dep and lockfile changes as reviewed code. Direct npm registry deps, including internal workspace deps, stay pinned to exact versions (`Cargo.toml` uses workspace version ranges).
- When updating `undici`, read its changelog/release notes for the target version and evaluate whether any changes affect functionality before applying the update.
- When running npm installs, hydrate locally with `npm install --ignore-scripts` and clean/CI-style with `npm ci --ignore-scripts` (note: lockfiles are currently absent and `@earendil-works/pi-*` vs `@mehh-code/*` workspace package names are mid-migration, so `npm ci` and `--package-lock-only` refresh apply once a lockfile and aligned package names are in place). Don't run lifecycle scripts unless the user asks.

## Git, Issues, and PRs

Multiple agent sessions may run in this workspace simultaneously, each modifying different files. Git commands that touch files outside your changes can destroy other sessions' work.

Committing:

- Only commit files YOU changed in THIS session.
- Stage explicit paths (`git add <path1> <path2>`); never `git add -A` / `git add .`.
- Before committing, run `git status` and verify you are only staging your files (and that new files were read before staging).
- `packages/ai/src/models.generated.ts` may always be included alongside your files when regenerating the model catalog.
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

- TypeScript packages (`packages/`): `ai`, `agent`, `coding-agent`, `tui`, `mcp`, `codemode`, `schema`, `telemetry`
- Rust crates (`crates/`): `mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-tui`, `mehh-mcp`, `mehh-codemode`, `mehh-cli`

## User Override

If the user's instructions conflict with any rule in this document, ask for explicit confirmation before overriding. Only then execute their instructions.
