# Goose-Parity Core & Runtime Architecture — 3-Wave Design Specification

**Date:** 2026-11-16
**Target:** `crates/*` (`mehh-agent`, `mehh-mcp`, `mehh-tools`, `mehh-codemode`, `mehh-coding-agent`, `mehh-tui`, `mehh-cli`)
**Approach:** Approach A — Core Runtime Governance, Code Intelligence, Working Memory, and Multi-Mode Lead/Worker Subagents

---

## 1. Intent and Why

Mehhcode already outperforms `aaif-goose/goose` in binary footprint (~16.9 MB standalone binary vs. Electron + daemon), zero-IPC in-process tools (`mehh-tools`), QuickJS code execution (`mehh-codemode`), file mutation serialization (`FileMutationQueue`), and migration-free JSONL v3 tree persistence (`SessionManager`).

However, inspecting the current codebase reveals specific runtime and coding-intelligence gaps compared to Goose:
- **Unwired lifecycle hooks:** `AgentLoopConfig` (`crates/mehh-agent/src/agent_loop.rs`) declares `before_tool_call`, `after_tool_call`, `prepare_request`, `prepare_next_turn`, `transform_context`, and `finish_turn`, yet `AgentSession::prepare_prompt_content` (`crates/mehh-coding-agent/src/agent_session.rs`) leaves all six `None`.
- **No runtime tool governance:** `trust.rs` only gates loading `.mehh/*` at startup. Once inside a session, tools execute with no dangerous-command guard, no `smart_approve`/interactive confirmation, and no infinite-repetition loop breaker. Furthermore, `CodemodeTool` (`crates/mehh-codemode/src/tool.rs`) invokes `tool.execute(...)` directly, bypassing `AgentLoopConfig` hooks, and `McpToolInfo` (`crates/mehh-mcp/src/client.rs`) drops MCP `annotations` (`readOnlyHint`, `destructiveHint`).
- **Blind TUI tool cards:** `TranscriptItem::ToolCall` (`crates/mehh-coding-agent/src/interactive.rs`) stores only `{ id, name, status, is_error }`, discarding live `BashTool` streaming previews and `EditTool` unified diffs.
- **Missing code structure, undo, and working memory:** Agents burn tokens calling `read` on huge files without an AST/symbol outline (`analyze`), cannot undo a bad file mutation without `git`, lack a live `Todo` + MOIM (`Model-Observed Internal Memory`) checklist injected across compactions, and `ToolSearchTool` (`crates/mehh-coding-agent/src/tool_search.rs`) uses naive substring `contains` scoring without indexing JSON schema properties.
- **Constrained subagent & single-model execution:** `SubagentTool` (`crates/mehh-coding-agent/src/subagent.rs`) is locked to 4 read-only tools, drops `on_update`, runs sequentially, and always uses the parent's heavy model instead of Lead/Worker model routing.

Closing these gaps turns Mehhcode into a safer, lower-token, faster terminal coding agent than Goose without sacrificing its single-binary, in-process architecture.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
- **Wave 1 — Runtime Tool Governance, MCP Hints & TUI Tool Visibility:**
  - Introduce a priority-ordered `ToolInspector` pipeline (`SecurityInspector` -> `PermissionInspector` -> `RepetitionInspector`) wired through `AgentSession::prepare_prompt_content` (`before_tool_call`) AND through `CodemodeTool`'s internal sandbox executor.
  - Parse MCP `annotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) in `McpToolInfo` and add `is_read_only(&self) -> Option<bool>` on `AgentTool`.
  - Add `PermissionMode` (`Auto`, `SmartApprove`, `Approve`, `Chat`), persisted tool permission overrides (`~/.mehh/agent/permissions.json`), CLI flags, `/mode` slash command, serialized interactive TUI approval modal with diff/command preview, and rich `TranscriptItem::ToolCall` rendering (live streaming output + colorized `EditTool` diff).
- **Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM, and BM25 `tool_search`:**
  - Add a pure-Rust structural code analyzer tool (`analyze`) supporting `structure`, `symbols`, and `focus` modes for Rust, TypeScript/JavaScript, Python, and Go with zero new external crates.
  - Add session-scoped file snapshot & rollback (`EditUndoStore` + `undo_edit` tool) integrated inside `with_file_mutation_queue` for `EditTool` and `WriteTool`.
  - Add `TodoTool` (`todo`) + MOIM injection via `AgentLoopConfig.prepare_request` (keeping active checklist and pinned instructions visible at the turn boundary without polluting saved JSONL).
  - Upgrade `DeferredToolCatalog` in `tool_search.rs` to Okapi BM25 (`k1 = 1.2`, `b = 0.75`) indexing tool names, descriptions, server namespaces, and JSON schema property keys/descriptions.
- **Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing:**
  - Upgrade `SubagentTool` to support `mode: "research" | "coding" | "review"`, live `on_update` progress forwarding, parent `ToolInspector` enforcement inside child loops, and parallel subagent execution (`ExecutionMode::Parallel` for `research`/`review`; `FileMutationQueue` + serialized approval broker for `coding`).
  - Add Lead/Worker model routing (`LeadWorkerConfig` in `Settings` / CLI `--worker-model` / `--lead-turns`): route `SubagentTool` (default) and post-lead execution turns to a fast/cheap worker model with automatic escalation/fallback back to the lead model on provider/tool-loop failure.

### Non-Goals
- Adding `tree-sitter` C crates or any new external Cargo dependencies (`Cargo.toml` dependencies remain fixed).
- Goose YAML `Recipe` engine (`mehh-recipe`), `ChatRecall` cross-session search, HTTP/WebSocket daemon (`mehh serve`), or IDE `ACP` server (`mehh acp`) — reserved for follow-up tracks (Approaches B and C).
- Using an auxiliary LLM call as a permission classifier inside `SmartApprove` (deterministic rules + `is_read_only` + shell/path classification avoid extra latency, token cost, and network failure modes).
- Converting `mehh-tools` into external MCP child processes (core tools stay in-process).

### Anti-Goals
- Allowing `CodemodeTool` JS scripts or `"coding"` subagents to bypass `SecurityInspector` or `PermissionInspector`.
- Blocking headless CLI/RPC runs (`-p`, `--mode json`, `--mode rpc`) on an interactive approval prompt that has no TTY listener (headless `RequireApproval` must fail closed as `Deny` with an actionable error).
- Persisting ephemeral MOIM injections into `.jsonl` session entries or silently swallowing malformed `permissions.json` syntax errors (`ENOENT` -> default; corrupt JSON -> explicit `Err`).
- Allowing recursive `subagent -> subagent` spawning (`coding`/`research`/`review` child tool lists must exclude `subagent`).

---

## 3. Global Constraints

- **Zero new external crates:** Implement Waves 1–3 strictly with existing workspace dependencies in root `Cargo.toml` (`tokio`, `tokio-util`, `futures`, `serde`, `serde_json`, `regex`, `ignore`, `globset`, `similar`, `uuid`, `sha2`, `crossterm`, `unicode-width`, `unicode-segmentation`, `clap`, `tracing`, `anyhow`, `thiserror`).
- **Path & file hygiene:** Normalize file paths canonically (with lexical fallback and lowercasing on Windows `cfg!(windows)`) before keying security rules, `FileMutationQueue`, `EditUndoStore`, or `permissions.json`. Strip UTF-8 BOM on config reads and write UTF-8 without BOM using `write_file_atomic`.
- **Backward-compatible Serde evolution:** New fields on shared structs (`Model`, `Settings`, `McpToolInfo`, `AgentToolResult`, `AgentEvent`) use `#[serde(default, skip_serializing_if = "Option::is_none")]` (or `#[serde(default)]`) so existing tests and JSONL sessions remain valid.
- **Keybinding discipline:** Register any new TUI interactive actions (approval keys if mapped via actions) in `TuiAction` / `Keybindings` (`crates/mehh-tui/src/keys.rs`) rather than hardcoding raw checks inconsistently.
- **Verification gate per wave:** After each wave, `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass with 0 failures and 0 warnings.

---

## 4. Approaches Considered

1. **Approach A — Core Governance, Code Intelligence, Working Memory & Multi-Mode Lead/Worker Subagents (Chosen):** Hardens runtime safety, wires all unused `AgentLoopConfig` hooks, adds pure-Rust AST `analyze` + `undo_edit` + `Todo`/MOIM + BM25 `tool_search`, renders live diffs/approvals in TUI, and upgrades `subagent` + Lead/Worker routing. Delivers immediate daily coding superiority over Goose in the standalone CLI/TUI binary.
2. **Approach B — Declarative Recipes & Multi-Session Recall (`mehh-recipe` + `ChatRecall`):** Focuses on YAML playbook automation and cross-session history search. Deferred until Approach A establishes runtime tool governance (`ToolInspector`), which automated recipes depend on to run safely.
3. **Approach C — Daemon Server & IDE Protocol (`mehh serve` + `mehh acp`):** Focuses on multi-client HTTP/WebSocket serving and Zed/VS Code ACP integration. Deferred until core execution safety, TUI observability, and Lead/Worker subagents are solid.

---

## 5. Wave 1 — Runtime Tool Governance, MCP Annotations & TUI Approval/Diffs

### 5.1 Tool Read-Only Metadata & MCP Annotations (`mehh-agent`, `mehh-mcp`, `mehh-tools`)

1. **`mehh-mcp` (`crates/mehh-mcp/src/client.rs`, `tool_adapter.rs`):**
   - Add `McpToolAnnotations` struct (`read_only_hint: Option<bool>`, `destructive_hint: Option<bool>`, `idempotent_hint: Option<bool>`, `open_world_hint: Option<bool>`, camelCase serde) and `#[serde(default, skip_serializing_if = "Option::is_none")] pub annotations: Option<McpToolAnnotations>` on `McpToolInfo`.
   - Update `McpAgentTool` (`tool_adapter.rs`) to expose `annotations` and implement `AgentTool::is_read_only(&self) -> Option<bool>` returning `self.tool_info.annotations.as_ref().and_then(|a| a.read_only_hint)` and `AgentTool::execution_mode(&self)`: `Parallel` only when `read_only_hint == Some(true)`, otherwise `Sequential` (preventing unannotated mutating MCP tools from firing concurrently without serialization).
   - Update `ListMcpResourcesTool`, `ListMcpResourceTemplatesTool`, `ReadMcpResourceTool` (`tool_adapter.rs`) to return `is_read_only() -> Some(true)`.

2. **`mehh-agent` (`crates/mehh-agent/src/types.rs`):**
   - Add two default methods on `AgentTool`:
     - `fn is_read_only(&self) -> Option<bool> { None }`
     - `fn is_safe_for_auto_approve(&self, _args: &serde_json::Map<String, serde_json::Value>) -> Option<bool> { self.is_read_only() }`
     *(Note: `is_safe_for_auto_approve` allows `BashTool`/`PowerShellTool` to inspect the command string, while pure read tools return `Some(true)`.)*

3. **`mehh-tools` & `mehh-coding-agent` implementations:**
   - `ReadTool`, `GrepTool`, `FindTool`, `LsTool`: `is_read_only() -> Some(true)`.
   - `WriteTool`, `EditTool`: `is_read_only() -> Some(false)`.
   - `BashTool`, `PowerShellTool`: `is_read_only() -> Some(false)`. (Read-only shell classification in `SmartApprove` is handled centrally inside `PermissionInspector` rather than scattered in tools so all rules stay auditable in one place.)
   - `ToolSearchTool` (`tool_search.rs`): `is_read_only() -> Some(true)`.
   - `SubagentTool` (`subagent.rs`): `is_read_only()` returns `Some(true)` only when invoked in `"research"`/`"review"`? Since `is_read_only(&self)` takes `&self` (static metadata), `SubagentTool` returns `None` (neutral container) and its inner child loop runs through the parent's `ToolInspectionManager`, which inspects each child `read`/`edit`/`bash` call individually!
   - `CodemodeTool` (`mehh-codemode/src/tool.rs`): `is_read_only() -> None` at the outer tool boundary (or `Some(false)` unless inspected per inner tool call — see 5.2 below!).

### 5.2 Chain-of-Responsibility `ToolInspectionManager` (`crates/mehh-coding-agent/src/tool_inspection.rs`)

Create `crates/mehh-coding-agent/src/tool_inspection.rs`:

- **Core Types:**
  ```rust
  pub enum InspectionVerdict {
      Allow,
      RequireApproval { reason: String, preview: Option<String> },
      Deny { reason: String },
  }

  #[async_trait::async_trait]
  pub trait ToolInspector: Send + Sync {
      fn name(&self) -> &str;
      async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
  }
  ```
  `ToolInspectionContext` holds `{ tool_call_id, tool_name, args, is_read_only: Option<bool>, cwd }`.
  `ToolInspectionManager` runs inspectors in strict priority order and merges verdicts:
  - First `Deny` stops immediately and blocks the tool call (`BeforeToolCallResult { block: true, reason: Some(reason), terminate: false }`).
  - Any `RequireApproval` escalates to the configured `ApprovalHandler` (even if a later inspector would `Allow`).
  - `Allow` proceeds only if no inspector returned `Deny` or `RequireApproval`.

- **Inspector 1 — `SecurityInspector` (Priority 1, runs in ALL modes including `Auto`):**
  - **Command danger patterns (for `bash` / `powershell`):** Compiled `OnceLock<Vec<(Regex, &'static str)>>` matching catastrophic/exfiltration patterns (e.g., `rm -rf /` or `rm -rf ~`, `mkfs`, `dd if=.* of=/dev/`, fork bombs `:(){ :|:& };:`, `curl|wget .* \| (sh|bash)`, reading/exfiltrating `~/.ssh/id_*`, `~/.mehh/agent/auth.json`, `~/.mehh/agent/mcp-auth.json`, `Invoke-Expression` / `iex` piped from `Invoke-WebRequest`). Returns `RequireApproval` in interactive `Auto` mode (or `Deny` if critical/untrusted) and `Deny` in `Chat`/headless without explicit override.
  - **Sensitive path protection (for `read`, `write`, `edit`, `undo_edit`):** Rejects writes/edits to `~/.mehh/agent/{auth.json,mcp-auth.json,trust.json,permissions.json}` so a prompt-injected model cannot rewrite its own permission rules or steal OAuth tokens. Rejects writes outside normalized `cwd` unless the user explicitly approves (`RequireApproval` in interactive, `Deny` in headless unless `--allow-outside-cwd` / `Auto`).

- **Inspector 2 — `PermissionInspector` (Priority 2):**
  - **Modes (`PermissionMode`):**
    - `Auto`: Executes tools without prompting (except `SecurityInspector` flags).
    - `SmartApprove` (**default in interactive mode**):
      - Checks persisted rules in `~/.mehh/agent/permissions.json` (`AlwaysAllow`, `AskBefore`, `NeverAllow`) and session-scoped `"Allow for this session"` grants first.
      - Otherwise, if `ctx.is_read_only == Some(true)` -> `Allow`.
      - If `tool_name` is `bash` / `powershell` -> checks a conservative read-only command whitelist (`git status`, `git diff`, `git log`, `git show`, `cargo check`, `cargo clippy`, `cargo test`, `ls`, `dir`, `pwd`, `cat`, `rg`, `find`, `which`, `Get-ChildItem`, `Select-String` when free of redirections `>`, `| sh`, `-exec`, or chaining `;`/`&&`/`||` containing non-whitelisted commands). Safe read-only commands -> `Allow`; mutating/unknown commands -> `RequireApproval` with command preview.
      - If `tool_name` is `edit` / `write` / `undo_edit` -> `RequireApproval` with pre-computed unified diff preview (or byte/line summary for `write`).
      - If `tool_name` is `codemode` -> outer `codemode` execution is `Allow` (unless `Chat` mode), while EVERY inner `tools.<name>(args)` call inside `CodemodeTool` runs through `ToolInspectionManager`! That way read-only JS scripts run without prompting, while a JS script calling `tools.edit(...)` or `tools.bash(...)` triggers `SecurityInspector` + `PermissionInspector` cleanly.
    - `Approve`: Prompts (`RequireApproval`) for every tool call not explicitly marked `AlwaysAllow`.
    - `Chat`: Read-only conversational investigation mode. Allows read-only exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, and MCP tools with `annotations.readOnlyHint == Some(true)`). Strictly returns `Deny` for mutating tools (`edit`, `write`, `undo_edit`), shell execution (`bash`, `powershell`), unannotated MCP tools, and `subagent` with `mode: "coding"`.
    - **Rule Scoping & Granularity in `permissions.json`:**
      - Scoped at `~/.mehh/agent/permissions.json` (per-user) and optional trusted `.mehh/permissions.json` (per-workspace, only loaded if workspace is trusted in `trust.rs`).
      - Schema supports:
        - `tool_name: String` (e.g. `"read"`, `"mcp__github__get_issue"`, `"bash"`).
        - `decision: "AlwaysAllow" | "AskBefore" | "NeverAllow"`.
        - `arg_prefix: Option<String>` (e.g., `command_prefix: "cargo test"` or `"git status"` for `bash`/`powershell`).
        - `path_glob: Option<String>` (e.g., `path_glob: "tests/**"` for `edit`/`write`/`undo_edit`).
      - Evaluation precedence: Exact rule (`tool_name` + matching `arg_prefix`/`path_glob`) > Tool-level rule (`tool_name` with no filter) > Default mode policy (`SmartApprove`).
    - **MCP Unannotated Tool Fallback:** MCP tools without `readOnlyHint` (`annotations.read_only_hint == None`) fail closed: treated as mutating (`is_read_only() -> None`), requiring explicit approval in `SmartApprove` and denied in `Chat` mode.

- **Inspector 3 — `RepetitionInspector` (Priority 3):**
  - Tracks consecutive identical `(tool_name, canonical_json_args)` calls per turn/session (`Mutex<RepetitionState>`, default `max_repetitions = 3`).
  - Resets when a different tool or different arguments are called (or on turn reset).
  - When count exceeds `max_repetitions`, returns `Deny { reason: "Repeated identical tool call detected (3+ times). Change strategy or inspect prior output instead of repeating the exact same call." }`.

### 5.3 Interactive Approval Broker, `CodemodeTool` Integration & Headless Policy

- **Wiring `AgentSession` (`crates/mehh-coding-agent/src/agent_session.rs`):**
  - Add `permission_mode: PermissionMode`, `permission_store: PermissionStore`, `approval_handler: Option<Arc<dyn ApprovalHandler>>`, and `inspection_manager: Arc<ToolInspectionManager>` on `AgentSession`.
  - In `prepare_prompt_content`, wire `config.before_tool_call` to invoke `inspection_manager.inspect_and_authorize(...)` (checking `RepetitionInspector`, `SecurityInspector`, `PermissionInspector`, and calling `approval_handler` if `RequireApproval`).
  - Wire `CodemodeTool` (`crates/mehh-codemode/src/tool.rs`): add an optional `ToolCallInterceptor` callback trait/closure on `CodemodeTool` (`with_interceptor(...)`) invoked before `tool.execute(...)` inside the QuickJS executor closure (`tool.rs:140-162`). `AgentSession` attaches the same `ToolInspectionManager` + `approval_handler` to `CodemodeTool`.
- **Serialized Interactive Approval (`interactive.rs`):**
  - Add `AgentSignal::ApprovalRequest { request: ApprovalRequest, reply: tokio::sync::oneshot::Sender<ApprovalDecision> }` and `SelectorKind::ToolApproval` in `interactive.rs`.
  - Wrap `ApprovalHandler` requests behind an async `tokio::sync::Mutex<()>` so parallel tool calls (e.g., two MCP tools requiring approval) queue their modal prompts one-by-one rather than overwriting `state.selector`.
  - When `SelectorKind::ToolApproval` is active, display tool name, target path/command, and up to 18 lines of diff/preview inside a `BorderedBox`, letting the user choose:
    - `Allow once` (`y` / `Enter` on option 1)
    - `Allow tool for session` (`a`)
    - `Always allow tool` (`A`, persisted to `~/.mehh/agent/permissions.json`)
    - `Deny` (`n` / `Esc`)
  - Register `y`/`a`/`n` shortcuts cleanly or navigate with `Up`/`Down` + `Enter` inside `SelectList` so existing keybindings stay uniform.
- **Headless Policy & Security Boundaries (`mehh-cli` / `modes.rs` / `rpc.rs` / `trust.rs`):**
  - Add `--permission-mode <auto|smart_approve|approve|chat>` to `CliArgs`.
  - **Permission Mode Escalation Guard:** Workspace `.mehh/settings.json` is strictly forbidden from loosening `permission_mode` to `Auto`. Any workspace setting specifying `auto` is ignored and clamped to `SmartApprove` unless overridden by global `~/.mehh/agent/settings.json` or explicit `--permission-mode auto` on the CLI.
  - **Protected Metadata Files:** `<cwd>/.mehh/*` files (`settings.json`, `trust.json`, `permissions.json`) are strictly write-protected in `SecurityInspector` against write/edit/undo tools during a session.
  - **Headless Runs (`-p`, `--mode json`):** Default to `PermissionMode::SmartApprove`. `ApprovalHandler` is `HeadlessDenyHandler`: any `RequireApproval` verdict fails closed as `Deny` with an actionable error (`"Tool '<name>' requires approval in headless mode (pass --permission-mode auto or configure permissions.json)"`).
  - **RPC Mode (`--mode rpc`):** During RPC handshake (`initialize` / `get_session`), the client declares its capabilities (`capabilities: { tool_approvals: bool }`). If `tool_approvals: true`, `ApprovalHandler` sends an `approval_request` JSON-RPC notification and awaits an `approve_tool` response. If unannounced or `false`, it fails closed (`Deny`) to protect non-interactive RPC scripts.
- **Slash command `/mode`:**
  - Add `/mode [auto|smart_approve|approve|chat]` in `interactive.rs` (`builtin_slash_commands()` + `handle_slash_command`) and expose `permission_mode` in `/settings` (`build_settings_list`).

### 5.4 Rich `TranscriptItem::ToolCall` Rendering (`interactive.rs`)

- Extend `TranscriptItem::ToolCall`:
  ```rust
  ToolCall {
      id: String,
      name: String,
      status: String,
      is_error: bool,
      args_summary: String,
      output_preview: Vec<String>,
      diff: Option<String>,
  }
  ```
- In `apply_agent_event`:
  - `ToolExecutionStart`: populate concise `args_summary` (e.g., `path` for `read`/`edit`/`write`, truncated `command` for `bash`, `query` for `grep`/`tool_search`, `mode` for `subagent`).
  - `ToolExecutionUpdate`: retain the last 8 non-empty lines of `partial_result` text into `output_preview` (so `BashTool` streaming and `SubagentTool` progress render live in the transcript).
  - `ToolExecutionEnd`: set final status (`completed` / `failed`, plus `duration_ms` if present), extract `result.details["diff"]` into `diff`, and clear transient streaming preview for `read`/`grep`/`find`/`ls` while keeping concise tail (`max 8 lines`) on `bash` errors or `diff` (capped at `24` lines) with theme `+` (`tool_success`) / `-` (`tool_error`) / `@@` (`accent`) colors.

---

## 6. Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM & BM25 `tool_search`

### 6.1 Pure-Rust Structural Code Analyzer (`crates/mehh-tools/src/analyze.rs`)

- **Tool name:** `analyze` (`is_read_only() -> Some(true)`, `ToolExecutionMode::Parallel`).
- **Parameters:**
  - `path` (`string`, required): file or directory path (relative to `cwd` or absolute).
  - `mode` (`"structure" | "symbols" | "focus"`, optional, default `"symbols"` for files, `"structure"` for directories).
  - `focus` (`string`, optional): symbol name (e.g., `AgentSession` or `prepare_prompt_content`) to zoom into and print exact line range + body (capped at `250` lines).
  - `depth` (`integer`, optional, default `2`, clamped `1..=5`): directory recursion depth for `"structure"` mode.
- **Parser Engine (Zero External Dependencies):**
  - Uses `ignore::WalkBuilder` for directory traversal in `structure` mode (skipping `.git`, `target`, `node_modules`).
  - Language detection by extension:
    - **Rust (`.rs`)**: matches `pub(crate|super)? (async )?(unsafe )?(fn|struct|enum|trait|impl|mod|type|const|macro_rules!)` and tracks `{`/`}` brace depth (ignoring strings/comments) to compute accurate `[start_line..end_line]` spans for each item and its methods inside `impl` blocks.
    - **TypeScript / JavaScript (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`)**: matches `export`, `class`, `interface`, `type`, `enum`, `(async )?function`, and arrow/method signatures with `{`/`}` span tracking.
    - **Python (`.py`)**: matches `(async )?def`, `class` and computes `[start_line..end_line]` via indentation tracking.
    - **Go (`.go`)**: matches `func`, `type ... struct`, `type ... interface` with `{`/`}` span tracking.
  - Truncates output cleanly (`MAX_ANALYZE_CHARS = 24_000`) with actionable guidance to call `read` or `analyze(focus=...)`.

### 6.2 File Snapshot & Rollback (`crates/mehh-tools/src/undo_edit.rs`, `edit.rs`, `write.rs`)

- **Shared `EditUndoStore`:**
  - Thread-safe per-session/tool-set stack `Arc<Mutex<HashMap<String, Vec<FileSnapshot>>>>`, keyed by `get_mutation_queue_key(&full_path)` (`file_mutation_queue.rs`).
  - `FileSnapshot`:
    ```rust
    pub enum FileSnapshot {
        Existed { bytes: Vec<u8>, tool_name: String, timestamp_ms: i64 },
        DidNotExist { tool_name: String, timestamp_ms: i64 },
    }
    ```
  - Bounded per-file (`MAX_SNAPSHOTS_PER_FILE = 20`) and total memory (`MAX_UNDO_BYTES = 16 MiB` total across snapshots; skips snapshots for files `> 4 MiB` with a note).
  - `EditTool` (`with_undo_store(...)`) and `WriteTool` (`with_undo_store(...)`) record the pre-mutation bytes (preserving exact UTF-8 BOM and CRLF bytes!) or `DidNotExist` inside `with_file_mutation_queue` immediately before `tokio::fs::write`.
- **`UndoEditTool` (`"undo_edit"`):**
  - `is_read_only() -> Some(false)`, `ToolExecutionMode::Sequential`.
  - Parameters: `path` (`string`, required).
  - Runs inside `with_file_mutation_queue(&full_path, ...)`: pops the latest `FileSnapshot` for that normalized path, restores exact original bytes (or deletes the file if `DidNotExist` and current content matches/exists), and returns unified diff of the revert in `details["diff"]`.
  - Also expose `/undo` slash command in `interactive.rs` (optional user shortcut calling `EditUndoStore` directly or guiding the user).

### 6.3 `TodoTool` & MOIM Working Memory Injection (`crates/mehh-coding-agent/src/todo.rs`, `agent_session.rs`)

- **`TodoState` & `TodoTool` (`"todo"`):**
  - Shared `Arc<RwLock<TodoState>>` stored on `AgentSession` (and persisted optionally as `SessionEntryKind::CustomMessage` or dedicated `custom_type: "todo_state"` / reconstructed from active branch entries on `/resume` and `/tree`!).
  - `TodoItem { id: String, content: String, status: TodoStatus (Pending | InProgress | Completed), priority: Option<String> }`.
  - Tool operations (`action`: `"replace" | "add" | "update" | "list" | "clear"`): `is_read_only()` returns `None` (`list` is safe, mutating actions update only in-memory/session checklist — treated as `Allow` in `SmartApprove` since it touches no workspace files or external commands!).
- **MOIM (`Model-Observed Internal Memory`) via `prepare_request` hook:**
  - Wire `AgentLoopConfig.prepare_request` in `AgentSession::prepare_prompt_content`.
  - Before every LLM call inside `run_agent_loop` (including multi-turn tool chains and post-compaction turns), `prepare_request` reads:
    1. Active `TodoState` (if non-empty, formatted compactly as `<working_checklist>...</working_checklist>`).
    2. Optional pinned session/env invariants (`MEHH_MOIM_TEXT` / `MEHH_MOIM_FILE` or `.mehh/MOIM.md` when `trusted == true`, capped at 8 KB).
  - Injects this ephemeral block into the final `User` message (or as a transient `Message::System` at the tail of `req_ctx.context.messages` — *our call:* append as a single transient `Message::System` or trailing user reminder block only in `req_ctx.context.messages` after `transform_messages` strips/handles system turns cleanly!). Let's verify `transform_messages.rs` first so `Message::System` at the tail works across all 6 providers — if `Anthropic`/`Google` reject mid/trailing `System` messages, fold MOIM cleanly into the last `User`/`ToolResult` text context or into `system_prompt` clone! Wait: modifying `req_ctx.context.messages` to append a synthetic `<moim_context>` text block onto the latest user turn (or a trailing user context message only when the last message is `ToolResult` / `User`) guarantees 100% compatibility across Anthropic, OpenAI, Google, Bedrock, and Mistral without ever persisting to `new_messages` or `SessionManager`!

### 6.4 BM25 & Schema Indexing in `tool_search.rs`

- Upgrade `DeferredToolCatalog::search` (`crates/mehh-coding-agent/src/tool_search.rs`):
  - Precompute inverted document stats (`DeferredToolIndex`) on `DeferredToolCatalog::new` and `replace_deferred`:
    - Tokenize identifiers (`snake_case`, `camelCase`, `mcp__server__tool` splits on `_`, `-`, uppercase transitions) into normalized lowercase terms.
    - Weighted fields:
      - Exact tool name / local tool name boost (`+50.0` exact match, `+15.0` suffix/substring match).
      - BM25 (`k1 = 1.2`, `b = 0.75`) across weighted fields: `name` + `local_name` (`weight = 3.0`), `description` + JSON schema property names & property descriptions (`weight = 1.0`), server namespace (`weight = 0.5`).
  - Keep `ToolSearchTool` response format (`matches` + `newlyActivated`) backward-compatible so existing callers/tests and `CodemodeTool` work seamlessly.

---

## 7. Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing

### 7.1 Multi-Mode `SubagentTool` (`crates/mehh-coding-agent/src/subagent.rs`)

- **Tool Parameters:**
  - `prompt` (`string`, required when `tasks` is omitted): Self-contained objective.
  - `tasks` (`array` of `{ prompt: string, mode?: string, context?: string, max_turns?: integer }`, optional, max `6` items): Enables a single LLM turn to fan out parallel subtasks cleanly via `futures::future::join_all` (capped at concurrency `4` via `tokio::sync::Semaphore` to avoid API rate-limit bursts).
  - `mode` (`"research" | "coding" | "review"`, optional, default `"research"`):
    - `"research"`: `["read", "grep", "find", "ls", "analyze"]`.
    - `"coding"`: `["read", "write", "edit", "undo_edit", "bash", "powershell", "grep", "find", "ls", "analyze"]` (excludes `subagent`, `codemode`, `todo`).
    - `"review"`: `["read", "grep", "find", "ls", "analyze"]` with a review-oriented system prompt focusing on bugs, invariants, and missing tests.
  - `context` (`string`, optional): Extra context prepended to child user prompt.
  - `max_turns` (`integer`, optional, default `15`, clamped `1..=30`).
- **Safety & Execution Mode:**
  - Child `AgentLoopConfig` ALWAYS inherits the parent's `before_tool_call` (`ToolInspectionManager` + serialized `ApprovalHandler`) and `cancel.child_token()`.
  - Since `EditTool` / `WriteTool` / `UndoEditTool` already serialize per-file via `FileMutationQueue`, parallel `"coding"` tasks editing disjoint files run concurrently while same-file edits serialize cleanly; `SubagentTool::execution_mode()` returns `Parallel` when `mode` isn't statically known, relying on `FileMutationQueue` + serialized approval mutex for safety (or keeps `Sequential` for single calls when `mode == "coding"` if checked? Since `execution_mode(&self)` has no `params`, keeping `FileMutationQueue` + approval mutex safe for concurrent execution is mandatory!).
  - **Progress Streaming:** Wire `on_event` inside the child `run_agent_loop` to forward concise throttled updates (`"[subagent:<mode>] turn N: <tool_name>"` / `"[subagent batch 2/3] completed"`) through the parent tool's `on_update` callback!
  - **Rich Return Details:** Return `details: { mode, model, turns_used, total_tokens, cost, aborted }` alongside final assistant text (or aggregated structured report for `tasks` batches).

### 7.2 Lead/Worker Model Routing (`crates/mehh-coding-agent/src/lead_worker.rs`, `settings.rs`, `agent_session.rs`)

- **Configuration (`Settings` + CLI):**
  - Extend `Settings` (`settings.rs`) with optional fields:
    - `permission_mode: Option<PermissionMode>`
    - `worker_model: Option<String>` (e.g. `"anthropic/claude-3-5-haiku-latest"` or `"openai/gpt-4o-mini"` or local Ollama model from `models.json`)
    - `lead_turns: Option<usize>`: Defines the number of initial planning turns handled by the `lead` model for each user prompt.
      - **Per-Prompt Lifecycle:** `lead_turns` tracking resets at the start of each user prompt. The first `n` turns execute on the Lead model, switching to Worker for subsequent turns.
      - **Escalation Rules:** An automatic fallback/escalation from Worker back to Lead occurs ONLY when the Worker hits a provider error (`StopReason::Error`), returns an empty response, or trips `RepetitionInspector`. Normal tool errors (e.g. test failure, compiler error, missing file) do NOT trigger escalation. Once escalated, the session remains on Lead for the remainder of that user prompt.
  - Add `--worker-model <MODEL>` and `--lead-turns <N>` to `CliArgs` (`mehh-cli/src/main.rs`).
- **Resolution & Key Isolation:**
  - Resolve `worker_model` via `ModelRegistry` (`models.json` + built-in catalog) and resolve its provider + API key separately via `resolve_api_key_async` / `ModelRegistry::provider_api_key` so a cross-provider Lead/Worker pair (e.g., Anthropic Opus Lead + OpenAI/Gemini/Ollama Worker) never leaks Anthropic keys to OpenAI or vice versa!
- **Integration Points:**
  1. **`SubagentTool`:** When a `worker` model + provider + key is configured, `SubagentTool` uses the `worker` model for `"research"` and `"coding"` subtasks (unless `"review"` or explicitly overridden, or falls back to `lead` on worker failure).
  2. **Main `AgentSession` loop (`prepare_next_turn` / `prepare_request`):** Wait — notice `AgentLoopConfig` currently holds a single `provider: Arc<dyn LlmProvider>` and `stream_options.api_key: Option<String>`, while `AgentLoopTurnUpdate` can change `model` and `thinking_level` (`agent_loop.rs:173-178`). To support cross-provider Lead/Worker switching mid-loop cleanly, extend `AgentLoopTurnUpdate` / `AgentRequestUpdate` (`crates/mehh-agent/src/types.rs`) with optional `provider: Option<Arc<dyn LlmProvider>>` and `api_key: Option<Option<String>>` (non-breaking `#[serde(skip)]` fields or builder defaults), and update `run_agent_loop` to pass the active `(current_model, current_provider, current_api_key)` into `stream_assistant_response`!

---

## 8. Verification & Testing Strategy

- **TDD discipline:** Write failing unit/integration tests (`RED`) before implementing each component, using `FauxProvider` and `InMemoryTransport` (no live API calls).
- **Wave 1 tests:**
  - `mehh-mcp`: `McpToolInfo` deserialization with/without `annotations`; `McpAgentTool::is_read_only()` and `execution_mode()`.
  - `mehh-coding-agent`: `SecurityInspector` blocks credential exfiltration (`~/.ssh`, `auth.json`) and destructive shell patterns; `PermissionInspector` matrix across `Auto`, `SmartApprove`, `Approve`, `Chat` (`read` allowed, `edit`/`bash` require approval in `SmartApprove`); `RepetitionInspector` triggers after `max_repetitions`; `CodemodeTool` interceptor blocks dangerous inner JS `tools.bash(...)` calls; headless approval fails closed; serialized concurrent approval broker test; `TranscriptItem::ToolCall` live preview + diff rendering test.
- **Wave 2 tests:**
  - `mehh-tools`: `AnalyzeTool` `structure`, `symbols`, and `focus` tests across Rust/TS/Python/Go fixtures; `EditUndoStore` + `UndoEditTool` tests restoring BOM/CRLF edits, deleting newly created `write` files, and respecting `FileMutationQueue`.
  - `mehh-coding-agent`: `TodoTool` CRUD + `prepare_request` MOIM injection verification (present in `FauxProvider` captured request context, absent from `.jsonl` session entries); BM25 `tool_search` ranking tests verifying schema property matches outrank noise.
- **Wave 3 tests:**
  - `mehh-coding-agent` & `mehh-agent`: `SubagentTool` `"coding"` mode executing `edit` through `ToolInspectionManager`, parallel `tasks` batch execution, `on_update` progress emissions, and Lead/Worker cross-provider switching + error escalation via `FauxProvider`.
- **Final gate per wave:** `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.

---

## 9. Decisions and Open Calls Left Explicit

### Design Calls Made Here (Reviewable by Builder & User)
1. **Deterministic `SmartApprove` (no LLM permission judge):** `PermissionInspector` relies on `AgentTool::is_read_only()`, MCP `annotations.readOnlyHint`, and a deterministic read-only shell command parser rather than making an extra LLM classification call per unknown tool. Faster, zero token cost, deterministic, and offline-safe.
2. **Pure-Rust structural `analyze` parser (zero new crates):** Avoids adding `tree-sitter` C bindings and grammar crates, keeping builds fast and dependency-free without requiring `gcc`/`cl` in `PATH`. Can be upgraded later behind a Cargo feature flag if full syntax trees for obscure languages are needed.
3. **Ephemeral MOIM injection on trailing context (no `.jsonl` bloat):** `prepare_request` injects `<working_checklist>` / MOIM only into the outgoing `Context` snapshot, reconstructing `TodoState` from session entries (`CustomMessage` `"todo_state"`) so `/resume` and `/tree` branch switches keep the right task state automatically.
4. **Headless `SmartApprove` fails closed on mutating tools:** When running `-p`/`--mode json` without `--permission-mode auto`, tools requiring approval fail closed with an explicit message rather than silently auto-approving or hanging waiting for stdin.
5. **Cross-provider `provider` + `api_key` swap in `AgentLoopTurnUpdate`:** Extending `AgentLoopTurnUpdate` (`crates/mehh-agent/src/types.rs`) lets `LeadWorker` switch between different providers (e.g., Anthropic Lead -> OpenAI/Ollama Worker) mid-turn cleanly without duplicating loop logic.
6. **Granular rule scoping in `permissions.json`:** Rules support optional `arg_prefix` (for shell commands) and `path_glob` (for file mutations). Exact prefix/glob rules take precedence over general tool-level permissions.
7. **Read-only conversational `Chat` mode:** `/mode chat` permits read-only codebase exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, MCP `readOnlyHint: true`) and denies all mutating tools, shell execution, and unannotated MCP tools.
8. **Per-prompt Lead/Worker tracking & strict escalation:** `lead_turns` resets on every user prompt. Escalation from Worker back to Lead triggers only on provider error (`StopReason::Error`), empty assistant completion, or `RepetitionInspector` loop breaks (routine tool errors like compiler/test failures stay on Worker).
9. **Workspace security lockdown:** Workspace `.mehh/settings.json` cannot elevate `permission_mode` to `Auto`, and `<cwd>/.mehh/*` configuration files are strictly write-protected from in-session agent mutations.
10. **RPC capability handshake:** Non-interactive or legacy RPC clients fail closed on `RequireApproval` unless they announce `"capabilities": { "tool_approvals": true }` during session handshake.

### What Is Left to the Builder
- Exact regex set for `SecurityInspector` and read-only command whitelist tokens inside `PermissionInspector` (follow fail-closed conservative defaults: anything with shell redirections, subshells, pipes to interpreters, or unknown commands escalates to `RequireApproval`).
- Exact UI layout spacing and color token mapping for `SelectorKind::ToolApproval` and diff line truncation inside `crates/mehh-coding-agent/src/interactive.rs`.
- Exact BM25 tokenization edge cases and `EditUndoStore` eviction policy (`VecDeque`/FIFO per file).
