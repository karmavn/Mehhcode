# Review Suggestions Follow-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the four non-blocking suggestions left by the independent reviewer: summary truncation direction, TUI `@file` whitespace/quoted paths, stdin blocking in print mode, and unescaped file-name markup.

**Architecture:** Four small, independent fixes in three crates. Each fix adds a pure helper with a focused unit test, then rewires exactly one call site to use it, so semantic decisions are testable without a TTY, a network, or an MCP server. Task 3 touches code that belongs to another uncommitted session; confirm ownership before editing.

**Tech Stack:** Rust workspace, `tokio` async I/O, `cargo test`, `mehh_ai::UserContentBlock`.

**Spec:** `docs/superpowers/specs/2026-11-20-review-suggestions-design.md`

## Global Constraints

- No release builds on this laptop. Use `cargo test -p <crate> --lib` and `cargo check -p <crate> --tests` only.
- TDD for every fix: write the failing test, watch it fail, add minimal code, watch it pass.
- Do not commit unless the user explicitly asks. If asked, stage only the paths listed in each task.
- Do not touch unrelated uncommitted work. Before Task 3, run `git diff -- crates/mehh-cli/src/main.rs` and confirm the stdin block is still the uncommitted version described in the spec.
- Keep these signatures unchanged: `compact_with_provider`, `expand_prompt_with_files`, `process_file_arguments`, `run_print_flow`.
- Reuse `crate::process_file_arguments`; do not reimplement file reads or base64 image handling.

## Review Focus

- Multi-byte text at the clamp boundary: must not panic on a non-char-boundary; pinned by Task 1 helper test with `é`.
- One enormous message with no whitespace: clamp must still terminate and keep the newest text; pinned by Task 1 `compact_with_provider_caps_oversized_history` assertion.
- `@"my notes.txt"` with spaces and quotes: must resolve the file and keep surrounding line breaks; pinned by Task 2 quoted-path test.
- Prompt text with newlines but no `@` token: output must be byte-identical to input; pinned by Task 2 identity test.
- Piped stdin open but prompt already supplied: must not call `read_to_string`; pinned by Task 3 predicate test with a non-empty prompt.

---

### Task 1: Keep newest history in compaction summary

**Files:**
- Modify: `crates/mehh-coding-agent/src/compaction.rs:534-541` and its `#[cfg(test)] mod tests`
- Test: `cargo test -p mehh-coding-agent --lib compaction::tests`

**Interfaces:**
- Consumes: `serialize_conversation(&[Message]) -> String` (`compaction.rs:274`) with chronological order (oldest first), produced by `prepare_compaction`.
- Produces: `fn keep_newest_chars(text: &str, budget_chars: usize) -> String` — private helper in `compaction.rs`; returns `text` unchanged when `text.chars().count() <= budget_chars`, otherwise returns `"[... older history truncated for summarization]\n\n"` followed by the last `budget_chars` characters, sliced on a char boundary.

- [ ] **Step 1: Write the failing helper test**

In `compaction.rs::tests`, add `keep_newest_chars_keeps_tail_and_is_char_boundary_safe`:

```rust
let input = format!("{}{}", "old ".repeat(4000), "é".repeat(10) + "NEWEST");
let out = super::keep_newest_chars(&input, 20);
assert!(out.starts_with("[... older history truncated for summarization]\n\n"));
assert!(out.contains("NEWEST"));
assert!(!out.contains("old "));
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-coding-agent --lib keep_newest_chars_keeps_tail`
Expected: FAIL with "cannot find function `keep_newest_chars`".

- [ ] **Step 3: Implement `keep_newest_chars`**

Add the helper next to `estimate_message_tokens`. Use `text.chars().count()` for the budget check, `char_indices().nth(count - budget_chars)` to find the start of the retained tail, and `&text[start..]` for the slice. Do not change `compact_with_provider`'s signature.

- [ ] **Step 4: Run the helper test**

Run: `cargo test -p mehh-coding-agent --lib keep_newest_chars_keeps_tail`
Expected: PASS.

- [ ] **Step 5: Update the end-to-end test fixture**

In `compact_with_provider_caps_oversized_history`, replace the single `"x".repeat(200_000)` message with two messages: `Message::User` holding `"OLDEST".repeat(20_000)` followed by `Message::User` holding `"NEWEST_MARKER"`. Add assertions that the recorded prompt contains `NEWEST_MARKER` and does not contain `OLDEST`, while keeping the existing `< 20_000` length assertion.

- [ ] **Step 6: Replace the inline clamp in `compact_with_provider`**

Substitute the current truncate-at-`nth(budget_chars)` block with `let serialized_conv = keep_newest_chars(&serialize_conversation(&prep.messages_to_summarize), budget_chars);`. Keep the existing `budget_chars` calculation unchanged.

- [ ] **Step 7: Run compaction tests**

Run: `cargo test -p mehh-coding-agent --lib compaction::tests`
Expected: all PASS, including the existing `test_compact_with_provider_faux`.

- [ ] **Step 8: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/compaction.rs && git commit -m "fix(compaction): keep newest history in summary prompt"`

---

### Task 2: Preserve whitespace and quoted `@file` paths in the TUI

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2497-2503` (`expand_prompt_with_files`) and `:2757` (autocomplete item)
- Test: `cargo test -p mehh-coding-agent --lib interactive::tests`

**Interfaces:**
- Consumes: `crate::process_file_arguments(cwd: &Path, args: &[String]) -> anyhow::Result<ProcessedPromptArgs>` with fields `text: String`, `blocks: Vec<UserContentBlock>`.
- Produces: `fn file_completion_label(rel: &str) -> String` — private helper returning `@{rel}` unchanged unless `rel` contains whitespace, then `@"{rel}"`.
- Produces: `expand_prompt_with_files(cwd: &Path, text: &str) -> anyhow::Result<(String, Vec<UserContentBlock>)>` (same signature as today) — output text keeps all characters outside file tokens exactly; each file token is replaced by that file's `ProcessedPromptArgs.text`; returned blocks contain only non-`Text` blocks (images). `start_turn` already inserts one `Text` block for the assembled text.

- [ ] **Step 1: Write the identity failing test**

Add `expand_prompt_preserves_plain_text_verbatim`:

```rust
let input = "first line\n\n  second  line";
let (out, blocks) = expand_prompt_with_files(Path::new("."), input).unwrap();
assert_eq!(out, input);
assert!(blocks.is_empty());
```

- [ ] **Step 2: Write the quoted-path failing test**

Add `expand_prompt_resolves_quoted_path_with_spaces`: create a temp dir, write `my notes.txt` containing `BODY`, then call with `"keep\n\n@\"my notes.txt\" tail"`. Assert `out.contains("keep\n\n")`, `out.contains("<file name=\"my notes.txt\">")`, `out.contains("BODY")`, `!out.contains("@\"")`, and `blocks.iter().all(|b| !matches!(b, UserContentBlock::Text(_)))`.

- [ ] **Step 3: Write the autocomplete-label failing test**

Add `file_completion_label_quotes_whitespace`: `assert_eq!(file_completion_label("src/a.rs"), "@src/a.rs")` and `assert_eq!(file_completion_label("my notes.txt"), "@\"my notes.txt\"")`.

- [ ] **Step 4: Run the three tests and watch them fail**

Run: `cargo test -p mehh-coding-agent --lib expand_prompt` then `cargo test -p mehh-coding-agent --lib file_completion_label`
Expected: FAIL with missing function / wrong output (flattened newlines).

- [ ] **Step 5: Add `file_completion_label` and use it at `interactive.rs:2757`**

Replace `format!("@{rel}")` with `file_completion_label(&rel)`. Keep the label (second argument to `SelectItem::new`) as the original `rel`.

- [ ] **Step 6: Rewrite `expand_prompt_with_files`**

Scan with `char_indices`. A file token starts at `@` when it is first in the text or preceded by whitespace. If the next character is `"` or `'`, read to the matching quote; otherwise read until whitespace. Append non-token spans with `&text[start..end]` unchanged. For each token, call `process_file_arguments(cwd, &[format!("@{path}")])?`, push only non-`Text` blocks into the result, and append `processed.text` to the output. Keep the current early return when no token starts with `@`.

- [ ] **Step 7: Run the three tests**

Run: `cargo test -p mehh-coding-agent --lib expand_prompt` then `cargo test -p mehh-coding-agent --lib file_completion_label`
Expected: all PASS.

- [ ] **Step 8: Run the full crate suite**

Run: `cargo test -p mehh-coding-agent --lib`
Expected: 141+ passed, 0 failed.

- [ ] **Step 9: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/interactive.rs && git commit -m "fix(tui): preserve prompt whitespace and quoted file paths"`

---

### Task 3: Gate stdin reads in print mode

**Files:**
- Modify: `crates/mehh-cli/src/main.rs:994-1021` and its `#[cfg(test)] mod tests`; add `UserContentBlock` to the `mehh_ai` import at the top.
- Test: `cargo test -p mehh-cli`

**Interfaces:**
- Consumes: `io::stdin().is_terminal()` and `blocks: Vec<UserContentBlock>` assembled by `process_file_arguments`.
- Produces: `fn should_read_stdin(prompt_text: &str, blocks: &[UserContentBlock], stdin_is_terminal: bool) -> bool` — private helper in `main.rs`; returns true only when stdin is not a terminal AND `prompt_text.trim().is_empty()` AND `blocks.is_empty()`.

- [ ] **Step 0: Ownership check**

Run `git diff -- crates/mehh-cli/src/main.rs` and confirm the stdin block is still the other session's uncommitted change. If that session landed a different version, stop and re-scope this task rather than overwriting it.

- [ ] **Step 1: Write the failing predicate tests**

In `main.rs::tests`, add `should_read_stdin_only_when_prompt_is_empty`:

```rust
assert!(should_read_stdin("", &[], false));
assert!(!should_read_stdin("review", &[], false));
assert!(!should_read_stdin("", &[mehh_ai::UserContentBlock::Image(
    mehh_ai::ImageContent { data: "x".into(), mime_type: "image/png".into() }
)], false));
assert!(!should_read_stdin("", &[], true));
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-cli should_read_stdin_only_when_prompt_is_empty`
Expected: FAIL with "cannot find function `should_read_stdin`".

- [ ] **Step 3: Add the helper and rewire the call site**

Add `should_read_stdin` near `run_print_flow`. Replace `if !io::stdin().is_terminal() {` at `main.rs:1011` with `if should_read_stdin(&prompt_text, &blocks, io::stdin().is_terminal()) {`. Leave the read/append body unchanged.

- [ ] **Step 4: Run the CLI tests**

Run: `cargo test -p mehh-cli`
Expected: 48+ passed, 0 failed.

- [ ] **Step 5: Commit (only if the user asked)**

`git add crates/mehh-cli/src/main.rs && git commit -m "fix(cli): do not read piped stdin when a prompt is present"`

---

### Task 4: Escape file names in the prompt markup

**Files:**
- Modify: `crates/mehh-coding-agent/src/file_args.rs:79` and its test module
- Test: `cargo test -p mehh-coding-agent --lib file_args`

**Interfaces:**
- Consumes: `path_str` from `arg.strip_prefix('@')` in `process_file_arguments`.
- Produces: `fn escape_file_tag_value(raw: &str) -> String` — private helper in `file_args.rs`; escapes `&` first, then `"` -> `&quot;`, `'` -> `&apos;`, `<` -> `&lt;`, `>` -> `&gt;`.

- [ ] **Step 1: Write the failing helper test**

Add `file_tag_value_is_escaped`:

```rust
assert_eq!(
    super::escape_file_tag_value("a\"b<c>d&e'f"),
    "a&quot;b&lt;c&gt;d&amp;e&apos;f"
);
assert_eq!(super::escape_file_tag_value("notes.txt"), "notes.txt");
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p mehh-coding-agent --lib file_tag_value_is_escaped`
Expected: FAIL with "cannot find function `escape_file_tag_value`".

- [ ] **Step 3: Implement and use the helper**

Add `escape_file_tag_value` above `process_file_arguments`. Change the tag at `file_args.rs:79` to `format!("<file name=\"{}\">\n{clean}\n</file>", escape_file_tag_value(path_str))`. Do not change the file-content handling or image branch.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mehh-coding-agent --lib file_args` then `cargo test -p mehh-cli`
Expected: both PASS; the existing CLI `process_file_arguments_expands_at_text_with_bom_strip_and_at_images` still passes.

- [ ] **Step 5: Commit (only if the user asked)**

`git add crates/mehh-coding-agent/src/file_args.rs && git commit -m "fix(prompt): escape file names in attached-file markup"`

---

## Final verification (after all four tasks)

- [ ] **Run both suites**

Run: `cargo test -p mehh-coding-agent --lib && cargo test -p mehh-cli && cargo check -p mehh-cli --tests`
Expected: 0 failures, `cargo check` finishes cleanly.

- [ ] **Re-run the independent review pipeline**

Regenerate per-file patches into a fresh scratch dir (include the untracked `file_args.rs` via `git diff --no-index /dev/null ...`), run the static scan, and dispatch a fresh reviewer with `requesting-code-review`. Only report done when `passed=true` with empty `security_concerns` and `logic_errors`.
