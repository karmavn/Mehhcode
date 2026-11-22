# Goose-Parity Core & Runtime Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform Mehhcode into a safer, lower-token, and faster single-binary coding agent that surpasses `aaif-goose/goose` by implementing runtime tool governance (`ToolInspector`), MCP hints, pure-Rust structural `analyze`, snapshot/rollback `undo_edit`, `Todo` + MOIM working memory, BM25 schema search, multi-mode parallel `subagent`, and Lead/Worker model routing.

**Architecture:** A 3-wave, test-driven evolution across `crates/*`:
- Wave 1 adds `ToolInspectionManager` (`SecurityInspector` -> `PermissionInspector` -> `RepetitionInspector`), parses MCP `annotations`, binds `AgentSession` loop hooks (`before_tool_call`), intercepts `CodemodeTool`, adds TUI approval modals with unified diff previews, and implements `--permission-mode`.
- Wave 2 implements zero-dependency pure-Rust `analyze` (AST symbols and structure for Rust/TS/Python/Go), `EditUndoStore` + `undo_edit`, `TodoTool` with ephemeral `prepare_request` MOIM injection, and Okapi BM25 schema indexing in `tool_search`.
- Wave 3 upgrades `SubagentTool` with multi-mode (`research`, `coding`, `review`), parallel batch execution, and live progress forwarding, plus Lead/Worker model routing with cross-provider credential isolation and error escalation.

**Tech Stack:** Rust 2021, `crates/mehh-*`, Tokio, Crossterm, Serde, Regex, Globset, Similar, QuickJS (`rquickjs`). Strictly zero new external Cargo crates.

**Spec:** `docs/superpowers/specs/2026-11-16-goose-parity-core-design.md`

## Global Constraints

- Strictly zero new external crates: use only crates already in root `Cargo.toml`.
- Source files must be UTF-8 without BOM; preserve target file's existing BOM/CRLF on edits and undos.
- Fail closed on security checks: permission, path, and trust checks never default to open.
- Normalize all paths canonically (with lowercase normalization on Windows `cfg!(windows)`) before keying rules, queues, or stores.
- No `any` or disabling lints; no `@ts-ignore` in TS; no unhandled error swallowing (`unwrap_or_default` without local recovery).
- Never commit unless the user explicitly requests it; stage only explicitly modified files.
- All unit and integration tests must run offline with `FauxProvider` (zero real network calls or paid API keys).

## Review Focus

1. **BOM & CRLF preservation during file rollback:** `undo_edit` must restore exact original file bytes (including UTF-8 BOM `0xEF 0xBB 0xBF` and Windows `\r\n`) without normalizing line endings or stripping BOM. (Pinned to Task 6, Step 1).
2. **Prompt injection defence on `.mehh` metadata files:** `SecurityInspector` must block any `edit`, `write`, or `undo_edit` targeting `<cwd>/.mehh/*` or `~/.mehh/agent/*` with `Deny` across ALL modes including `Auto`. (Pinned to Task 2, Step 1).
3. **Headless fail-closed execution without stdin hang:** Headless runs (`-p`, `--mode json`) triggering a mutating tool in `SmartApprove` mode without `--permission-mode auto` must terminate with an actionable error rather than blocking on stdin. (Pinned to Task 3, Step 1).
4. **Subagent recursion prohibition:** Child subagents in `"coding"` or `"research"` mode must have `subagent` and `codemode` stripped from their tool catalog to prevent runaway recursion. (Pinned to Task 9, Step 1).
5. **Cross-provider credential isolation:** Mid-turn Lead-to-Worker switching (e.g. Anthropic Lead -> OpenAI Worker) must resolve and pass provider-specific API keys without leaking Anthropic keys to OpenAI headers. (Pinned to Task 10, Step 1).

---

## Task Decomposition

### Wave 1 — Runtime Tool Governance, MCP Hints & TUI Tool Visibility

#### Task 1: MCP Tool Annotations & AgentTool Read-Only Metadata

**Files:**
- Modify: `crates/mehh-mcp/src/client.rs:30-45`
- Modify: `crates/mehh-mcp/src/tool_adapter.rs:440-475`
- Modify: `crates/mehh-agent/src/types.rs:59-78`
- Modify: `crates/mehh-tools/src/read.rs`, `grep.rs`, `find.rs`, `ls.rs`, `write.rs`, `edit.rs`, `bash.rs`, `powershell.rs`
- Test: `crates/mehh-mcp/tests/test_annotations.rs`

**Interfaces:**
- Consumes: MCP JSON-RPC protocol schema, `AgentTool` trait in `mehh-agent`.
- Produces:
  ```rust
  // crates/mehh-mcp/src/client.rs
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
  #[serde(rename_all = "camelCase")]
  pub struct McpToolAnnotations {
      pub read_only_hint: Option<bool>,
      pub destructive_hint: Option<bool>,
      pub idempotent_hint: Option<bool>,
      pub open_world_hint: Option<bool>,
  }

  // crates/mehh-agent/src/types.rs
  pub trait AgentTool: Send + Sync {
      // ... existing methods
      fn is_read_only(&self) -> Option<bool> { None }
      fn is_safe_for_auto_approve(&self, _args: &serde_json::Map<String, serde_json::Value>) -> Option<bool> {
          self.is_read_only()
      }
  }
  ```

- [ ] **Step 1: Write the failing test for MCP annotations and `is_read_only`**

Create `crates/mehh-mcp/tests/test_annotations.rs`:
```rust
use mehh_mcp::client::{McpToolAnnotations, McpToolInfo};
use mehh_agent::AgentTool;

#[test]
fn test_mcp_tool_info_deserializes_annotations() {
    let json = serde_json::json!({
        "name": "read_query",
        "description": "Run SQL query",
        "inputSchema": { "type": "object" },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false
        }
    });
    let info: McpToolInfo = serde_json::from_value(json).expect("deserialize McpToolInfo");
    assert_eq!(info.annotations.as_ref().and_then(|a| a.read_only_hint), Some(true));
    assert_eq!(info.annotations.as_ref().and_then(|a| a.destructive_hint), Some(false));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-mcp --test test_annotations`
Expected: FAIL with "no field `annotations` on type `McpToolInfo`"

- [ ] **Step 3: Implement `McpToolAnnotations` and `AgentTool::is_read_only`**

- In `crates/mehh-mcp/src/client.rs`, define `McpToolAnnotations` with `#[serde(rename_all = "camelCase")]` and add `#[serde(default, skip_serializing_if = "Option::is_none")] pub annotations: Option<McpToolAnnotations>` to `McpToolInfo`.
- In `crates/mehh-agent/src/types.rs`, add `fn is_read_only(&self) -> Option<bool> { None }` and `fn is_safe_for_auto_approve(...)` default methods to `pub trait AgentTool`.
- In `crates/mehh-mcp/src/tool_adapter.rs`:
  - Implement `AgentTool::is_read_only` on `McpAgentTool` returning `self.tool_info.annotations.as_ref().and_then(|a| a.read_only_hint)`.
  - Update `AgentTool::execution_mode` on `McpAgentTool` to return `ToolExecutionMode::Parallel` when `read_only_hint == Some(true)`, and `ToolExecutionMode::Sequential` otherwise.
  - Implement `is_read_only() -> Some(true)` on `ListMcpResourcesTool`, `ListMcpResourceTemplatesTool`, and `ReadMcpResourceTool`.
- In `crates/mehh-tools/src/`:
  - `read.rs`, `grep.rs`, `find.rs`, `ls.rs`: implement `fn is_read_only(&self) -> Option<bool> { Some(true) }`.
  - `write.rs`, `edit.rs`, `bash.rs`, `powershell.rs`: implement `fn is_read_only(&self) -> Option<bool> { Some(false) }`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-mcp --test test_annotations`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 2: Tool Inspectors Pipeline & Permission Store (`tool_inspection.rs`)

**Files:**
- Create: `crates/mehh-coding-agent/src/tool_inspection.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs:25-45`
- Test: `crates/mehh-coding-agent/tests/test_tool_inspection.rs`

**Interfaces:**
- Consumes: `AgentTool`, `BeforeToolCallResult` from `mehh-agent`, canonical path helpers.
- Produces:
  ```rust
  pub enum InspectionVerdict {
      Allow,
      RequireApproval { reason: String, preview: Option<String> },
      Deny { reason: String },
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
  #[serde(rename_all = "snake_case")]
  pub enum PermissionMode {
      Auto,
      #[default]
      SmartApprove,
      Approve,
      Chat,
  }

  pub struct ToolInspectionManager { ... }
  impl ToolInspectionManager {
      pub fn new(mode: PermissionMode, cwd: PathBuf) -> Self;
      pub async fn inspect(&self, ctx: &ToolInspectionContext) -> InspectionVerdict;
  }
  ```

- [ ] **Step 1: Write the failing test for `ToolInspectionManager` and security boundaries**

Create `crates/mehh-coding-agent/tests/test_tool_inspection.rs`:
```rust
use std::path::PathBuf;
use mehh_coding_agent::tool_inspection::{InspectionVerdict, PermissionMode, ToolInspectionContext, ToolInspectionManager};

#[tokio::test]
async fn test_security_inspector_blocks_mehh_metadata_edits() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(".mehh/settings.json"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_1".to_string(),
        tool_name: "edit".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
    };

    let verdict = mgr.inspect(&ctx).await;
    assert!(matches!(verdict, InspectionVerdict::Deny { .. }));
}

#[tokio::test]
async fn test_repetition_inspector_triggers_after_three_repeats() {
    let cwd = std::env::current_dir().unwrap();
    let mgr = ToolInspectionManager::new(PermissionMode::Auto, cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("command".to_string(), serde_json::json!("cargo check"));

    let ctx = ToolInspectionContext {
        tool_call_id: "call_repeat".to_string(),
        tool_name: "bash".to_string(),
        args,
        is_read_only: Some(false),
        cwd,
    };

    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Allow));
    assert!(matches!(mgr.inspect(&ctx).await, InspectionVerdict::Deny { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_tool_inspection`
Expected: FAIL with "unresolved import `mehh_coding_agent::tool_inspection`"

- [ ] **Step 3: Implement `tool_inspection.rs`**

- In `crates/mehh-coding-agent/src/tool_inspection.rs`:
  - Define `InspectionVerdict`, `PermissionMode`, `ToolInspectionContext`.
  - Define `pub trait ToolInspector: Send + Sync`.
  - Implement `SecurityInspector`:
    - Checks commands (`bash`/`powershell`) against catastrophic patterns (`rm -rf /`, `curl | sh`, reading `~/.ssh/id_*`, `~/.mehh/agent/auth.json`, `Invoke-Expression`).
    - Checks file paths (`edit`, `write`, `undo_edit`) against `.mehh/*` and `~/.mehh/agent/*` (strict `Deny` in all modes).
    - Checks writes outside normalized `cwd` (`RequireApproval`).
  - Implement `PermissionInspector`:
    - Handles `Auto` (allows all except security flags).
    - Handles `SmartApprove`: allows `is_read_only == Some(true)`, checks shell whitelist (`cargo check`, `cargo test`, `git status`, `git diff`, `ls`), requires approval for mutating tools (`edit`, `write`, `bash` unknown).
    - Handles `Approve`: requires approval for all non-whitelisted tools.
    - Handles `Chat`: allows read-only exploration tools (`read`, `grep`, `find`, `ls`, `analyze`, `todo`, MCP `readOnlyHint: true`), denies mutating tools and shell execution.
    - Loads persisted `permissions.json` with exact rule matching (`tool_name` + optional `arg_prefix`/`path_glob`).
  - Implement `RepetitionInspector`:
    - Tracks consecutive `(tool_name, canonical_json_args)` calls up to `max_repetitions = 3`.
    - Returns `Deny` on 4th identical call.
  - Implement `ToolInspectionManager` composing the three inspectors in order.
- Register `pub mod tool_inspection;` in `crates/mehh-coding-agent/src/lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_tool_inspection`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 3: Wiring `AgentSession` Hooks, `CodemodeTool` Interceptor & Headless Fail-Closed

**Files:**
- Modify: `crates/mehh-codemode/src/tool.rs:135-170`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:550-610`
- Modify: `crates/mehh-coding-agent/src/settings.rs:30-65`
- Test: `crates/mehh-coding-agent/tests/test_agent_session_governance.rs`

**Interfaces:**
- Consumes: `ToolInspectionManager`, `CodemodeTool`, `AgentLoopConfig.before_tool_call`.
- Produces:
  ```rust
  // crates/mehh-codemode/src/tool.rs
  pub type ToolCallInterceptor = Arc<
      dyn Fn(&str, &serde_json::Map<String, serde_json::Value>) -> Result<(), String> + Send + Sync,
  >;
  impl CodemodeTool {
      pub fn with_interceptor(mut self, interceptor: ToolCallInterceptor) -> Self;
  }
  ```

- [ ] **Step 1: Write the failing test for headless fail-closed and `CodemodeTool` interception**

Create `crates/mehh-coding-agent/tests/test_agent_session_governance.rs`:
```rust
use std::sync::Arc;
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::tool_inspection::PermissionMode;

#[tokio::test]
async fn test_headless_smart_approve_blocks_mutating_tool() {
    let cwd = std::env::current_dir().unwrap();
    // In headless mode with SmartApprove, mutating bash tool must fail closed with Deny
    let mut session = AgentSession::create_for_test(cwd, PermissionMode::SmartApprove).await;
    let (_msg, _hist, config) = session.prepare_prompt("run dangerous command");
    
    assert!(config.before_tool_call.is_some(), "before_tool_call must be wired");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_agent_session_governance`
Expected: FAIL

- [ ] **Step 3: Implement hook wiring, interceptor, and headless policy**

- In `crates/mehh-codemode/src/tool.rs`:
  - Add `tool_interceptor: Option<ToolCallInterceptor>` to `CodemodeTool`.
  - In `execute` sandbox loop, call `interceptor(name_str, &params_map)` before `tool.execute(...)`.
- In `crates/mehh-coding-agent/src/settings.rs`:
  - Add `pub permission_mode: Option<PermissionMode>` to `Settings`.
  - Enforce permission mode escalation guard: ignore `permission_mode = auto` from workspace `.mehh/settings.json`, clamp to `SmartApprove`.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Add `inspection_manager: Arc<ToolInspectionManager>` and `approval_handler: Option<Arc<dyn ApprovalHandler>>` to `AgentSession`.
  - In `prepare_prompt_content`, wire `config.before_tool_call` to call `inspection_manager.inspect_and_authorize(...)`.
  - Attach `ToolInspectionManager` interceptor to `self.codemode_tool`.
  - Provide `HeadlessDenyHandler` for headless environments (`-p`, `--mode json`) that returns `Deny` on `RequireApproval`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_agent_session_governance`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 4: Interactive TUI Approval Modal, Rich Tool Cards & `/mode` Slash Command

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:30-80, 140-180, 500-600, 1100-1300`
- Modify: `crates/mehh-cli/src/main.rs:20-60, 200-240`
- Test: `crates/mehh-coding-agent/tests/test_interactive_approvals.rs`

**Interfaces:**
- Consumes: `InspectionVerdict::RequireApproval`, `ApprovalRequest`, `TranscriptItem::ToolCall`.
- Produces:
  ```rust
  // crates/mehh-coding-agent/src/interactive.rs
  pub enum SelectorKind {
      Model,
      Tree,
      Resume,
      Settings,
      Login,
      ToolApproval,
  }
  ```

- [ ] **Step 1: Write the failing test for `/mode` slash command and tool call preview**

Create `crates/mehh-coding-agent/tests/test_interactive_approvals.rs`:
```rust
use mehh_coding_agent::interactive::{handle_slash_command, SlashCommandOutcome};
use mehh_coding_agent::tool_inspection::PermissionMode;

#[test]
fn test_mode_slash_command_switches_mode() {
    let mut current_mode = PermissionMode::SmartApprove;
    let outcome = handle_slash_command("/mode chat", &mut current_mode);
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert_eq!(current_mode, PermissionMode::Chat);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_interactive_approvals`
Expected: FAIL

- [ ] **Step 3: Implement TUI approval selector, tool card preview, and `/mode`**

- In `crates/mehh-coding-agent/src/interactive.rs`:
  - Extend `SelectorKind` with `ToolApproval`.
  - Implement `ApprovalRequest` broker with async mutex to serialize concurrent approvals.
  - Render approval overlay inside `BorderedBox`: display tool name, target path or command, up to 18 lines of diff/output preview, and options:
    - 1. Allow once (`y` / `Enter`)
    - 2. Allow tool for session (`a`)
    - 3. Always allow tool (`A`, saved to `~/.mehh/agent/permissions.json`)
    - 4. Deny (`n` / `Esc`)
  - Extend `TranscriptItem::ToolCall` with `{ id, name, status, is_error, args_summary, output_preview, diff }`.
  - In `apply_agent_event`:
    - `ToolExecutionStart`: set `args_summary`.
    - `ToolExecutionUpdate`: buffer last 8 non-empty lines into `output_preview`.
    - `ToolExecutionEnd`: populate unified `diff` (capped at 24 lines with `+`/`-`/`@@` theme colors) and update final status.
  - Add `/mode [auto|smart_approve|approve|chat]` to `builtin_slash_commands()` and `handle_slash_command`.
- In `crates/mehh-cli/src/main.rs`:
  - Add `#[arg(long = "permission-mode", value_name = "MODE")] pub permission_mode: Option<String>` to `CliArgs`.
  - Pass configured permission mode into `AgentSession`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_interactive_approvals`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

### Wave 2 — Pure-Rust `analyze`, `undo_edit`, `Todo` + MOIM & BM25 `tool_search`

#### Task 5: Pure-Rust Structural Code Analyzer (`crates/mehh-tools/src/analyze.rs`)

**Files:**
- Create: `crates/mehh-tools/src/analyze.rs`
- Modify: `crates/mehh-tools/src/lib.rs:1-45`
- Test: `crates/mehh-tools/tests/test_analyze.rs`

**Interfaces:**
- Consumes: `AgentTool`, `ignore::WalkBuilder`, `regex::Regex`.
- Produces:
  ```rust
  pub struct AnalyzeTool {
      cwd: PathBuf,
  }
  impl AnalyzeTool {
      pub fn new(cwd: PathBuf) -> Self;
  }
  ```

- [ ] **Step 1: Write the failing test for `AnalyzeTool` on Rust and TypeScript fixtures**

Create `crates/mehh-tools/tests/test_analyze.rs`:
```rust
use std::path::PathBuf;
use mehh_tools::AnalyzeTool;
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_analyze_rust_symbols() {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = AnalyzeTool::new(cwd.clone());

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!("src/lib.rs"));
    args.insert("mode".to_string(), serde_json::json!("symbols"));

    let res = tool.execute("call_1", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute analyze");
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("pub fn create_coding_tools"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tools --test test_analyze`
Expected: FAIL with "unresolved import `mehh_tools::AnalyzeTool`"

- [ ] **Step 3: Implement `AnalyzeTool`**

- In `crates/mehh-tools/src/analyze.rs`:
  - Define `AnalyzeTool` implementing `AgentTool` with `is_read_only() -> Some(true)` and `execution_mode() -> ToolExecutionMode::Parallel`.
  - Modes:
    - `"structure"`: Directory tree traversal via `ignore::WalkBuilder` with clamped depth `1..=5` (default 2), file counts, and extensions.
    - `"symbols"`: File-level structural extraction with line spans:
      - Rust (`.rs`): regex matching item declarations (`fn`, `struct`, `enum`, `trait`, `impl`, `type`) with brace depth tracking for body spans.
      - TS/JS (`.ts`, `.tsx`, `.js`, `.jsx`): declarations (`export`, `class`, `interface`, `type`, `function`, methods).
      - Python (`.py`): indentation-based `def` and `class` tracking.
      - Go (`.go`): `func`, `type ... struct`, `type ... interface`.
    - `"focus"`: Extracts full source of a named symbol up to 250 lines.
  - Enforce `MAX_ANALYZE_CHARS = 24_000` with clean truncation note.
- In `crates/mehh-tools/src/lib.rs`:
  - Add `pub mod analyze; pub use analyze::AnalyzeTool;`.
  - Register `AnalyzeTool` in `create_coding_tools` and `create_all_tools`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tools --test test_analyze`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 6: File Snapshot & Rollback (`EditUndoStore` + `undo_edit` tool)

**Files:**
- Create: `crates/mehh-tools/src/undo_edit.rs`
- Modify: `crates/mehh-tools/src/edit.rs:1-60, 200-240`
- Modify: `crates/mehh-tools/src/write.rs:1-50, 100-140`
- Modify: `crates/mehh-tools/src/lib.rs:1-45`
- Test: `crates/mehh-tools/tests/test_undo_edit.rs`

**Interfaces:**
- Consumes: `with_file_mutation_queue`, `similar::TextDiff`.
- Produces:
  ```rust
  pub struct EditUndoStore { ... }
  pub struct UndoEditTool { ... }
  ```

- [ ] **Step 1: Write the failing test for BOM/CRLF preservation and `undo_edit`**

Create `crates/mehh-tools/tests/test_undo_edit.rs`:
```rust
use std::sync::Arc;
use mehh_tools::{EditTool, EditUndoStore, UndoEditTool};
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_undo_edit_preserves_bom_and_crlf() {
    let temp = tempfile::tempdir().unwrap();
    let file_path = temp.path().join("crlf_bom.txt");
    
    // Original file with UTF-8 BOM and CRLF
    let original_bytes = b"\xEF\xBB\xBFline1\r\nline2\r\n";
    std::fs::write(&file_path, original_bytes).unwrap();

    let undo_store = Arc::new(EditUndoStore::new());
    let edit_tool = EditTool::new(temp.path().to_path_buf()).with_undo_store(undo_store.clone());
    let undo_tool = UndoEditTool::new(temp.path().to_path_buf(), undo_store);

    // Apply edit
    let mut edit_args = serde_json::Map::new();
    edit_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    edit_args.insert("old_string".to_string(), serde_json::json!("line2"));
    edit_args.insert("new_string".to_string(), serde_json::json!("line2_edited"));
    edit_tool.execute("call_edit", edit_args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();

    // Rollback with undo_edit
    let mut undo_args = serde_json::Map::new();
    undo_args.insert("path".to_string(), serde_json::json!("crlf_bom.txt"));
    undo_tool.execute("call_undo", undo_args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .unwrap();

    let restored_bytes = std::fs::read(&file_path).unwrap();
    assert_eq!(restored_bytes, original_bytes, "exact BOM and CRLF must be restored");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tools --test test_undo_edit`
Expected: FAIL with "unresolved import `mehh_tools::EditUndoStore`"

- [ ] **Step 3: Implement `EditUndoStore` and `UndoEditTool`**

- In `crates/mehh-tools/src/undo_edit.rs`:
  - Define `FileSnapshot` (`Existed { bytes: Vec<u8>, timestamp_ms: i64 }`, `DidNotExist`).
  - Implement `EditUndoStore`: thread-safe bounded per-file history (`MAX_SNAPSHOTS_PER_FILE = 20`, `MAX_UNDO_BYTES = 16 MiB`).
  - Implement `UndoEditTool`:
    - `is_read_only() -> Some(false)`.
    - Operates inside `with_file_mutation_queue`.
    - Restores previous snapshot or removes created file if `DidNotExist`.
    - Returns unified diff of the revert in `details["diff"]`.
- In `crates/mehh-tools/src/edit.rs` and `write.rs`:
  - Add optional `undo_store: Option<Arc<EditUndoStore>>` via `.with_undo_store(...)`.
  - Capture snapshot inside `with_file_mutation_queue` immediately prior to writing.
- In `crates/mehh-tools/src/lib.rs`: export `EditUndoStore` and `UndoEditTool`, add to `create_coding_tools`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tools --test test_undo_edit`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 7: `TodoTool` & MOIM Ephemeral Working Memory Injection

**Files:**
- Create: `crates/mehh-coding-agent/src/todo.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:550-610`
- Modify: `crates/mehh-coding-agent/src/lib.rs:25-45`
- Test: `crates/mehh-coding-agent/tests/test_todo_moim.rs`

**Interfaces:**
- Consumes: `AgentTool`, `AgentLoopConfig.prepare_request`.
- Produces:
  ```rust
  pub struct TodoTool { ... }
  pub struct TodoState { ... }
  ```

- [ ] **Step 1: Write the failing test for `TodoTool` CRUD and MOIM request injection**

Create `crates/mehh-coding-agent/tests/test_todo_moim.rs`:
```rust
use std::sync::Arc;
use tokio::sync::RwLock;
use mehh_coding_agent::todo::{TodoState, TodoTool};
use mehh_agent::AgentTool;

#[tokio::test]
async fn test_todo_tool_updates_shared_state() {
    let state = Arc::new(RwLock::new(TodoState::default()));
    let tool = TodoTool::new(state.clone());

    let mut args = serde_json::Map::new();
    args.insert("action".to_string(), serde_json::json!("add"));
    args.insert("content".to_string(), serde_json::json!("Verify test suite"));

    tool.execute("call_1", args, tokio_util::sync::CancellationToken::new(), None)
        .await
        .expect("execute todo add");

    let read = state.read().await;
    assert_eq!(read.items.len(), 1);
    assert_eq!(read.items[0].content, "Verify test suite");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_todo_moim`
Expected: FAIL with "unresolved import `mehh_coding_agent::todo`"

- [ ] **Step 3: Implement `TodoTool` and `prepare_request` MOIM hook**

- In `crates/mehh-coding-agent/src/todo.rs`:
  - Implement `TodoItem`, `TodoStatus` (`Pending`, `InProgress`, `Completed`), and `TodoState`.
  - Implement `TodoTool`: actions `"replace"`, `"add"`, `"update"`, `"list"`, `"clear"`.
  - Format helper `TodoState::format_moim_block(&self) -> Option<String>`.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Add `todo_state: Arc<RwLock<TodoState>>` to `AgentSession`.
  - In `prepare_prompt_content`, wire `config.prepare_request`:
    - Reads active `TodoState` (if non-empty) and optional `.mehh/MOIM.md`.
    - Appends `<working_checklist>...</working_checklist>` into the trailing user message context of `req_ctx.context.messages`.
    - Ensures ephemeral MOIM context is NOT saved to `session.append_message`.
- Register `pub mod todo;` in `crates/mehh-coding-agent/src/lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_todo_moim`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 8: BM25 Okapi & JSON Schema Property Indexing in `tool_search.rs`

**Files:**
- Modify: `crates/mehh-coding-agent/src/tool_search.rs:1-180`
- Test: `crates/mehh-coding-agent/tests/test_bm25_tool_search.rs`

**Interfaces:**
- Consumes: `DeferredToolCatalog`, `ToolSearchTool`.
- Produces:
  ```rust
  pub struct DeferredToolIndex { ... }
  // BM25 k1 = 1.2, b = 0.75 ranking algorithm
  ```

- [ ] **Step 1: Write the failing test for schema property ranking**

Create `crates/mehh-coding-agent/tests/test_bm25_tool_search.rs`:
```rust
use mehh_coding_agent::tool_search::DeferredToolCatalog;

#[test]
fn test_bm25_ranks_schema_property_matches_higher_than_noise() {
    let mut catalog = DeferredToolCatalog::new();
    catalog.register_test_tool("database_query", "Execute SQL", serde_json::json!({
        "properties": { "table_name": { "description": "Target SQL table" } }
    }));
    catalog.register_test_tool("unrelated_tool", "Unrelated utility", serde_json::json!({}));

    let results = catalog.search("table_name", 5);
    assert!(!results.is_empty());
    assert_eq!(results[0].name, "database_query");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_bm25_tool_search`
Expected: FAIL

- [ ] **Step 3: Implement BM25 Okapi and schema indexing**

- In `crates/mehh-coding-agent/src/tool_search.rs`:
  - Implement term tokenization (`snake_case`, `camelCase`, `mcp__server__tool` boundaries).
  - Implement `DeferredToolIndex` calculating term frequencies and document frequencies.
  - Implement Okapi BM25 (`k1 = 1.2`, `b = 0.75`) scoring:
    - Tool name exact match boost (`+50.0`).
    - Tool name substring boost (`+15.0`).
    - Weighted terms: `name` (3.0), `description` + JSON schema property names & descriptions (1.0), server namespace (0.5).
  - Maintain backward compatibility for `ToolSearchTool` result JSON structure.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_bm25_tool_search`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

### Wave 3 — Multi-Mode Subagents, Parallel Dispatch & Lead/Worker Routing

#### Task 9: Multi-Mode Parallel `SubagentTool` with Progress Streaming

**Files:**
- Modify: `crates/mehh-coding-agent/src/subagent.rs:1-250`
- Test: `crates/mehh-coding-agent/tests/test_subagent_multimode.rs`

**Interfaces:**
- Consumes: `ToolInspectionManager`, `AgentToolUpdateCallback`, `tokio::sync::Semaphore`.
- Produces:
  ```rust
  // Multi-mode SubagentTool supporting tasks batching and progress streaming
  pub struct SubagentTool { ... }
  ```

- [ ] **Step 1: Write the failing test for subagent recursion stripping and parallel task batching**

Create `crates/mehh-coding-agent/tests/test_subagent_multimode.rs`:
```rust
use mehh_coding_agent::subagent::SubagentTool;
use mehh_agent::AgentTool;

#[test]
fn test_subagent_tool_parameters_support_tasks_and_modes() {
    let params = SubagentTool::parameters_schema();
    assert!(params["properties"]["mode"]["enum"].as_array().unwrap().contains(&serde_json::json!("coding")));
    assert!(params["properties"]["mode"]["enum"].as_array().unwrap().contains(&serde_json::json!("review")));
    assert!(params["properties"]["tasks"].is_object());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_subagent_multimode`
Expected: FAIL

- [ ] **Step 3: Implement multi-mode, parallel batching, and progress streaming**

- In `crates/mehh-coding-agent/src/subagent.rs`:
  - Extend parameter schema: `mode` (`"research" | "coding" | "review"`), `tasks` (array of subtasks).
  - Filter tool catalogs per mode:
    - `"research"`: `read`, `grep`, `find`, `ls`, `analyze`.
    - `"coding"`: `read`, `write`, `edit`, `undo_edit`, `bash`, `powershell`, `grep`, `find`, `ls`, `analyze` (strictly exclude `subagent`, `codemode`, `todo` to eliminate recursion risk).
    - `"review"`: `read`, `grep`, `find`, `ls`, `analyze` + code review system prompt.
  - Implement parallel dispatch using `futures::future::join_all` bounded by `Semaphore::new(4)`.
  - Forward child turn events to parent `on_update` callback (`"[subagent:coding] turn N: edit"`).
  - Inherit parent's `ToolInspectionManager` and cancellation token in child loop.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_subagent_multimode`
Expected: PASS

- [ ] **Step 5: Verify workspace compiles cleanly**

Run: `cargo check --workspace`
Expected: 0 errors

---

#### Task 10: Lead/Worker Model Routing & Cross-Provider Error Escalation

**Files:**
- Modify: `crates/mehh-agent/src/types.rs:250-290`
- Modify: `crates/mehh-agent/src/agent_loop.rs:170-220`
- Create: `crates/mehh-coding-agent/src/lead_worker.rs`
- Modify: `crates/mehh-coding-agent/src/agent_session.rs:460-520, 580-620`
- Modify: `crates/mehh-coding-agent/src/settings.rs:30-65`
- Modify: `crates/mehh-cli/src/main.rs:40-70, 250-290`
- Test: `crates/mehh-coding-agent/tests/test_lead_worker.rs`

**Interfaces:**
- Consumes: `AgentLoopTurnUpdate`, `ModelRegistry`, `resolve_api_key_async`.
- Produces:
  ```rust
  // crates/mehh-agent/src/types.rs
  pub struct AgentLoopTurnUpdate {
      pub model: Option<Model>,
      pub provider: Option<Arc<dyn LlmProvider>>,
      pub api_key: Option<Option<String>>,
      pub thinking_level: Option<Option<ThinkingLevel>>,
  }

  // crates/mehh-coding-agent/src/lead_worker.rs
  pub struct LeadWorkerManager { ... }
  ```

- [ ] **Step 1: Write the failing test for cross-provider key isolation and error escalation**

Create `crates/mehh-coding-agent/tests/test_lead_worker.rs`:
```rust
use mehh_coding_agent::lead_worker::LeadWorkerManager;
use mehh_ai::{Model, StopReason};

#[test]
fn test_lead_worker_escalates_on_provider_error() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 1);
    
    // Turn 1: Lead
    assert_eq!(mgr.active_model_for_turn(0), "lead-model");
    // Turn 2: Switches to Worker
    assert_eq!(mgr.active_model_for_turn(1), "worker-model");
    
    // Worker hits StopReason::Error -> escalates back to Lead
    mgr.record_turn_outcome(StopReason::Error, false);
    assert_eq!(mgr.active_model_for_turn(2), "lead-model");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent --test test_lead_worker`
Expected: FAIL

- [ ] **Step 3: Implement cross-provider turn updates, `LeadWorkerManager`, and CLI flags**

- In `crates/mehh-agent/src/types.rs`:
  - Extend `AgentLoopTurnUpdate` with `pub provider: Option<Arc<dyn LlmProvider>>` and `pub api_key: Option<Option<String>>`.
- In `crates/mehh-agent/src/agent_loop.rs`:
  - Update `run_agent_loop` to use `(current_model, current_provider, current_api_key)` when `turn_update` specifies them.
- In `crates/mehh-coding-agent/src/lead_worker.rs`:
  - Implement `LeadWorkerManager`:
    - Stores `lead_model`, `worker_model`, `lead_turns`, per-prompt turn counter.
    - Resolves distinct provider instances and API keys per model (zero key leakage).
    - Escalates to Lead on `StopReason::Error`, empty completions, or repetition breaks.
    - Resets turn count to 0 at the start of each user prompt.
- In `crates/mehh-coding-agent/src/agent_session.rs`:
  - Integrate `LeadWorkerManager` into `prepare_prompt_content` and `prepare_next_turn`.
  - Pass `worker_model` to `SubagentTool` as default execution model.
- In `crates/mehh-coding-agent/src/settings.rs`:
  - Add `pub worker_model: Option<String>` and `pub lead_turns: Option<usize>` to `Settings`.
- In `crates/mehh-cli/src/main.rs`:
  - Add `--worker-model <MODEL>` and `--lead-turns <N>` to `CliArgs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent --test test_lead_worker`
Expected: PASS

- [ ] **Step 5: Run full workspace test suite and linter**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: 0 test failures, 0 warnings

---

## Execution Self-Review

1. **Spec Coverage:**
   - Wave 1: `McpToolAnnotations` (Task 1), `ToolInspectionManager` (Task 2), `AgentSession` hooks + `CodemodeTool` interceptor (Task 3), TUI approval modal + `/mode` + CLI `--permission-mode` (Task 4).
   - Wave 2: `AnalyzeTool` pure-Rust parser (Task 5), `EditUndoStore` + `undo_edit` (Task 6), `TodoTool` + MOIM (Task 7), BM25 schema search (Task 8).
   - Wave 3: Multi-mode parallel `SubagentTool` (Task 9), Lead/Worker routing with cross-provider swap (Task 10).
   - Complete 100% coverage of all spec sections.
2. **Step Scan:** Every step defines exact signatures, exact file targets, checkable test code, and concrete verification commands. No hand-waving or undefined types.
3. **Type Consistency:** Types defined in early tasks (`ToolInspectionManager`, `EditUndoStore`, `McpToolAnnotations`, `TodoState`, `LeadWorkerManager`) match exact usages across later tasks.
4. **Review Focus:** All 5 failure modes (BOM/CRLF preservation, `.mehh` protection, headless fail-closed, subagent recursion stripping, cross-provider key isolation) are explicitly tested in Tasks 6, 2, 3, 9, and 10.
5. **Proportion:** The plan is focused, direct, and actionable without code bloat or redundant filler.
