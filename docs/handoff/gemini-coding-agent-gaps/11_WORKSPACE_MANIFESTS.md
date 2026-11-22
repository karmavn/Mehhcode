# Dump 11 - Workspace manifests va AGENTS.md

> Code trong fence 5-backtick la byte-for-byte tu file goc. Hash SHA-256 tin tren byte goc.

| path | KB | dong | SHA-256 |
|---|---:|---:|---|
| Cargo.toml | 1.4 | 52 | fbe87140b96746ad355af4d6411eb12c546335f31ace0939f5530892ea05f6c7 |
| AGENTS.md | 9.8 | 107 | 92d77897f6b17c9830d7561d4e8f5ebb03e324649b1930ebb2ded315a8b38020 |
| crates/mehh-agent/Cargo.toml | 0.3 | 14 | bee17d625ee78e60bff832b8a22bbb930c1819cdc5ce7fa9f5fd2f8788614b87 |
| crates/mehh-ai/Cargo.toml | 0.5 | 21 | d82b34515ec72ad58febf556721a0b56baade82df209c9b4c2a4f894e57ae606 |
| crates/mehh-cli/Cargo.toml | 0.4 | 18 | 3b075355abebc15329c08c15caa97cc923b29b2d89b93c5e767ee6e9d6e7d18f |
| crates/mehh-codemode/Cargo.toml | 0.4 | 18 | ed1a5dd4cf7beb205656b5cfe212edeac9975909c310fd2f71a3f5831291bb51 |
| crates/mehh-coding-agent/Cargo.toml | 0.8 | 28 | 8209cd8e1d43f320afbd07ee57932a8943441a591e5b7a64cd4980ffefde920b |
| crates/mehh-execpolicy/Cargo.toml | 0.1 | 7 | 99810f31c56d5dc81d3e5af0994d513e6eabcab8d529195e90fed44bc6711184 |
| crates/mehh-mcp/Cargo.toml | 0.6 | 21 | a1c9cd26da3e040e82251e9628f7f613f1ce34d075f071bd51073ca35dfd2bd3 |
| crates/mehh-sandbox/Cargo.toml | 0.1 | 7 | 628893f69d7631cc968e75494df0bba1b1c60bdf33918748c13c73bf58db7e41 |
| crates/mehh-tools/Cargo.toml | 0.6 | 21 | e6c1f860f451ac2b8dd63a8845282b303bcc1bca33d1fb497b90d548e3b8b4eb |
| crates/mehh-tui/Cargo.toml | 0.3 | 13 | fe6461cbcaeea6f3400f957bbcd9635750294227cf8569aa5d9d92f672f4949a |

## Cargo.toml

`````toml
[workspace]
resolver = "2"
members = [
    "crates/mehh-ai",
    "crates/mehh-agent",
    "crates/mehh-tools",
    "crates/mehh-coding-agent",
    "crates/mehh-tui",
    "crates/mehh-mcp",
    "crates/mehh-codemode",
    "crates/mehh-execpolicy",
    "crates/mehh-sandbox",
    "crates/mehh-cli",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
futures = "0.3"
async-trait = "0.1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
anyhow = "1"
uuid = { version = "1", features = ["v4", "v7", "serde"] }
base64 = "0.22"
sha2 = "0.10"
regex = "1"
globset = "0.4"
ignore = "0.4"
similar = "2"
clap = { version = "4", features = ["derive", "env"] }
crossterm = "0.28"
unicode-width = "0.2"
unicode-segmentation = "1"
tracing = "0.1"

mehh-ai = { path = "crates/mehh-ai" }
mehh-agent = { path = "crates/mehh-agent" }
mehh-tools = { path = "crates/mehh-tools" }
mehh-coding-agent = { path = "crates/mehh-coding-agent" }
mehh-tui = { path = "crates/mehh-tui" }
mehh-mcp = { path = "crates/mehh-mcp" }
mehh-codemode = { path = "crates/mehh-codemode" }
mehh-execpolicy = { path = "crates/mehh-execpolicy" }
mehh-sandbox = { path = "crates/mehh-sandbox" }
`````

## AGENTS.md

`````markdown
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
- Never report "verified" or "done" without running the relevant checks after the final edit and citing the exact command output.

## Commands

- Rust workspace (`crates/*`, runnable directly): `cargo check`, `cargo clippy`, `cargo test -p <crate>`.
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
`````

## crates/mehh-agent/Cargo.toml

`````toml
[package]
name = "mehh-agent"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
mehh-ai = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
futures = { workspace = true }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
`````

## crates/mehh-ai/Cargo.toml

`````toml
[package]
name = "mehh-ai"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
tokio = { workspace = true }
tokio-util = { workspace = true }
futures = { workspace = true }
async-trait = { workspace = true }
reqwest = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
uuid = { workspace = true }
base64 = { workspace = true }
sha2 = { workspace = true }
hmac = "0.12"
crc32fast = "1"
regex = { workspace = true }
`````

## crates/mehh-cli/Cargo.toml

`````toml
[package]
name = "mehh-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "mehh"
path = "src/main.rs"

[dependencies]
mehh-ai.workspace = true
mehh-agent.workspace = true
mehh-tools.workspace = true
mehh-coding-agent.workspace = true
tokio.workspace = true
clap.workspace = true
anyhow.workspace = true
`````

## crates/mehh-codemode/Cargo.toml

`````toml
[package]
name = "mehh-codemode"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
mehh-ai.workspace = true
mehh-agent.workspace = true
tokio.workspace = true
tokio-util.workspace = true
futures.workspace = true
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
rquickjs = "0.9"
base64.workspace = true
`````

## crates/mehh-coding-agent/Cargo.toml

`````toml
[package]
name = "mehh-coding-agent"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
mehh-ai = { workspace = true }
mehh-agent = { workspace = true }
mehh-tools = { workspace = true }
mehh-tui = { workspace = true }
mehh-mcp = { workspace = true }
mehh-codemode = { workspace = true }
mehh-execpolicy = { workspace = true }
crossterm = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
futures = { workspace = true }
async-trait = { workspace = true }
reqwest = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
uuid = { workspace = true }
sha2 = { workspace = true }
ignore = { workspace = true }
regex = { workspace = true }
globset = { workspace = true }
`````

## crates/mehh-execpolicy/Cargo.toml

`````toml
[package]
name = "mehh-execpolicy"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
`````

## crates/mehh-mcp/Cargo.toml

`````toml
[package]
name = "mehh-mcp"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
mehh-ai = { workspace = true }
mehh-agent = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
futures = { workspace = true }
async-trait = { workspace = true }
reqwest = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
sha2 = { workspace = true }
tracing = { workspace = true }
base64 = { workspace = true }
uuid = { workspace = true }
`````

## crates/mehh-sandbox/Cargo.toml

`````toml
[package]
name = "mehh-sandbox"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
`````

## crates/mehh-tools/Cargo.toml

`````toml
[package]
name = "mehh-tools"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
mehh-ai = { workspace = true }
mehh-agent = { workspace = true }
mehh-sandbox = { workspace = true }
anyhow = { workspace = true }
tokio = { workspace = true }
tokio-util = { workspace = true }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
base64 = { workspace = true }
regex = { workspace = true }
globset = { workspace = true }
ignore = { workspace = true }
similar = { workspace = true }
`````

## crates/mehh-tui/Cargo.toml

`````toml
[package]
name = "mehh-tui"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
crossterm = { workspace = true }
unicode-width = { workspace = true }
unicode-segmentation = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
pulldown-cmark = "0.12"
`````

