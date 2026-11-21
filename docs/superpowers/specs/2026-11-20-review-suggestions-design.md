# Review follow-ups design (2026-11-20)

## Intent

The independent reviewer passed the current patch set (`passed=true`, no security or
logic errors) but left four non-blocking suggestions. This spec turns those four into
work that can be done tomorrow without re-deriving the context.

Source of truth for each item is the verified finding, not the reviewer's wording:

1. `compaction.rs:534-541` — `compact_with_provider` keeps the head of a chronological
   transcript and drops the tail, while its marker claims older history was truncated.
2. `interactive.rs:2497-2503` — `expand_prompt_with_files` uses `split_whitespace()` and
   rejoins with `" "`, flattening line breaks and breaking `@path` values with spaces.
3. `main.rs:1011-1013` — piped stdin is read whenever stdin is not a terminal, even when
   a prompt was supplied; `read_to_string` waits for EOF. HEAD only read stdin when the
   prompt was empty.
4. `file_args.rs:79` — `path_str` is interpolated into `<file name="...">` unescaped.

## Goals

- Summary prompt keeps the most recent history, and its marker matches what was cut.
- TUI prompts preserve exact whitespace, and quoted `@"path with spaces"` works.
- Print mode never blocks on an open stdin pipe when a prompt argument already exists.
- File-name markup is escaped before it enters the LLM prompt.

## Non-goals

- No new CLI flag for combining piped stdin with a prompt argument.
- No refactor of `interactive.rs` into a module directory in this batch.
- No changes to the already-passing MOIM, Lead/Worker, MCP sync, or Windows URI fixes.
- No commit unless explicitly requested later.

## Constraints

- Rust workspace; run debug tests only (`cargo test -p ... --lib`), no release builds.
- TDD: every fix gets a failing test first, then minimal code, then a green run.
- Do not touch unrelated uncommitted work from other sessions, especially the stdin
  region of `crates/mehh-cli/src/main.rs`; confirm ownership before editing it.
- Reuse `crate::process_file_arguments`; do not duplicate file-read logic in `interactive.rs`.
- Keep the existing public signatures of `compact_with_provider`,
  `expand_prompt_with_files`, `process_file_arguments`, and `run_print_flow` unchanged.
