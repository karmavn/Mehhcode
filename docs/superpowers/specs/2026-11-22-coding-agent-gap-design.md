---

# Phần 1 — Architecture map

## 1.1 Bảng Hiện tại → Đề xuất

| Hạng mục | Hiện tại (đã xác thực trong repo) | Đề xuất thiết kế & File tác động |
| --- | --- | --- |
| **A1 — Plan mode** | Chỉ có hằng số `EnterPlanMode`/`ExitPlanMode` trong `CLAUDE_CODE_TOOLS`; `PermissionMode` gồm 4 chế độ (`Auto`, `SmartApprove`, `Approve`, `Chat`); `todo` + `<working_checklist>` không khóa quyền ghi file.

 | Thêm `PermissionMode::Plan` trong `crates/mehh-coding-agent/src/tool_inspection.rs`; thêm module **[Mới]** `crates/mehh-coding-agent/src/plan_mode.rs` (`PlanModeState`, `EnterPlanModeTool`, `ExitPlanModeTool`); gắn vào `AgentSession` (`agent_session.rs`), `prepare_request`, `/plan` + `SelectorKind::PlanApproval` (`interactive.rs`), `TuiAction::TogglePlanMode` (`backtab` trong `keys.rs`), alias OAuth trong `anthropic.rs`. |
| **A2 — User hooks** | Chỉ có 6 hook closure nội bộ trên `AgentLoopConfig` (`before_tool_call`, `after_tool_call`, `prepare_request`, `prepare_next_turn`, `transform_context`, `finish_turn`), trong đó `finish_turn` và `transform_context` đang là `None` trên `AgentSession`.

 | Thêm module **[Mới]** `crates/mehh-coding-agent/src/hooks.rs` (`HooksConfigFile`, `HookEvent`, `HookRunner`); cập nhật `has_local_trust_targets` (`trust.rs`) để gate `.mehh/hooks.json`; nối `PreToolUse` vào `before_tool_call`, `PostToolUse` vào `after_tool_call`, `Stop` vào `finish_turn`, `SessionStart`/`Notification` vào `AgentSession` và `interactive.rs`. |
| **A3 — WebFetch / WebSearch** | Chỉ có hằng số `WebFetch`/`WebSearch` trong `anthropic.rs`; chưa có tool mạng hay network policy.

 | Thêm 3 module **[Mới]** trong `mehh-tools`: `crates/mehh-tools/src/net_policy.rs` (SSRF + domain allow/deny + redirect hop guard + secret redactor), `crates/mehh-tools/src/web_fetch.rs` (`WebFetchTool`), `crates/mehh-tools/src/web_search.rs` (`WebSearchTool`); thêm `NetworkSettings` vào `settings.rs`; bổ sung rule URL vào `SecurityInspector` (`tool_inspection.rs`). |
| **A4 — LSP / Code intelligence** | `AnalyzeTool` (`crates/mehh-tools/src/analyze.rs`) dùng regex tĩnh cho Rust/TS/Python/Go, không cross-file definition/references/diagnostics.

 | Giữ nguyên `AnalyzeTool` cho outline nhanh zero-setup; thêm module **[Mới]** `crates/mehh-tools/src/lsp.rs` (client JSON-RPC 2.0 `Content-Length` stdio thuần Rust, **0 crate mới**, spawn lazy `rust-analyzer`/`typescript-language-server`/`pyright-langserver`/`gopls`, expose `LspTool`); cấu hình `LspSettings` trong `settings.rs`. |
| **A5 — `apply_patch` + Per-hunk** | `EditTool` (`edit.rs`) áp toàn bộ edit; `ApprovalHandler::request_approval` chỉ trả `bool`; diff trong `interactive.rs` chỉ đọc.

 | Tách helper BOM/line-ending từ `edit.rs` sang **[Mới]** `crates/mehh-tools/src/core/text.rs`; thêm **[Mới]** `crates/mehh-tools/src/apply_patch.rs` (`ApplyPatchTool`); mở rộng `BeforeToolCallResult` thêm `modified_args` (`mehh-agent/src/types.rs` + `agent_loop.rs`); thêm `ApprovalOutcome` & `request_approval_ext` (`tool_inspection.rs`); thêm `SelectorKind::HunkApproval` riêng trong `interactive.rs` (**không đụng** `interactive.rs:3316-3352`). |
| **A6 — Background shell** | `run_shell` (`core/shell.rs`) luôn chặn chờ process thoát hoặc timeout rồi `kill_process_tree` sau 500ms; `KillShell` chỉ là hằng số.

 | Thêm **[Mới]** `crates/mehh-tools/src/bg_shell.rs` (`BackgroundShellManager`, `ShellOutputTool`, `KillShellTool`); mở rộng `BashTool` (`bash.rs`) thêm param `run_in_background: Option<bool>`; gắn cleanup khi thoát session vào `agent_session.rs`, `interactive.rs`, `mehh-cli/src/main.rs`; thêm `/shells` + badge footer. |
| **A7 — Interactive session switcher** | Đã có spec/plan ngày 17/11 nhưng chưa implement trong code (`SelectorKind` có 6 loại, `TuiAction` chưa có `SessionPicker`, chưa có `/sessions`).

 | Triển khai & vá lỗi từ spec 17/11: thêm `TuiAction::SessionPicker` (`ctrl+s` trong `keys.rs`); thêm `parse_session_summary`, `list_all_sessions`, `delete_session_file` (canonical containment + chống symlink trong `session.rs`); nâng cấp `SelectorKind::Resume` (`SessionPickerScope`, `pending_delete_id`, dùng `delete`/`ctrl+d` thay vì `d` để không xung đột fuzzy filter) trong `interactive.rs`. |
| **A8 — Auto-memory** | `TodoState::format_moim_block` (`todo.rs`) chỉ đọc `<cwd>/.mehh/MOIM.md`, không có công cụ ghi/tìm kiếm `MEMORY.md`.

 | Thêm **[Mới]** `crates/mehh-coding-agent/src/memory.rs` (`MemoryStore`, `MemoryTool`) lưu tại `~/.mehh/projects/<cwd_hash>/memory/MEMORY.md` + topic slugs và `~/.mehh/agent/MEMORY.md`; inject `<auto_memory>` idempotent trong `prepare_request` (`agent_session.rs`); thêm `/memory` (`interactive.rs`). |
| **A9 — Turn/session checkpoint & rewind** | `EditUndoStore` (`undo_edit.rs`) chỉ in-memory (20/file, 16 MiB); `SessionManager::branch_from` (`session.rs`) chỉ di chuyển `leaf_id`, không khôi phục file.

 | Thêm `FileMutationObserver` hook trong `mehh-tools`; thêm **[Mới]** `crates/mehh-coding-agent/src/checkpoint.rs` (content-addressed blob `~/.mehh/checkpoints/<session_id>/blobs/<sha256>`); thêm variant `SessionEntryKind::Checkpoint` trong `session.rs`; thêm `AgentSession::rewind_to` (`agent_session.rs`) + `/rewind` (`interactive.rs`). |
| **A10 — Custom keybindings & Vim mode** | `Keybindings::default_bindings()` hardcode; `Settings` không có `keybindings`/`vim_mode`; `ctrl+t`/`alt+enter` hardcode trong `interactive.rs`; `Editor` không có Normal/Insert.

 | Derive `Serialize`/`Deserialize` cho `TuiAction`, thêm `ToggleThinking`/`TogglePlanMode`/`SessionPicker`/`FollowUpSubmit`, `Keybindings::load_file` & `from_overrides` (`keys.rs`); thêm `EditorMode { Insert, Normal }` + `vim_enabled` trong `editor.rs`; thêm `keybindings`/`vim_mode` trong `settings.rs`; nối `/reload`, `/settings`, `/hotkeys` trong `interactive.rs`. |
| **B1–B8 — Hoàn thiện Nhóm B** | `discover_context_files` nối hết root→cwd, thiếu BOM/`@import`/`CLAUDE.local.md`/`.claude/rules/`; config/skills chỉ đọc `cwd`; export chỉ JSONL; chưa có budget limit, auto-title; `is_safe_for_auto_approve` chưa được gọi; `PowerShellTool` bỏ qua sandbox plan; chưa paste ảnh clipboard.

 | B1: nâng cấp `system_prompt.rs`; B2: walk-up trusted trong `settings.rs`/`skills.rs`/`prompt_templates.rs`/`todo.rs`; B3: `ExportFormat` (`jsonl`/`md`/`html`/`txt`) trong `session.rs` + `interactive.rs` + `main.rs`; B4: `BudgetConfig` trong `settings.rs` + `agent_session.rs`; B5: auto-title trong `agent_session.rs`; B6: truyền `is_safe_for_auto_approve` vào `ToolInspectionContext` (`tool_inspection.rs` + `agent_session.rs`); B7: `SandboxPlan` trong `powershell.rs` + workspace temp dir trong `mehh-sandbox/src/lib.rs`; B8: clipboard image attach trong `clipboard.rs` + `interactive.rs`. |

## 1.2 Sơ đồ luồng dữ liệu theo tính năng

### Luồng Tool Governance, Plan Mode, Hooks, Network, Hunk Approval (A1, A2, A3, A5, B6)

```text
LLM ToolCall
  │
  ▼
mehh-agent::execute_single_tool (agent_loop.rs)
  │  1. validate_tool_arguments
  │  2. gọi config.before_tool_call(BeforeToolCallContext)
  ▼
AgentSession::before_tool_call (agent_session.rs)
  │  ├─ [Priority 1] SecurityInspector (tool_inspection.rs)
  │  │      • chặn .mehh/*, auth.json, trust.json, hooks.json
  │  │      • chặn shell nguy hiểm & URL SSRF/file:// (A3)
  │  │      • kiểm tra mọi file trong multi-file apply_patch (A5)
  │  ├─ [Priority 1.5] HookRunner::run_pre_tool_use (hooks.rs, A2)
  │  │      • chỉ chạy hooks đã trust; exit 2 / decision=deny → block
  │  ├─ [Priority 2] PermissionInspector (tool_inspection.rs)
  │  │      • PermissionStore rules → PermissionMode (Auto|SmartApprove|Approve|Chat|Plan)
  │  │      • đánh giá ctx.is_safe_for_auto_approve (B6) + PlanModeState (A1)
  │  ├─ [Priority 3] RepetitionInspector (tool_inspection.rs)
  │  └─ [Approval] ApprovalHandler::request_approval_ext (tool_inspection.rs)
  │         ├─ Tool thường → SelectorKind::ToolApproval (giữ nguyên :3316-3352)
  │         ├─ apply_patch → SelectorKind::HunkApproval → modified_args.accepted_hunks (A5)
  │         └─ exit_plan_mode → SelectorKind::PlanApproval → restore mode (A1)
  ▼
AgentTool::execute(id, modified_args ?? validated_args, cancel, on_update)
  │  ├─ EditTool / WriteTool / ApplyPatchTool / UndoEditTool
  │  │      └─ with_file_mutation_queue → EditUndoStore + FileMutationObserver (A9)
  │  ├─ BashTool(run_in_background=true) / ShellOutputTool / KillShellTool → BackgroundShellManager (A6)
  │  ├─ WebFetchTool / WebSearchTool → NetworkPolicy (DNS+SSRF+cap) → reqwest (A3)
  │  ├─ LspTool → LspManager (JSON-RPC stdio) → language server (A4)
  │  └─ MemoryTool → MemoryStore (~/.mehh/projects/<hash>/memory/) (A8)
  ▼
config.after_tool_call (agent_session.rs)
  │  ├─ LeadWorkerManager::record_tool_outcome
  │  └─ HookRunner::run_post_tool_use (A2) → optional additionalContext
  ▼
config.finish_turn (agent_session.rs)
  │  ├─ BudgetTracker check (B4)
  │  └─ HookRunner::run_stop (A2) → TurnAction::Continue | End (tối đa 3 lần)

```

### Luồng Context Injection & Session Persistence (A1, A7, A8, A9, B1, B4, B5)

```text
SessionManager (JSONL v3: Message, Compaction, BranchSummary, SessionInfo, Checkpoint...)
  │
  ├─ Startup / reload: discover_context_files (B1) + hierarchical skills/prompts/settings (B2)
  ├─ Trước mỗi LLM call: config.prepare_request (agent_session.rs)
  │     ├─ <working_checklist> (todo_state + MOIM.md)
  │     ├─ <auto_memory> (MEMORY.md, A8)
  │     ├─ <plan_mode> (nếu đang ở Plan mode, A1)
  │     └─ <context_budget_warning> (nếu >= warn_pct, B4)
  │        [Tất cả chỉ inject ephemeral vào Context gửi model, KHÔNG ghi JSONL]
  ├─ Sau mỗi turn: AgentSession::finish_prompt
  │     ├─ append Message entries
  │     ├─ append SessionEntryKind::Checkpoint nếu có file mutated (A9)
  │     └─ auto-title SessionInfo lần đầu nếu chưa có name (B5)
  └─ TUI: Ctrl+S / /sessions → SelectorKind::Resume (Current/All scope, A7)
          /rewind → AgentSession::rewind_to (khôi phục blobs/<sha256> + branch_from, A9)

```

### Luồng TUI Input & Vim Mode (A10)

```text
crossterm::Event::Key → normalize_crossterm_key (mehh-tui/src/keys.rs)
  │
  ├─ Keybindings (default_bindings ⊕ ~/.mehh/agent/keybindings.json ⊕ settings.keybindings)
  ├─ Selector overlay đang mở → handle modal (HunkApproval / PlanApproval / Resume / ...)
  ├─ Action toàn cục: SessionPicker (Ctrl+S), ToggleThinking (Ctrl+T), TogglePlanMode (Shift+Tab), Scroll*
  └─ Editor::handle_editor_key (editor.rs)
        ├─ vim_enabled == false → hành vi Emacs hiện tại (giữ nguyên)
        └─ vim_enabled == true  → EditorMode::Insert ↔ EditorMode::Normal
              └─ Editor::render luôn giữ nguyên CURSOR_MARKER = "\x1b_mehh:c\x07"

```

## 1.3 Điểm tích hợp tối thiểu (Minimal diff surface)

* **`crates/mehh-agent`**: chỉ sửa 2 file (`types.rs`, `agent_loop.rs`) để thêm `modified_args: Option>` vào `BeforeToolCallResult` — không đổi chữ ký bất kỳ hook hay trait nào hiện có.


* **`crates/mehh-tui`**: chỉ sửa 2 file (`keys.rs`, `components/editor.rs`); giữ nguyên `TuiRenderer`, `BorderedBox`, `SelectList`, `SettingsList`, và mọi quy tắc `visible_width`/`CURSOR_MARKER`.


* **`crates/mehh-tools`**: thêm các file độc lập (`net_policy.rs`, `web_fetch.rs`, `web_search.rs`, `lsp.rs`, `apply_patch.rs`, `bg_shell.rs`, `core/text.rs`); sửa rất gọn `lib.rs`, `bash.rs`, `powershell.rs`, `edit.rs`, `write.rs`, `undo_edit.rs`.


* **`crates/mehh-coding-agent`**: đặt logic mới vào các module riêng (`plan_mode.rs`, `hooks.rs`, `memory.rs`, `checkpoint.rs`), chỉ gắn lời gọi ngắn vào `agent_session.rs`, `tool_inspection.rs`, `session.rs`, `settings.rs`, `trust.rs`, `system_prompt.rs`, `interactive.rs` — **tuyệt đối không đụng** `interactive.rs:3316-3352`.



---

# Phần 2 — Design spec từng tính năng (A1→A10, rồi B)

## A1 — Plan Mode

* **Problem**: Hiện tại `EnterPlanMode` và `ExitPlanMode` chỉ nằm trong mảng `CLAUDE_CODE_TOOLS` của `crates/mehh-ai/src/anthropic.rs`, còn `PermissionMode` chỉ có `Auto`, `SmartApprove`, `Approve`, `Chat`. Khi người dùng muốn agent phân tích codebase và lập kế hoạch trước khi sửa code, dùng `/mode chat` sẽ khóa cứng mọi tool ghi nhưng không tạo được bản kế hoạch có cấu trúc, không cho agent chủ động đề xuất chuyển từ bước lập kế hoạch sang bước thực thi có phê duyệt, và người dùng phải đổi mode thủ công hai lần. Ví dụ trace: user hỏi `"Thiết kế refactor module auth trước khi sửa"`, agent ở `SmartApprove` tự gọi `edit` ngay turn 2 thay vì hoàn tất bản kế hoạch để duyệt.


* **Solution**:
* **Cấu trúc dữ liệu mới** trong **[Mới]** `crates/mehh-coding-agent/src/plan_mode.rs`:
```rust
#[derive(Debug, Clone, Default)]
pub struct PlanModeState {
    pub active: bool,
    pub previous_mode: Option<PermissionMode>,
    pub plan_path: Option<PathBuf>,
    pub current_plan_markdown: Option<String>,
}

```


* **Mở rộng `PermissionMode**` (`crates/mehh-coding-agent/src/tool_inspection.rs`): thêm variant `Plan` (`"plan"` trong `FromStr` và serde `snake_case`).


* **2 tool mới** (đăng ký trong `AgentSession::new_with_trust` cùng `TodoTool`):
* `EnterPlanModeTool` (`name = "enter_plan_mode"`, `is_read_only() = Some(true)`): chuyển `PlanModeState.active = true`, lưu `previous_mode`, đặt `inspection_manager.set_mode(PermissionMode::Plan).await`, chuẩn bị `plan_path = ~/.mehh/plans/<session_id>.md`.
* `ExitPlanModeTool` (`name = "exit_plan_mode"`, `is_read_only() = Some(false)`, `execution_mode = Sequential`): nhận `plan: String` (bắt buộc) và `summary: Option<String>`. Ghi plan vào `plan_path` qua `write_file_atomic(..., true)` **sau khi được phê duyệt**; ở `PermissionInspector`, `exit_plan_mode` luôn trả `InspectionVerdict::RequireApproval { reason: "Approve plan and exit Plan mode", preview: Some(plan_preview) }`.


* **JSON Schema của `exit_plan_mode**`:
```json
{
  "type": "object",
  "properties": {
    "plan": { "type": "string", "description": "Complete Markdown implementation plan for user approval" },
    "summary": { "type": "string", "description": "Short 1-2 line summary of the plan" }
  },
  "required": ["plan"]
}

```


* **State transitions**:
* `NormalMode (Auto|SmartApprove|Approve)` --(`enter_plan_mode` / `/plan on` / `Shift+Tab`)--> `Plan` (lưu `previous_mode`).
* Trong `Plan`: `PermissionInspector` chỉ `Allow` tool có `is_safe_for_auto_approve == Some(true)` (read-only: `read`, `grep`, `find`, `ls`, `analyze`, `lsp` read actions, `todo`, `tool_search`, `memory` read, `subagent` chỉ khi `mode` là `research`/`review`) và `enter_plan_mode`; trả `RequireApproval` cho `exit_plan_mode`; trả `Deny` cho mọi tool ghi/chạy lệnh (`write`, `edit`, `apply_patch`, `undo_edit`, `bash`, `powershell`, `kill_shell`, `subagent(coding)`, MCP mutating).
* `Plan` --(`exit_plan_mode` được duyệt trong `SelectorKind::PlanApproval`)--> ghi file plan, khôi phục `previous_mode` (hoặc `Auto` nếu user chọn *"Approve & Auto-Execute"*), lưu `SessionEntryKind::Custom { custom_type: "plan_approved", data }`.
* `Plan` --(`exit_plan_mode` bị từ chối)--> giữ nguyên `PermissionMode::Plan`, trả tool error `"User rejected the plan; refine it based on feedback"`.


* **Prompt injection & TUI**:
* `prepare_request` (`agent_session.rs`) chèn block `<plan_mode>...</plan_mode>` (ephemeral, idempotent, không ghi JSONL) khi `plan_mode.active == true`.
* `permission_badge` (`interactive.rs`) thêm `PermissionMode::Plan => ("[plan]", &theme.accent)`.
* Đăng ký `TuiAction::TogglePlanMode` mặc định `backtab` (`Shift+Tab`, không trùng phím nào trong `keys.rs`).


* Cập nhật `to_claude_code_name` / `from_claude_code_name` (`crates/mehh-ai/src/anthropic.rs`) map hai chiều `enter_plan_mode` ↔ `EnterPlanMode`, `exit_plan_mode` ↔ `ExitPlanMode`.



* **File cần sửa**: `crates/mehh-coding-agent/src/plan_mode.rs` **[Mới]**, `tool_inspection.rs`, `agent_session.rs`, `interactive.rs`, `lib.rs`, `crates/mehh-tui/src/keys.rs`, `crates/mehh-ai/src/anthropic.rs`.


* **Why**: Cần một state machine rõ ràng giữa chế độ nghiên cứu chỉ-đọc và chế độ thực thi có phê duyệt, kèm file plan làm hợp đồng đầu ra. Không dùng `Chat` mode vì `Chat` không lưu `previous_mode`, không cho model xin thoát bằng `exit_plan_mode`, và không phân biệt với trò chuyện thuần túy. Không cho model tự `write` file `.mehh/plan.md` trong Plan mode vì sẽ phải đục lỗ `SecurityInspector` (đang khóa `.mehh/*`) — để `ExitPlanModeTool` tự ghi nguyên tử vào `~/.mehh/plans/` đơn giản và an toàn hơn hẳn.


* **Security/compat**:
* Fail-closed: ở headless (`-p`/`--mode json`) không có interactive approval, `exit_plan_mode` bị `HeadlessDenyHandler` chặn trừ khi chạy `--permission-mode auto` (hoặc giữ nguyên ở Plan mode và in bản plan ra stdout).
* `PermissionMode::Plan` backward-compatible với `Settings` (`#[serde(default)]`).
* Không đục lỗ `SecurityInspector`; `subagent(coding)` bị `PermissionInspector` chặn khi `mode == Plan`.
* Cross-platform: `plan_path` dùng `PathBuf::join` với `session_id` (UUID v7, không chứa `:` hay ký tự cấm trên Windows).



## A2 — Hooks do người dùng cấu hình (`hooks.json`)

* **Problem**: `AgentLoopConfig` chỉ có các hook bằng closure Rust biên dịch cứng, chưa có cơ chế để người dùng gắn script tự động hóa (như chạy linter sau `edit`, chặn lệnh tùy biến trước `bash`, thông báo khi cần approval, hoặc buộc agent kiểm tra test trước khi dừng) qua file cấu hình. Ví dụ trace: repo có quy tắc cấm sửa file sinh tự động `src/generated/*`, nhưng hiện không thể cấu hình hook `PreToolUse` để chặn hoặc `PostToolUse` để tự chạy `cargo fmt --check`.


* **Solution**:
* **Module mới**: **[Mới]** `crates/mehh-coding-agent/src/hooks.rs`.
* **Schema `hooks.json**` (đặt tại `~/.mehh/agent/hooks.json` và `<cwd>/.mehh/hooks.json`):
```json
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^(edit|write|apply_patch)$",
        "command": "node scripts/check-generated.mjs",
        "timeoutMs": 5000,
        "failClosed": true
      }
    ],
    "PostToolUse": [
      {
        "matcher": "^(edit|write)$",
        "command": "cargo fmt --check",
        "timeoutMs": 15000,
        "failClosed": false
      }
    ],
    "SessionStart": [
      { "command": "git status --short", "timeoutMs": 3000 }
    ],
    "Stop": [
      { "command": "cargo check --quiet", "timeoutMs": 30000, "failClosed": false }
    ],
    "Notification": [
      { "command": "powershell -NoProfile -Command \"[console]::beep()\"", "timeoutMs": 2000 }
    ]
  }
}

```


* **Cấu trúc Rust**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    Stop,
    Notification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEntry {
    #[serde(default)]
    pub matcher: Option<String>, // regex khớp tool_name (hoặc "*" nếu None)
    pub command: String,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64, // mặc định 10_000, clamp 500..=60_000
    #[serde(default)]
    pub fail_closed: Option<bool>, // mặc định true cho PreToolUse, false cho Notification
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HooksConfigFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub hooks: HashMap<HookEvent, Vec<HookEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow { additional_context: Option<String> },
    Ask { reason: String },
    Deny { reason: String },
}

```


* **Giao thức thực thi subprocess (`HookRunner`)**:
* Nhận payload JSON qua **stdin** của hook child process (`Stdio::piped()`, ghi JSON rồi đóng stdin ngay — không bao giờ chia sẻ TTY stdin với TUI/RPC).
* Chạy qua `build_command_spec` + `mehh_sandbox::wrap` (`SandboxPolicy::WorkspaceWrite`) + scrub toàn bộ biến môi trường bí mật theo `mehh_sandbox::is_secret_env_key` + `kill_process_tree` khi quá `timeout_ms` hoặc `cancel.cancelled()`.


* Quy ước kết quả:
* Exit `0`: nếu stdout rỗng → `Allow { additional_context: None }`; nếu stdout là JSON hợp lệ `{ "decision": "allow"|"deny"|"ask", "reason": "...", "additionalContext": "..." }` → áp dụng decision tương ứng.
* Exit `2`: `Deny { reason: stderr_or_stdout }` (tương thích chuẩn Claude Code).
* Exit khác `0`/`2`, timeout, spawn lỗi, hoặc stdout JSON hỏng: nếu `PreToolUse` với `fail_closed.unwrap_or(true)` → `Deny` (fail-closed); nếu `PostToolUse`/`SessionStart`/`Notification` → ghi cảnh báo rõ ràng (không nuốt lỗi im lặng).



* **Tích hợp & thứ tự chạy**:
* `trust.rs`: bổ sung `mehh_dir.join("hooks.json").exists()` vào `has_local_trust_targets`. Khi `trusted == false`, tuyệt đối không đọc `<cwd>/.mehh/hooks.json`.


* `agent_session.rs`:
* `before_tool_call`: chạy **SecurityInspector trước** → `HookRunner::run_pre_tool_use` → `PermissionInspector` + `RepetitionInspector` + `ApprovalHandler`. Nếu hook trả `Ask`, ép `RequireApproval`. Hook **không bao giờ** có thể nâng từ `SecurityInspector::Deny` lên `Allow`.
* `after_tool_call`: chạy `LeadWorkerManager::record_tool_outcome` rồi `HookRunner::run_post_tool_use`; nếu có `additional_context`, nối thêm block text vào `AfterToolCallResult.content`.
* `finish_turn` (đang là `None` trên `AgentLoopConfig`): chạy `HookRunner::run_stop`. Nếu hook trả `Deny`, nối `reason` vào `steering_queue` và trả `FinishTurnDecision { action: TurnAction::Continue }` (chặn cứng tối đa `MAX_STOP_HOOK_LOOPS = 3` mỗi prompt để chống lặp vô hạn).


* `SessionStart`: gọi tại khởi tạo session / `/new` / `/resume`; nếu trả `additional_context`, inject vào `system_prompt` hoặc `CustomMessage`.
* `Notification`: gọi bất đồng bộ không chặn (bounded) khi phát sinh `ToolApprovalRequest` hoặc kết thúc turn.



* **File cần sửa**: `crates/mehh-coding-agent/src/hooks.rs` **[Mới]**, `trust.rs`, `agent_session.rs`, `tool_inspection.rs`, `interactive.rs`, `lib.rs`.


* **Why**: Subprocess JSON-over-stdin/stdout giúp người dùng viết hook bằng bất kỳ ngôn ngữ nào (sh, PowerShell, Python, Node, Rust binary) mà không cần nhúng plugin runtime hay WASM host nặng nề. Không gộp vào `settings.json` để giữ ranh giới cấu hình rõ ràng và tái dùng cơ chế bảo vệ file `.mehh/` của `SecurityInspector` + `trust.rs`.
* **Security/compat**:
* Fail-closed: `ENOENT` trả config rỗng; JSON hỏng trả `Err` ngay khi load (không im lặng fallback); strip UTF-8 BOM.


* Project `.mehh/hooks.json` bị chặn hoàn toàn khi workspace chưa trust; `SecurityInspector` chặn agent tự sửa `.mehh/hooks.json` hoặc `~/.mehh/agent/hooks.json` (chống prompt injection tự chèn backdoor hook).
* Mọi env bí mật bị scrub bằng `mehh_sandbox::is_secret_env_key`.


* Cross-platform: dùng `ShellKind::WindowsPowerShellCompat` trên Windows và `ShellKind::PosixSh` trên Unix, cùng `kill_process_tree` khi timeout.





## A3 — Web Search & WebFetch Tool

* **Problem**: `WebFetch` và `WebSearch` hiện chỉ là tên trong `CLAUDE_CODE_TOOLS`. Agent muốn tra tài liệu crate/API phải nhờ user copy hoặc gọi `bash` chạy `curl`, dễ đụng rule chặn `curl | sh`, dễ leak token/env hoặc đổ hàng trăm KB HTML vào context.


* **Solution**:
* **3 module mới** trong `crates/mehh-tools/src/` (dùng sẵn `reqwest` + `regex` + `tokio` đã có trong workspace, **0 dependency mới**):
1. `crates/mehh-tools/src/net_policy.rs` **[Mới]**:
```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    #[serde(default)]
    pub allow_domains: Vec<String>,   // vd: ["docs.rs", "*.rust-lang.org"]
    #[serde(default)]
    pub deny_domains: Vec<String>,    // luôn thắng allow_domains
    #[serde(default)]
    pub allow_http: bool,             // mặc định false (chỉ HTTPS)
    #[serde(default = "default_max_bytes")]
    pub max_response_bytes: usize,    // mặc định 2 MiB, trần 5 MiB
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,              // mặc định 20_000, trần 60_000
    #[serde(default)]
    pub search_provider: Option<SearchProviderKind>, // Tavily | Brave | Searxng | Disabled
    #[serde(default)]
    pub search_endpoint: Option<String>, // bắt buộc với Searxng
}

```


* Hàm kiểm soát **fail-closed**:
* `validate_url(raw: &str, policy: &NetworkPolicy) -> Result<ValidatedUrl, String>`: chỉ chấp nhận `https` (hoặc `http` nếu `allow_http`), cấm `userinfo` (`user:pass@host`), cấm IP literal private/loopback/link-local/metadata, kiểm tra `deny_domains` trước rồi `allow_domains`.
* `resolve_and_check_ssrf(host: &str, port: u16) -> Result<Vec<SocketAddr>, String>`: gọi `tokio::net::lookup_host`, từ chối **nếu bất kỳ IP nào** thuộc `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (AWS/GCP metadata), `100.64.0.0/10` (CGNAT), `0.0.0.0/8`, `::1`, `fc00::/7`, `fe80::/10`, IPv4-mapped IPv6 (`::ffff:127.0.0.1`).
* **Redirect hop guard**: cấu hình `reqwest::redirect::Policy::none()`, tự bám theo tối đa `MAX_REDIRECTS = 5` hop và gọi lại cả `validate_url` + `resolve_and_check_ssrf` ở **mỗi hop** (chống redirect bypass sang `[http://169.254.169.254](http://169.254.169.254)`).
* **Pinning chống DNS rebinding**: tạo request kết nối tới `SocketAddr` đã kiểm duyệt qua `reqwest::ClientBuilder::resolve_to_addrs`.
* **Redaction & audit log**: `redact_secrets(text: &str) -> String` che pattern token (`sk-ant-`, `sk-`, `ghp_`, `AIza`, `AKIA`, query key `api_key=`, `token=`, `key=`); log kiểm soát qua `tracing::info!` chỉ ghi `{ tool, host, status, bytes, duration_ms }` đã redact.



2. `crates/mehh-tools/src/web_fetch.rs` **[Mới]** (`WebFetchTool`, `name = "web_fetch"`):
* Schema: `{ "url": string (required), "max_chars": integer (optional, default 20000, max 30000), "raw": boolean (optional, default false) }`.
* `is_read_only() -> Some(true)`; `is_safe_for_auto_approve(args)`: trả `Some(true)` **chỉ khi** domain khớp `policy.allow_domains` (và không khớp `deny_domains`), ngược lại trả `Some(false)` để buộc `SmartApprove` hỏi ý kiến người dùng!
* Stream body theo chunk với giới hạn cứng `max_response_bytes`; kiểm tra `Content-Type` (chặn binary/archive); tách text từ HTML thuần Rust (bỏ `<script>`/`<style>`/`<noscript>`, giữ heading/link/code); bọc kết quả trong thẻ `<untrusted_web_content url="...">...</untrusted_web_content>` để phòng indirect prompt injection.


3. `crates/mehh-tools/src/web_search.rs` **[Mới]** (`WebSearchTool`, `name = "web_search"`):
* Schema: `{ "query": string (required), "max_results": integer (optional, default 5, max 10), "allowed_domains": array<string> (optional) }`.
* Trait `SearchBackend`: hỗ trợ `Tavily`, `Brave`, `Searxng` (endpoint tự host có allowlist rõ ràng); mặc định khi chưa cấu hình provider/API key trả lỗi rõ ràng thay vì cào web không phép (fail-closed).
* API key của search provider đọc riêng từ biến môi trường chuyên biệt (`TAVILY_API_KEY` / `BRAVE_SEARCH_API_KEY`) thông qua `auth.rs` của `mehh-coding-agent` lúc khởi tạo tool — bản thân `WebFetchTool` **tuyệt đối không** truy cập `auth.json`, `.env`, hay `~/.ssh`.



* **Tích hợp Inspection Pipeline (`tool_inspection.rs`)**:
* `SecurityInspector` (Priority 1) kiểm tra `web_fetch` / `web_search`: chặn ngay `file://`, `ftp://`, `data:`, `javascript:`, URL có `user:pass@`, và literal localhost/private IP trong mọi mode kể cả `Auto`.
* `PermissionInspector` dùng `ctx.is_safe_for_auto_approve` (B6) nên domain ngoài `allow_domains` luôn phải approval ở `SmartApprove`, bị chặn ở headless nếu không allowlist, và trong `Chat`/`Plan` mode chỉ cho phép nếu nằm trong `allow_domains` hoặc được duyệt.
* `SettingsManager::try_load_from_paths` (`settings.rs`): **escalation guard** — project `.mehh/settings.json` không thể đặt `allow_domains: ["*"]` hoặc `allow_http: true` để vượt cấu hình global.


* **File cần sửa**: `crates/mehh-tools/src/{net_policy.rs, web_fetch.rs, web_search.rs}` **[Mới]**, `crates/mehh-tools/src/lib.rs`, `crates/mehh-coding-agent/src/{settings.rs, tool_inspection.rs, agent_session.rs}`, `crates/mehh-ai/src/anthropic.rs` (mapping `web_fetch`↔`WebFetch`, `web_search`↔`WebSearch`).


* **Why**: Dùng `reqwest` có sẵn tránh tăng kích thước binary. Tách `WebFetchTool` khỏi `bash` giúp loại bỏ hoàn toàn bề mặt tấn công shell injection và giới hạn chặt domain/SSRF/size cap. Không làm HTML headless browser (Playwright/Chrome) vì kéo dependency nặng và không cần cho 95% trang docs/API.
* **Security/compat**:
* Fail-closed toàn phần: không có allowlist → hỏi approval (interactive) hoặc từ chối (headless); SSRF check chạy trên cả IPv4/IPv6 và mọi redirect hop.
* Không bao giờ đọc `.env`, `auth.json`, `~/.ssh`; không gửi header Authorization/Cookie của LLM provider sang domain ngoài.
* `Settings.network: Option<NetworkPolicy>` gắn `#[serde(default, skip_serializing_if = "Option::is_none")]`.



## A4 — LSP / Code Intelligence thật

* **Problem**: `AnalyzeTool` (`crates/mehh-tools/src/analyze.rs`) chỉ dùng regex 1 file cho Rust/TS/Python/Go. Khi refactor một trait hay struct xuyên 10 crate (như đổi chữ ký `ApprovalHandler`), regex không phân biệt được symbol cùng tên khác scope, không tìm được mọi reference theo kiểu dữ liệu, và không đọc được lỗi compiler (diagnostics) tức thì.


* **Trade-off Dependency**:
* **`tree-sitter` (C bindings + grammar crates)**: kéo compiler C (`cc`/MSVC) dễ gãy build trên Windows, tăng binary +8–15 MB, và **vẫn không giải quyết được** go-to-definition, find-references, type-aware rename, hay compiler diagnostics (chỉ có syntax tree). → **Loại.**
* **`tower-lsp` / `lsp-types**`: `tower-lsp` thiết kế cho LSP server, kéo `tower`/`dashmap`; `lsp-types` tương đối lớn và chủ yếu là struct JSON. → **Không bắt buộc.**
* **Client LSP stdio JSON-RPC 2.0 thuần Rust (Chọn)**: Tái sử dụng đúng mẫu `tokio::process::Command` + `serde_json` + `CancellationToken` đang có ở `mehh-mcp`, chỉ khác framing `Content-Length: N\r\n\r\n` (~350 dòng). **0 dependency mới**, **0 rủi ro build C trên Windows**, tăng binary ~40 KB, và đạt code intelligence thật từ `rust-analyzer`, `typescript-language-server`, `pyright-langserver`, `gopls`.



* **Solution**:
* **Module mới**: **[Mới]** `crates/mehh-tools/src/lsp.rs` gồm:
* `LspServerConfig` (cấu hình trong `Settings::lsp: Option<LspSettings>` với `#[serde(default)]`):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerConfig {
    pub language_id: String,        // "rust", "typescript", "python", "go"
    pub extensions: Vec<String>,    // ["rs"], ["ts","tsx","js","jsx"], ...
    pub command: String,            // "rust-analyzer"
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_lsp_idle_secs")]
    pub idle_timeout_secs: u64,     // mặc định 300s
}

```


* `LspClient`: JSON-RPC 2.0 `Content-Length` framing over stdin/stdout, quản lý `initialize`/`initialized`, `textDocument/didOpen`/`didChange` (sync phiên bản từ đĩa khi file đổi mtime/hash), cache `textDocument/publishDiagnostics`, timeout mỗi request (`15s`), hủy qua `$/cancelRequest`, và `shutdown`/`exit` + `kill_process_tree` khi drop.
* `LspManager` (`Arc<LspManager>`): lazy-spawn tối đa 1 server mỗi ngôn ngữ theo `cwd`, tự tắt sau `idle_timeout_secs`, scrub toàn bộ env bí mật (`mehh_sandbox::is_secret_env_key`).


* `LspTool` (`name = "lsp"`):
* Schema:
```json
{
  "type": "object",
  "properties": {
    "action": { "type": "string", "enum": ["definition", "references", "hover", "diagnostics", "workspace_symbols", "rename"] },
    "path": { "type": "string", "description": "File path (required except workspace_symbols)" },
    "line": { "type": "integer", "description": "1-indexed line number" },
    "column": { "type": "integer", "description": "1-indexed UTF-16/char column" },
    "query": { "type": "string", "description": "Query string for workspace_symbols" },
    "new_name": { "type": "string", "description": "New symbol name for rename" }
  },
  "required": ["action"]
}

```


* `is_read_only()` trả `None`; `is_safe_for_auto_approve(args)` trả `Some(true)` cho `definition`/`references`/`hover`/`diagnostics`/`workspace_symbols`, và `Some(false)` cho `rename` (kết nối B6!).
* Với `rename`: gọi `textDocument/rename`, chuyển `WorkspaceEdit` (`changes` / `documentChanges`) thành danh sách thay thế đã kiểm tra `PathPolicy` + `SecurityInspector` (cấm rename xuyên ra ngoài `cwd` hoặc vào `.mehh/`), áp qua `with_file_mutation_queue` + `EditUndoStore` + `FileMutationObserver` (tương thích A9!), trả unified diff tổng hợp!



* **Fallback minh bạch**: Nếu server chưa cài trong `PATH`, trả `Err("LSP server 'rust-analyzer' not found in PATH. Install it or use 'analyze'/'grep' instead.")` — không bao giờ âm thầm trả kết quả regex giả mạo là LSP.
* **File cần sửa**: `crates/mehh-tools/src/lsp.rs` **[Mới]**, `crates/mehh-tools/src/lib.rs`, `crates/mehh-coding-agent/src/{settings.rs, agent_session.rs, tool_inspection.rs}`.


* **Why**: LSP là chuẩn duy nhất cho code intelligence chính xác theo type system của từng ngôn ngữ mà không phải tự viết compiler frontend. Viết framing `Content-Length` trực tiếp giữ nguyên triết lý single-binary nhẹ, build nhanh trên Windows. Giữ `AnalyzeTool` song song để máy không cài LSP vẫn có outline nhanh.
* **Security/compat**:
* Fail-closed: cấu hình `Settings::lsp` trong `<cwd>/.mehh/settings.json` **chỉ được load khi `trusted == true**` (chống repo untrusted đặt `command: "malicious.exe"` chạy khi agent mở file!).
* Chuẩn hóa URI `file:///` trên Windows (`C:\foo` ↔ `file:///c:/foo`, xử lý percent-encoding và case-insensitive) — tránh lỗi kinh điển của `rust-analyzer`/`gopls` trên Windows.
* Đóng toàn bộ LSP child process khi thoát session qua `LspManager::shutdown_all().await`.



## A5 — `apply_patch` + Accept/Reject từng Hunk

* **Problem**: Hiện `EditTool` (`crates/mehh-tools/src/edit.rs`) áp tất cả thay đổi trong một lần, `ApprovalHandler::request_approval` chỉ trả `bool`, và `interactive.rs` chỉ vẽ diff read-only trong `TranscriptItem::ToolCall`. Nếu agent đề xuất sửa 4 đoạn trong 1 file mà 1 đoạn bị sai logic, người dùng chỉ có thể từ chối cả tool call rồi nhắc model làm lại.


* **Solution**:
* **Tách helper chung** sang **[Mới]** `crates/mehh-tools/src/core/text.rs`: chuyển `split_bom`, `normalize_for_fuzzy_match`, và logic bảo toàn CRLF/LF cục bộ từ `edit.rs` ra module chung để cả `EditTool` và `ApplyPatchTool` cùng dùng.


* **Tool mới `ApplyPatchTool**` trong **[Mới]** `crates/mehh-tools/src/apply_patch.rs`:
* `name = "apply_patch"`, `execution_mode = Sequential`, `is_read_only() = Some(false)`.
* Schema:
```json
{
  "type": "object",
  "properties": {
    "patch": { "type": "string", "description": "Unified diff patch (single or multi-file)" },
    "accepted_hunks": {
      "type": "array",
      "items": { "type": "integer" },
      "description": "Internal/optional 0-based hunk indices approved for application"
    }
  },
  "required": ["patch"]
}

```


* Cấu trúc parser:
```rust
pub struct ParsedPatch { pub files: Vec<FilePatch> }
pub struct FilePatch {
    pub path: String,
    pub kind: FilePatchKind, // Update | Create | Delete
    pub hunks: Vec<PatchHunk>,
}
pub struct PatchHunk {
    pub global_index: usize,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<HunkLine>, // Context(String) | Remove(String) | Add(String)
}

```


* `plan_patch(&self, patch_str: &str) -> Result<ParsedPatch, String>`: parse và xác thực toàn bộ file/hunk trước khi ghi đĩa; trả danh sách đường dẫn target để `SecurityInspector` kiểm tra **mọi file** trong patch (không cho patch multi-file lén sửa `.mehh/` hoặc nhảy ra ngoài `cwd`!).
* Khi áp dụng: lọc các hunk theo `accepted_hunks` (nếu `None` → áp toàn bộ; nếu `Some(empty)` → không sửa đĩa, trả lỗi `"All hunks rejected by user"`). Với mỗi file có ít nhất 1 hunk được chọn: khóa bằng `with_file_mutation_queue`, lưu snapshot vào `EditUndoStore` + `FileMutationObserver`, ghi qua `atomic_write`, trả unified diff thực tế + danh sách hunk đã chấp nhận / bị từ chối.


* **Nâng cấp Hook & Approval (backward-compatible)**:
* `crates/mehh-agent/src/types.rs`: thêm `pub modified_args: Option<serde_json::Map<String, serde_json::Value>>` (mặc định `None` qua `Default`) vào `BeforeToolCallResult`.


* `crates/mehh-agent/src/agent_loop.rs`: trong `execute_single_tool`, nếu `before_res.modified_args` là `Some(m)` thì dùng `m` thay cho `validated_args` khi gọi `tool.execute(...)`.


* `crates/mehh-coding-agent/src/tool_inspection.rs`: thêm `pub enum ApprovalOutcome { Allow, AllowPartial { accepted_hunks: Vec<usize> }, Deny }` và default method `ApprovalHandler::request_approval_ext` gọi lại `request_approval` để không vỡ mọi implementer cũ (`HeadlessDenyHandler`, tests).



* **TUI Per-Hunk Picker (`crates/mehh-coding-agent/src/interactive.rs`)**:
* Thêm variant riêng `SelectorKind::HunkApproval` + `PendingHunkApproval { tool_name, hunks: Vec<HunkPreviewItem>, accepted: Vec<bool>, cursor: usize, reply_tx: oneshot::Sender<ApprovalOutcome> }`.
* **Bảo toàn bất biến**: nhánh `SelectorKind::HunkApproval` xử lý tách biệt hoàn toàn với `SelectorKind::ToolApproval`, **không đụng một dòng nào** vào permission-rule block tại `crates/mehh-coding-agent/src/interactive.rs:3316-3352`.
* Phím trong `HunkApproval`: `up`/`down` di chuyển giữa các hunk, `space` bật/tắt accept hunk hiện tại, `y` accept & nhảy xuống hunk kế, `n` reject & nhảy xuống hunk kế, `A` accept all, `enter` xác nhận bộ hunk đã chọn, `escape` hủy toàn bộ (`Deny`). Render dùng `Theme` (`tool_success`/`diff_add_bg`, `tool_error`/`diff_del_bg`, `accent`), `BorderedBox`, và `visible_width`/`truncate_to_width`.


* **File cần sửa**: `crates/mehh-tools/src/core/text.rs` **[Mới]**, `crates/mehh-tools/src/apply_patch.rs` **[Mới]**, `crates/mehh-tools/src/{edit.rs, lib.rs}`, `crates/mehh-agent/src/{types.rs, agent_loop.rs}`, `crates/mehh-coding-agent/src/{tool_inspection.rs, agent_session.rs, interactive.rs}`.


* **Why**: Tách `ApplyPatchTool` và `SelectorKind::HunkApproval` giúp giữ `EditTool` đơn giản cho sửa nhỏ, đồng thời hỗ trợ patch nhiều hunk / nhiều file với phê duyệt từng mảnh chính xác. Dùng `BeforeToolCallResult.modified_args` giữ `AgentTool::execute` thuần túy, không phải nhồi UI state vào `mehh-tools`.
* **Security/compat**:
* Fail-closed: `SecurityInspector` parse danh sách mọi target path trong `patch` — nếu bất kỳ path nào vi phạm `.mehh/*`/`auth.json`/`trust.json` → `Deny` toàn bộ patch; nếu path ngoài `cwd` → bắt buộc `RequireApproval`.
* Headless mode (`HeadlessDenyHandler`): trả `Deny` trừ khi `PermissionMode::Auto`.
* Không thay đổi JSONL schema; `modified_args` không serialize ra wire LLM.



## A6 — Background Shell / Quản lý Process

* **Problem**: `run_shell` (`crates/mehh-tools/src/core/shell.rs`) luôn chạy đồng bộ tới khi process thoát hoặc hết `timeout_ms`, sau đó `kill_process_tree` cả process group sau `PIPE_GRACE = 500ms`. Agent không thể khởi động dev server (như `npm run dev` hay mock HTTP server), kiểm tra log theo tiến độ, chạy test song song rồi tắt server sạch sẽ. `KillShell` trong `anthropic.rs` hiện chưa có tool tương ứng.


* **Solution**:
* **Module mới**: **[Mới]** `crates/mehh-tools/src/bg_shell.rs`.
* **State & quản lý tập trung (`BackgroundShellManager`)**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BgProcessStatus {
    Running,
    Exited(i32),
    Killed,
}

pub struct BgProcessSummary {
    pub shell_id: String,
    pub command: String,
    pub pid: Option<u32>,
    pub status: BgProcessStatus,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub buffered_bytes: usize,
}

pub struct BackgroundShellManager { /* Mutex<HashMap<String, BgProcessRecord>> */ }

```


* Giới hạn tối đa `MAX_BG_SHELLS = 8` tiến trình chạy đồng thời (vượt ngưỡng → trả `Err`, fail-closed).
* Tái sử dụng trực tiếp `build_command_spec`, `mehh_sandbox::wrap`, scrub env bằng `is_secret_env_key`, `Stdio::null()`, `process_group(0)` trên Unix, `RollingBytes` (256 KiB), và `kill_process_tree` từ `crates/mehh-tools/src/core/shell.rs`.


* Mỗi background shell có 1 tokio supervisor task đọc stdout/stderr vào `RollingBytes`, chờ `child.wait()` hoặc `entry_cancel.cancelled()`, cập nhật `BgProcessStatus` + `ended_at_ms`, và dọn cây tiến trình bằng `kill_process_tree` khi bị hủy.


* **3 điểm tiếp xúc Tool**:
1. **`BashTool` (`crates/mehh-tools/src/bash.rs`)**: thêm tham số tùy chọn `run_in_background: Option<bool>` (mặc định `false`). Khi `true`, ủy quyền cho `BackgroundShellManager::spawn(...)` và trả về JSON/text gồm `shell_id`, `pid`, `status: "running"`.
2. **`ShellOutputTool` (`name = "shell_output"`, `is_read_only() = Some(true)`)**:
* Schema: `{ "shell_id": string (optional — nếu bỏ trống sẽ liệt kê mọi background shell), "wait_ms": integer (optional, default 0, max 30000), "from_start": boolean (optional, default false) }`.
* Trả output mới kể từ lần đọc trước (incremental cursor) + trạng thái hiện tại.


3. **`KillShellTool` (`name = "kill_shell"`, `is_read_only() = Some(false)`, `execution_mode = Sequential`)**:
* Schema: `{ "shell_id": string (required) }`.
* Gọi `BackgroundShellManager::kill(shell_id).await` → `kill_process_tree`, trả trạng thái và 20 dòng log cuối.



* **Lifecycle & TUI**:
* `AgentSession` giữ `pub bg_shells: Arc<BackgroundShellManager>` và cung cấp `pub async fn shutdown_background_shells(&self)`. Gọi khi kết thúc `run_interactive_mode`, `run_rpc_mode`, và `run_print_flow`.
* Interactive TUI: hiển thị pill nhỏ `bg: N` (màu `theme.warning`) ở footer khi `running_count > 0`; thêm slash command `/shells` (liệt kê) và `/shells kill <id|all>`.
* `anthropic.rs`: map `kill_shell` ↔ `KillShell` trong OAuth mode.



* **File cần sửa**: `crates/mehh-tools/src/bg_shell.rs` **[Mới]**, `crates/mehh-tools/src/{core/shell.rs, bash.rs, lib.rs}`, `crates/mehh-coding-agent/src/{agent_session.rs, tool_inspection.rs, interactive.rs}`, `crates/mehh-cli/src/main.rs`, `crates/mehh-ai/src/anthropic.rs`.


* **Why**: Gom start vào `bash(run_in_background=true)`, list+read vào `shell_output`, và kill vào `kill_shell` giữ bộ tool gọn (chỉ thêm 2 tool definition trong prompt), giảm token schema mà đủ 100% chức năng start/poll/list/kill. Không dùng daemon ngoài vì vi phạm kiến trúc single-binary in-process.
* **Security/compat**:
* Fail-closed: `bash(run_in_background: true)` đi qua `SecurityInspector` + `PermissionInspector` + `SandboxPlan`; trong `SmartApprove`, **không bao giờ** auto-approve `run_in_background: true` ngay cả với lệnh thuộc whitelist read-only (vì tiến trình nền giữ tài nguyên hệ thống).
* Trong `Chat` và `Plan` mode: `bash` và `kill_shell` bị `Deny`.
* Cross-platform: Windows dùng `taskkill /PID <pid> /T /F`, Unix dùng `kill -KILL -<pgid>` đã có sẵn trong `kill_process_tree`.



## A7 — Session Switcher tương tác

* **Problem**: Spec `2026-11-17-interactive-session-switcher-design.md` và plan `2026-11-17-interactive-session-switcher-plan.md` đã có nhưng chưa được triển khai trong source: `TuiAction` thiếu `SessionPicker` (`Ctrl+S`), `interactive.rs` chưa có `/sessions`, `/session` chỉ in thống kê văn bản, và `session.rs` chỉ liệt kê session của đúng `cwd` hiện tại. Ngoài ra, rà soát code hiện tại cho thấy 2 lỗ hổng cần vá ngay khi triển khai spec 17/11:


1. Trong `interactive.rs`, `update_selector_filter` ăn mọi phím chữ đơn (`a-z`) trước khi dispatch phím modal → dùng phím `d` để xóa session như spec cũ sẽ xung đột trực tiếp với gõ filter (ví dụ gõ `"debug"`).


2. Spec/plan 17/11 chỉ kiểm tra path chứa chuỗi `.mehh` và `sessions` khi xóa file → dễ bị path traversal hoặc symlink đánh lừa.



* **Solution**:
* **`crates/mehh-tui/src/keys.rs`**: thêm `TuiAction::SessionPicker` gắn mặc định `"ctrl+s"` (đăng ký vào `Keybindings::default_bindings()` và `format_hotkeys`).


* **`crates/mehh-coding-agent/src/session.rs`**:
* Tách `pub fn parse_session_summary(path: &Path) -> Option<SessionSummary>` dùng chung giữa `list_sessions` và `list_all_sessions`.
* Thêm `pub fn sessions_root_dir() -> Result<PathBuf>` (`~/.mehh/sessions`) và `pub fn list_all_sessions() -> Result<Vec<SessionSummary>>` quét mọi thư mục con `<cwd_hash>`, sắp xếp `timestamp` giảm dần.
* Thêm `pub fn delete_session_file(path: &Path) -> Result<()>`:
* Kiểm tra `symlink_metadata(path)` → từ chối nếu là symlink.
* Kiểm tra extension == `"jsonl"`.
* Canonicalize cả `sessions_root_dir()?` và `path.parent()` (từ chối nếu `!canonical_target.starts_with(&canonical_root)`).
* Thực hiện `std::fs::remove_file(&canonical_target)`.



* **`crates/mehh-coding-agent/src/interactive.rs`**:
* Thêm `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SessionPickerScope { CurrentWorkspace, AllWorkspaces }`.
* Mở rộng `Selector` thêm `pub session_scope: Option<SessionPickerScope>` và `pub pending_delete_id: Option<String>`.
* Routing slash command:
* `/sessions` và `/resume`: mở `SelectorKind::Resume` (mặc định `CurrentWorkspace`).
* `/session` không tham số: mở `SelectorKind::Resume`; `/session info`: giữ nguyên bảng thống kê hiện tại; `/session new`: alias `/new`.


* `resume_selector_items(sessions, scope)`: luôn đặt item 0 là `SelectItem::new("__new__", "[+ Start New Session]")`; ở `AllWorkspaces`, gắn thêm tên thư mục ngắn `compact_path` vào description.
* Phím trong `SelectorKind::Resume`:
* `tab`: đổi scope `CurrentWorkspace` ↔ `AllWorkspaces`, giữ modal mở và xóa `pending_delete_id`.
* `delete` hoặc `ctrl+d`: lần 1 đặt `pending_delete_id = Some(item.id)` và hiển thị nhắc xác nhận trên tiêu đề/status; lần 2 gọi `delete_session_file` (từ chối nếu `item.id == "__new__"` hoặc trùng `session.session().file_path()`). Không dùng phím `d` trần để giữ nguyên fuzzy search.
* `enter`: chọn `__new__` hoặc mở file session, gọi `state.invalidate_context_tokens()`, `session.clear_queues()`, `state.scroll_offset = 0`, `rebuild_transcript_items`.


* Ngoài modal: bắt `keybindings.matches(&key_str, TuiAction::SessionPicker)` để mở Session Switcher.


* **File cần sửa**: `crates/mehh-tui/src/keys.rs`, `crates/mehh-coding-agent/src/{session.rs, interactive.rs}`.


* **Why**: Tận dụng toàn bộ kiến trúc `SelectList` + `FastSessionLine` sẵn có, không thêm SQLite. Sửa phím xóa sang `delete`/`ctrl+d` + xác nhận 2 bước và siết canonical containment khắc phục triệt để 2 điểm yếu của spec 17/11.
* **Security/compat**:
* Fail-closed: `delete_session_file` chống path traversal, chống symlink, cấm xóa session đang mở.
* Giữ tương thích 100% cho `/resume`, `/new`, và JSONL v3; cập nhật test `/session` hiện có (`interactive.rs`) sang `/session info` và bổ sung test cho `/session` không đối số.



## A8 — Auto-Memory (`MEMORY.md` + `memory` tool)

* **Problem**: `TodoState::format_moim_block` (`crates/mehh-coding-agent/src/todo.rs:103-122`) chỉ đọc file `<cwd>/.mehh/MOIM.md` một chiều. Agent không có cách tự lưu kiến thức bền vững giữa các session (quy ước build của user, kiến trúc dự án, quyết định đã chốt, các lỗi từng gặp), dẫn đến lặp lại câu hỏi và mất ngữ cảnh sau mỗi `/new`.


* **Solution**:
* **Module mới**: **[Mới]** `crates/mehh-coding-agent/src/memory.rs`.
* **Cấu trúc lưu trữ**:
* Project auto-memory (tách khỏi git tree bằng đúng `cwd_hash` từ `session_dir_for_cwd`):


* `~/.mehh/projects/<cwd_hash>/memory/MEMORY.md` (chỉ mục tổng hợp, tối đa 200 dòng / 8 KiB được inject).
* `~/.mehh/projects/<cwd_hash>/memory/<topic>.md` (nội dung chi tiết có YAML frontmatter `name`, `description`, `type: user|feedback|project|reference`, `updated_at`).


* Global memory: `~/.mehh/agent/MEMORY.md` (tối đa 4 KiB).
* Giữ nguyên `<cwd>/.mehh/MOIM.md` (read-only khi `trusted == true`) cho tương thích ngược.



* **Tool `MemoryTool` (`name = "memory"`)**:
* Schema:
```json
{
  "type": "object",
  "properties": {
    "action": { "type": "string", "enum": ["list", "read", "save", "delete", "search"] },
    "scope": { "type": "string", "enum": ["project", "global"], "default": "project" },
    "topic": { "type": "string", "description": "Slug ^[a-z0-9][a-z0-9_-]{0,63}$" },
    "title": { "type": "string" },
    "description": { "type": "string" },
    "kind": { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
    "content": { "type": "string" },
    "query": { "type": "string" }
  },
  "required": ["action"]
}

```


* `execution_mode = Sequential`, `is_read_only() = None`.
* `is_safe_for_auto_approve(args)`: trả `Some(true)` cho `list`/`read`/`search`; cho `save`/`delete`: trả `Some(true)` nếu `Settings::auto_memory.unwrap_or(true)` và `scope == "project"`, ngược lại `Some(false)` (yêu cầu approval cho ghi `global` hoặc khi user tắt auto-approve memory).
* Tự động cập nhật file chỉ mục `MEMORY.md` nguyên tử (`write_file_atomic(..., true)`, UTF-8 không BOM) mỗi khi `save`/`delete` thành công.


* **Injection idempotent trong `prepare_request` (`agent_session.rs`)**:
* Dùng cặp thẻ `<auto_memory>...</auto_memory>` tương tự `MOIM_START`/`MOIM_END` (`replace_or_strip_tagged_block`) để tiêm nội dung `MEMORY.md` (cắt an toàn theo `is_char_boundary`, trần 8 KiB) vào ngữ cảnh gửi LLM mà không ghi vào JSONL.



* **TUI**: thêm slash command `/memory [list|clear|path]` và toggle `auto_memory` trong `Settings` (`/settings`).
* **File cần sửa**: `crates/mehh-coding-agent/src/memory.rs` **[Mới]**, `agent_session.rs`, `settings.rs`, `interactive.rs`, `lib.rs`.


* **Why**: Dùng thư mục `~/.mehh/projects/<cwd_hash>/memory/` tách biệt giúp agent tự học mà không làm bẩn `git status` của dự án. Dùng file Markdown + index `MEMORY.md` cho phép người dùng tự mở đọc/sửa dễ dàng thay vì nhét vào database nhị phân hay vector DB nặng nề.
* **Security/compat**:
* Fail-closed: `topic` bắt buộc khớp regex `^[a-z0-9][a-z0-9_-]{0,63}$` (cấm tuyệt đối `/`, `\`, `..`); từ chối symlink; kiểm tra secret pattern (`PRIVATE KEY`, `sk-ant-`, `sk-`, `ghp_`, `AKIA`) trước khi ghi đĩa để ngăn lưu nhầm thông tin nhạy cảm vào `MEMORY.md`.
* Strip BOM khi đọc `MEMORY.md` (nếu user chỉnh bằng Notepad); ghi ra UTF-8 không BOM.
* `Settings::auto_memory: Option<bool>` dùng `#[serde(default, skip_serializing_if = "Option::is_none")]`.



## A9 — Checkpoint / Rewind gắn Turn và Session

* **Problem**: `EditUndoStore` (`crates/mehh-tools/src/undo_edit.rs`) chỉ lưu snapshot trên RAM (tối đa 20/file, 16 MiB), mất trắng khi đóng terminal. Đồng thời `SessionManager::branch_from` (`crates/mehh-coding-agent/src/session.rs:840-863`) chỉ đổi con trỏ `leaf_id` hội thoại mà không hoàn tác thay đổi file trên đĩa. Ví dụ trace: turn 3 agent sửa 5 file nhưng hướng đi sai; user `/tree` quay về turn 2 — hội thoại quay về turn 2 nhưng 5 file vẫn mang code hỏng của turn 3, khiến model bị lệch thực tế.


* **Solution**:
* **Hook quan sát mutation trong `mehh-tools**`:
* Thêm trait nhẹ `pub trait FileMutationObserver: Send + Sync { fn on_file_mutated(&self, path: &Path, pre_bytes: Option<&[u8]>, post_bytes: Option<&[u8]>); }` trong `crates/mehh-tools/src/undo_edit.rs` (hoặc `core/fs.rs`).
* `EditTool`, `WriteTool`, `ApplyPatchTool`, `UndoEditTool` gọi `observer.on_file_mutated(...)` bên trong `with_file_mutation_queue` ngay sau khi `atomic_write`/`remove_file` thành công.



* **Content-addressed Checkpoint Store** trong **[Mới]** `crates/mehh-coding-agent/src/checkpoint.rs`:
* Lưu blob gốc không trùng lặp tại `~/.mehh/checkpoints/<session_id>/blobs/<sha256>` (chỉ viết 1 lần mỗi hash, không mất BOM hay CRLF, bỏ qua file > `4 MiB` và ghi rõ `skipped_reason: Some("file exceeds 4 MiB")`, giới hạn tổng dung lượng `~/.mehh/checkpoints/` = `256 MiB` với dọn phiên cũ nhất theo mtime).
* `TurnCheckpointRecorder`: gom mọi file bị sửa trong 1 turn; với mỗi file chỉ giữ `pre_hash` **đầu tiên** của turn đó và `post_hash` **cuối cùng** của turn đó.


* **Mở rộng JSONL v3 (`crates/mehh-coding-agent/src/session.rs`)**:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointFileRecord {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

// Thêm vào SessionEntryKind:
Checkpoint {
    #[serde(rename = "turnEntryId")]
    turn_entry_id: String,
    #[serde(default)]
    files: Vec<CheckpointFileRecord>,
}

```


* `build_context_messages()` bỏ qua `Checkpoint` (`_ => Vec::new()`) nên không tốn token LLM.


* `AgentSession::finish_prompt` tự động ghi 1 entry `SessionEntryKind::Checkpoint` nếu turn vừa rồi có mutation.


* **API Rewind (`AgentSession::rewind_to`)**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindMode {
    ConversationOnly,
    FilesOnly,
    ConversationAndFiles,
}

pub struct RewindReport {
    pub restored_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub skipped_files: Vec<(String, String)>,
    pub dirty_conflicts: Vec<String>,
}

impl AgentSession {
    pub async fn rewind_to(
        &mut self,
        target_entry_id: &str,
        mode: RewindMode,
        force: bool,
    ) -> anyhow::Result<RewindReport>;
}

```


* Thu thập các entry `Checkpoint` dọc nhánh hiện tại **sau** `target_entry_id` (ngược thời gian về `target_entry_id`).
* **Phát hiện chỉnh sửa ngoài luồng (fail-closed)**: tính SHA-256 hiện tại trên đĩa của mỗi file; nếu khác `post_hash` mới nhất và `!force`, dừng trước khi sửa bất kỳ file nào và trả `dirty_conflicts` để user quyết định!
* Trước khi khôi phục từng file (qua `with_file_mutation_queue` + `atomic_write` hoặc `remove_file`), đẩy snapshot hiện tại vào `EditUndoStore` để chính thao tác rewind cũng có thể undo.


* **TUI**: thêm `/rewind [entry_id] [--files-only|--conversation-only|--force]` và khi mở `/rewind` không tham số sẽ hiện `SelectorKind::Tree` lọc các điểm mốc User/Checkpoint kèm chọn `RewindMode`.
* **File cần sửa**: `crates/mehh-tools/src/{undo_edit.rs, edit.rs, write.rs, apply_patch.rs, lib.rs}`, `crates/mehh-coding-agent/src/checkpoint.rs` **[Mới]**, `crates/mehh-coding-agent/src/{session.rs, agent_session.rs, interactive.rs, lib.rs}`.


* **Why**: Content-addressed blobs bằng `sha2` (đã có sẵn trong workspace) không phụ thuộc `git` (chạy được cả trong thư mục chưa `git init`), không đụng index/stash của user, tránh phình JSONL session (JSONL chỉ lưu hash 64 byte), và lưu chính xác byte gốc kể cả BOM/CRLF. Không dùng `git worktree` hay shadow git repo vì rủi ro xung đột file lock trên Windows và xử lý `.gitignore` phức tạp.
* **Security/compat**:
* Fail-closed: kiểm tra `target_path` không vượt `cwd` và không nằm trong `.mehh/` trước khi ghi khôi phục; từ chối symlink; báo `dirty_conflicts` nếu user đã tự sửa file bên ngoài.
* JSONL v3 backward-compatible hoàn toàn (`SessionEntryKind::Checkpoint` có `#[serde(default)]`); file cũ không có checkpoint sẽ rewind `ConversationOnly` bình thường.



## A10 — Keybindings tùy biến & Vim Mode

* **Problem**: `Keybindings::default_bindings()` hardcode toàn bộ phím trong `crates/mehh-tui/src/keys.rs:189-274`; `Settings` không có trường tùy biến phím hay `vim_mode`; một số phím như `ctrl+t` và `alt+enter` đang so sánh chuỗi trực tiếp trong `interactive.rs`; và `Editor` chỉ hỗ trợ chế độ nhập kiểu Emacs, không có trạng thái Normal/Insert cho người dùng Vim.


* **Solution**:
* **Đăng ký & Serialize `TuiAction` (`crates/mehh-tui/src/keys.rs`)**:
* Derive `Serialize, Deserialize` (`#[serde(rename_all = "snake_case")]`) cho `TuiAction`.


* Bổ sung các action còn thiếu (xóa hoàn toàn hardcode trong `interactive.rs`):
* `ToggleThinking` (mặc định `["ctrl+t"]`)
* `TogglePlanMode` (mặc định `["backtab"]`)
* `SessionPicker` (mặc định `["ctrl+s"]`)
* `FollowUpSubmit` (mặc định `["alt+enter"]`)


* Thêm API mới trên `Keybindings`:
```rust
pub fn from_overrides(overrides: &HashMap<TuiAction, Vec<String>>) -> Self;
pub fn load_file(path: &Path) -> anyhow::Result<HashMap<TuiAction, Vec<String>>>;
pub fn validate_and_guard(&mut self); // đảm bảo Interrupt luôn có ít nhất "ctrl+c"

```


* Ví dụ `~/.mehh/agent/keybindings.json`:
```json
{
  "submit": ["ctrl+enter"],
  "newline": ["enter", "shift+enter"],
  "toggle_thinking": ["ctrl+t"],
  "session_picker": ["ctrl+s"]
}

```



* **Mở rộng `Settings` (`crates/mehh-coding-agent/src/settings.rs`)**:
* Thêm `#[serde(default, skip_serializing_if = "Option::is_none")] pub keybindings: Option<HashMap<String, Vec<String>>>` và `#[serde(default, skip_serializing_if = "Option::is_none")] pub vim_mode: Option<bool>`.
* Cập nhật `SettingsManager::merge`, `copy_toggle` (`"vim_mode"`), và `/settings` (`build_settings_list`, `apply_setting_cycle`).
* Precedence: `Keybindings::default_bindings()` ← `~/.mehh/agent/keybindings.json` ← `settings.keybindings` (global rồi trusted project, luôn chạy `validate_and_guard()` để project không thể vô hiệu hóa `ctrl+c` Interrupt).


* **Vim Mode trong `Editor` (`crates/mehh-tui/src/components/editor.rs`)**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    #[default]
    Insert,
    Normal,
}

```


* Thêm các trường trên `Editor`: `pub vim_enabled: bool` (mặc định `false`), `pub mode: EditorMode` (`Insert`), `pending_op: Option<char>` (`d`/`c`/`y`/`g`).
* State transitions khi `vim_enabled == true`:
* `Insert` --(`escape`)--> `Normal` (lùi `cursor_col` 1 ký tự nếu > 0, không xóa buffer!). `ctrl+c` vẫn truyền ra ngoài làm `Interrupt`.
* `Normal`:
* Chuyển về `Insert`: `i` (tại chỗ), `I` (đầu dòng), `a` (sau con trỏ), `A` (cuối dòng), `o` (dòng mới dưới), `O` (dòng mới trên).
* Di chuyển: `h`/`j`/`k`/`l`, `w`/`b`/`e` (tái dùng `find_word_forward`/`find_word_backward` sẵn có trong `editor.rs`), `0`/`$`/`^`, `gg`/`G`.


* Sửa & yank: `x`, `dd`, `dw`, `d$`, `cc`, `cw`, `c$`, `yy`, `p`/`P` (tái dùng `KillRing` và `undo_stack` có sẵn), `u` (undo).


* Submit: `enter` trong Normal mode vẫn gửi prompt (`EditorOutput::Submit`).
* Phím không hợp lệ trong Normal mode: xóa `pending_op`, trả `EditorOutput::None` (không chèn ký tự rác).



* **Bảo toàn `CURSOR_MARKER**`: `Editor::render` **luôn giữ nguyên** `CURSOR_MARKER = "\x1b_mehh:c\x07"` ở cả `Insert` và `Normal` mode (kể cả khi `is_empty()` hoặc wrapped multi-line).


* Tiêu đề Prompt dock trong `interactive.rs` hiển thị thêm badge `[NORMAL]` (màu `theme.accent`) hoặc `[INSERT]` (màu `theme.dim`) khi `editor.vim_enabled`.
* `/reload` trong `interactive.rs` tải lại `keybindings.json` + `settings.keybindings` + `settings.vim_mode` (thay vì reset cứng về `Keybindings::default_bindings()`).



* **File cần sửa**: `crates/mehh-tui/src/{keys.rs, components/editor.rs}`, `crates/mehh-coding-agent/src/{settings.rs, interactive.rs}`.


* **Why**: Tái sử dụng trực tiếp `KillRing`, `undo_stack`, `find_word_forward`/`find_word_backward` trong `editor.rs` giúp Vim mode gọn (~180 dòng), không cần thư viện text-buffer mới. Không implement Vim Visual mode, macro `@q`, hay Ex command `:s///` vì đó là complexity thừa đối với ô nhập prompt CLI.
* **Security/compat**:
* Fail-closed: file `keybindings.json` sai cú pháp JSON hoặc chứa tên action không tồn tại sẽ trả `Err` và báo rõ trong transcript (không âm thầm ghi đè); strip UTF-8 BOM.
* `validate_and_guard()` bảo đảm `ctrl+c` luôn thoát/hủy được turn, không bao giờ bị khóa kẹt terminal.
* `vim_mode` mặc định `false`, 100% backward-compatible cho người dùng hiện tại.



---

# Nhóm B — Hoàn thiện các tính năng còn thiếu

## B1 — `AGENTS.md` / `CLAUDE.md` nearest-wins, `@import`, `CLAUDE.local.md`, `.claude/rules/`, BOM strip

* **Problem**: `discover_context_files` (`crates/mehh-coding-agent/src/system_prompt.rs:10-50`) duyệt từ `cwd` lên tới root hệ thống và nhồi **tất cả** file tìm được vào system prompt, không strip UTF-8 BOM, không đọc `CLAUDE.local.md`, không hỗ trợ `.claude/rules/*.md`, và không giải `@import`.


* **Solution**:
* Thêm `ContextDiscoveryOptions` (`nearest_wins: bool` mặc định `true`, `stop_at_git_root: bool` mặc định `true`).
* Trong `discover_context_files_with_options(cwd, opts)`:
1. Dừng duyệt khi gặp `.git` (rễ repo) để không vô tình hút `AGENTS.md` ở thư mục cha không liên quan.
2. Strip BOM (`strip_prefix('\u{feff}')`) cho mọi file đọc được.
3. Ở chế độ `nearest_wins` (mặc định): chọn file hướng dẫn gần `cwd` nhất (`AGENTS.override.md` > `AGENTS.md` > `CLAUDE.md`), cộng thêm `CLAUDE.local.md` tại cùng thư mục (nếu có) và các rule trong `.claude/rules/*.md` (nếu frontmatter có `paths: ["crates/mehh-tui/**"]`, lọc bằng `globset`).
4. Hỗ trợ directive `@import <relative_path>` trên dòng riêng: độ sâu tối đa `MAX_IMPORT_DEPTH = 5`, chống vòng lặp bằng `HashSet<PathBuf>` canonical, tổng kích thước trần `64 KiB`.


* **Why**: Nearest-wins + dừng tại `.git` ngăn prompt phình to trong monorepo và ngăn rò rỉ rule ngoài dự án.
* **Security/compat**: **Fail-closed containment** cho `@import`: đường dẫn sau khi canonicalize bắt buộc nằm trong `repo_root` (từ chối tuyệt đối `@import ~/.ssh/id_rsa`, `@import ../../etc/passwd`, hay symlink trỏ ra ngoài repo). Giữ hàm `discover_context_files(cwd)` cũ gọi sang bản mới để không vỡ caller/tests.

## B2 — Phân cấp thư mục con cho Config / Skills / Templates / MOIM

* **Problem**: `settings.rs`, `skills.rs`, `prompt_templates.rs`, và `todo.rs` chỉ đọc trực tiếp dưới `<cwd>/.mehh/`. Nếu chạy `mehh` từ thư mục con (ví dụ `crates/mehh-tui`), agent bỏ sót cấu hình và skills đặt ở root monorepo (hoặc ngược lại không kế thừa phân tầng).


* **Solution**:
* Thêm helper chung `crate::trust::collect_trusted_ancestors(cwd: &Path, trusted: bool) -> Vec<PathBuf>` trả về chuỗi thư mục từ `repo_root` (dừng ở `.git`) xuống `cwd` khi `trusted == true` (trả rỗng khi `trusted == false`).
* `SettingsManager::try_load_with_trust`: merge tuần tự `global` → `ancestor_0` → ... → `cwd`, áp dụng **escalation guard** (`permission_mode = Auto` và `network.allow_domains = ["*"]` bị vô hiệu hóa ở mọi tầng dự án).


* `load_skills_with_trust`, `load_prompt_templates_with_trust`, `TodoState::format_moim_block`: duyệt `global` → `ancestors` → `cwd`, thư mục gần `cwd` nhất ghi đè mục trùng tên.


* **Why**: Một helper `collect_trusted_ancestors` duy nhất giải quyết nhất quán cả 4 loader trong ~40 dòng code, không lặp logic walk-up.
* **Security/compat**: Chỉ áp dụng khi `trusted == true`; dừng cứng tại `.git` nên không bao giờ đọc `.mehh/` từ thư mục cha ngoài workspace.

## B3 — Export Session ra Markdown / HTML / Plain Text

* **Problem**: `SessionManager::export_jsonl` và `export_session` (`crates/mehh-coding-agent/src/session.rs:526-550, 960-971`) chỉ xuất JSONL thô, khó đọc hoặc chia sẻ báo cáo phiên làm việc.


* **Solution**:
* Thêm `pub enum ExportFormat { Jsonl, Markdown, Text, Html }` và `ExportFormat::from_path_or_flag(path: Option<&Path>, format: Option<&str>) -> Result<ExportFormat>`.
* Thêm `SessionManager::export_formatted(&self, output_path: &Path, format: ExportFormat) -> Result<()>` và `render_session_as(&self, format: ExportFormat) -> String`:
* `Markdown`: tiêu đề session, metadata (ID, model, cwd, timestamp), các lượt User/Assistant/ToolCall (gồm diff code block) theo nhánh active (`active_path_entries()`).
* `Text`: văn bản thuần không ANSI, phân tách rõ vai trò.
* `Html`: tài liệu HTML5 tự chứa với `<style>` tối giản (dark/light bằng `prefers-color-scheme`), **escape 100% HTML entities** (`&`, `<`, `>`, `"`, `'`), không nhúng `<script>` hay CDN ngoài.


* Cập nhật `/export [path] [--format md|html|txt|jsonl]` (`interactive.rs`) và `--export <in> [out] [--format ...]` (`mehh-cli/src/main.rs`).


* **Why**: Zero external dependencies; dùng nhánh active để báo cáo sạch sẽ, trong khi JSONL vẫn giữ nguyên toàn bộ cây nhánh cho `/import`.
* **Security/compat**: Escape HTML đầy đủ chống XSS khi mở file export bằng trình duyệt; UTF-8 không BOM; mặc định vẫn là `Jsonl` nếu không rõ đuôi file.

## B4 — Token/Cost Budget Limits & Context Warning bằng chữ cho Model

* **Problem**: `InteractiveState` chỉ cộng dồn `total_input_tokens`/`total_output_tokens`/`total_cost` để hiển thị footer; headless/RPC không có giới hạn ngân sách; thanh `ctx_bar` chỉ báo cho người dùng mà không đưa hướng dẫn bằng chữ cho model biết context sắp đầy.


* **Solution**:
* Thêm `BudgetConfig` vào `Settings` (`settings.rs`, `#[serde(default)]`):
```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetConfig {
    pub max_cost_usd: Option<f64>,
    pub max_session_tokens: Option<u64>,
    pub warn_at_pct: Option<u8>,       // mặc định 80
    pub context_warn_pct: Option<u8>,  // mặc định 80
}

```


* Chuyển bộ đếm `SessionUsageTracker` (`Arc<Mutex<SessionUsageTracker>>`) lên `AgentSession` (cập nhật trong `finish_prompt` và `after_tool_call` / `prepare_next_turn`) để áp dụng đồng nhất cho cả interactive, print, và RPC.
* **Model guidance**: trong `prepare_request`, khi `ctx_pct >= context_warn_pct.unwrap_or(80)`, chèn block ephemeral `<context_budget>Context window is at {ctx_pct}% ({used}/{limit} tokens). Prefer targeted reads, concise outputs, or summarize progress before hitting auto-compaction.</context_budget>`.
* **Budget enforcement (fail-closed)**: khi `total_cost >= max_cost_usd` hoặc `total_tokens >= max_session_tokens`, `finish_turn` trả `TurnAction::End` và `before_tool_call` chặn tool tiếp theo với lý do rõ ràng; người dùng có thể nới trong phiên bằng `/budget [max_cost_usd|off]`.


* **Why**: Ngăn cháy ngân sách trong vòng lặp dài (đặc biệt khi dùng subagent song song) và giúp model tự tiết kiệm token trước khi chạm ngưỡng compaction.
* **Security/compat**: Project `.mehh/settings.json` không thể nới rộng hoặc tắt `max_cost_usd` đã đặt ở global `~/.mehh/agent/settings.json` (chỉ được đặt ngưỡng thấp hơn hoặc bằng!).

## B5 — Auto Session Title

* **Problem**: `list_sessions` (`crates/mehh-coding-agent/src/session.rs:439-466`) chỉ cắt 80 ký tự đầu của prompt đầu tiên nếu user không tự gõ `/name`, dẫn đến tên session xấu khi prompt bắt đầu bằng `@file` hoặc paste log lỗi.


* **Solution**:
* Thêm `#[serde(default)] pub auto_title: Option<AutoTitleMode>` (`Heuristic` mặc định | `Off`) vào `Settings`.
* Hàm thuần `derive_session_title(first_user_text: &str) -> Option<String>` trong `session.rs`: lọc bỏ block `<file ...>...</file>`, token `@path`, tiền tố slash/markdown, gộp khoảng trắng, lấy câu/cụm có nghĩa đầu tiên tối đa 60 ký tự (char-safe).
* Trong `AgentSession::finish_prompt`: nếu `self.session.session_name().is_none()` và `auto_title != Off`, tự động gọi `self.session.set_session_name(Some(title))` ngay sau turn đầu tiên hoàn tất.


* **Why**: Heuristic nội bộ chạy mất 0ms, 0 token, hoạt động offline 100%, và vẫn cho phép user ghi đè bất cứ lúc nào bằng `/name`. Không cần gọi thêm 1 LLM request tốn chi phí/độ trễ mặc định.
* **Security/compat**: Ghi qua entry `SessionEntryKind::SessionInfo` sẵn có của JSONL v3; không đổi định dạng file.



## B6 — Nối `is_safe_for_auto_approve` vào Pipeline Governance

* **Problem**: `AgentTool::is_safe_for_auto_approve(&self, args)` đã định nghĩa tại `crates/mehh-agent/src/types.rs:73-75` và được `TodoTool` override tại `crates/mehh-coding-agent/src/todo.rs:235-244`, nhưng `AgentSession` và `ToolInspectionManager` hoàn toàn không gọi hàm này mà chỉ đọc `is_read_only()` và hardcode ngoại lệ `ctx.tool_name == "todo"`.


* **Solution**:
* Thêm `pub is_safe_for_auto_approve: Option<bool>` vào `ToolInspectionContext` (`tool_inspection.rs`).
* Trong `agent_session.rs` (cả `before_tool_call` và `create_codemode_tool` interceptor): gọi `tool.is_safe_for_auto_approve(&args)` và gán vào `ToolInspectionContext`.
* Trong `PermissionInspector::inspect`: ở `SmartApprove`, ưu tiên `ctx.is_safe_for_auto_approve == Some(true)` (fallback `ctx.is_read_only == Some(true)`). Điều này lập tức thống nhất quản trị theo đối số cho `todo`, `web_fetch` (A3), `lsp` (A4), `memory` (A8), `shell_output` (A6), và bỏ dần ngoại lệ hardcode theo tên tool.
* Chú ý giữ `todo` mutating (`add`/`update`/`replace`/`clear`) vẫn được `Allow` ở `SmartApprove` và `Plan` vì `todo` chỉ sửa bộ nhớ checklist trong phiên — cập nhật `TodoTool::is_safe_for_auto_approve` trả `Some(true)` cho mọi action của `todo` (hoặc giữ rule rõ ràng trong `PermissionInspector`).


* **Why**: Bít lỗ hổng thiết kế dead-code hiện có và tạo một điểm mở rộng duy nhất cho mọi tool đa-action (vừa có action đọc, vừa có action ghi).
* **Security/compat**: Fail-closed: `is_safe_for_auto_approve` trả `None` hoặc `Some(false)` luôn rơi vào `RequireApproval` ở `SmartApprove` và `Deny` ở `Chat`/`Plan`.

## B7 — `PowerShellTool` Sandbox Plan & Củng cố Sandbox Windows

* **Problem**: `BashTool` (`crates/mehh-tools/src/bash.rs:101-110`) truyền `Some(&SandboxPlan)` vào `run_shell`, nhưng `PowerShellTool` (`crates/mehh-tools/src/powershell.rs`) bỏ trống `None`; còn trên Windows, `mehh-sandbox/src/lib.rs:366-374` luôn trả `SandboxStatus::Degraded` vì chỉ scrub biến môi trường.


* **Solution**:
* `crates/mehh-tools/src/powershell.rs`: thêm trường `pub sandbox_policy: SandboxPolicy` (mặc định `WorkspaceWrite`) + `with_sandbox_policy`, tạo `SandboxPlan::new(self.sandbox_policy, self.cwd.clone())`, truyền `Some(&plan)` vào `run_shell`, và kiểm tra `mehh_sandbox::detect_violation` hệt như `BashTool`.


* `crates/mehh-sandbox/src/lib.rs` (Windows hardening không thêm dependency nặng):
* Chạy `plan.prepare()?` trước khi trả `Degraded` (bảo đảm kiểm tra symlink TOCTTOU chạy trên cả Windows — hiện `wrap` đã gọi `prepare()`, giữ nguyên bất biến này).


* Thiết lập `TEMP`/`TMP` riêng nằm trong thư mục tạm cô lập theo workspace (`<temp>/mehh-sb-<pid>`) và thêm biến môi trường đánh dấu `MEHH_SANDBOX=degraded` để build script nhận biết.
* Giữ trung thực `SandboxStatus::Degraded` trên Windows (tuyệt đối không giả mạo `Active` khi chưa có AppContainer/Job Object/WFP kernel isolation). Ghi nhận phương án Windows Job Object + Restricted Token qua crate `windows-sys` (tùy chọn feature flag) ở Phần 4 để người dùng quyết định.



* **Why**: Đồng bộ hóa bảo mật giữa `bash` và `powershell` với thay đổi tối thiểu; trung thực về mức độ cách ly OS đúng triết lý `mehh-sandbox`.


* **Security/compat**: Fail-closed khi `prepare()` phát hiện protected path qua writable symlink; không thêm dependency C/Win32 nặng ngoài ý muốn.

## B8 — Ảnh từ Clipboard & Ranh giới Multimodal / Voice

* **Problem**: Hiện ảnh chỉ vào qua `read` file ảnh local hoặc `@image.png`; chưa thể dán trực tiếp ảnh chụp màn hình từ clipboard vào TUI. Computer use (chụp màn hình/điều khiển chuột), Kitty/Sixel graphics protocol, và voice đều chưa có.


* **Solution**:
* **Trong phạm vi (nhẹ, 0 crate mới)**: thêm `clipboard::read_clipboard_image_png() -> anyhow::Result<Option<Vec<u8>>>` trong `crates/mehh-coding-agent/src/clipboard.rs` (Windows dùng `powershell.exe` `System.Windows.Forms.Clipboard::GetImage()` xuất PNG; macOS dùng `osascript`/`pngpaste`; Linux dùng `wl-paste -t image/png` hoặc `xclip -selection clipboard -t image/png -o`), gắn với lệnh `/paste-image` và `TuiAction::PasteImage` (`ctrl+alt+v`) trong `interactive.rs`. Ảnh đọc được lưu vào `InteractiveState.pending_images: Vec<ImageContent>` (hiển thị chip `[image #1 42KB]` trên tiêu đề dock, giới hạn `MAX_IMAGE_FILE_BYTES = 20 MiB` giống `read.rs`) và đính kèm vào `UserContent::Blocks` ở lần submit tiếp theo.


* **Hoãn có chủ đích (Non-goal)**: Desktop screenshot / computer use (rủi ro bảo mật cao, cần permission model riêng), Kitty/Sixel inline image rendering (nhiều lỗi tương thích trên Windows Terminal/ConPTY), và Voice STT/TTS (cần audio codec + WebRTC/streaming API ngoài phạm vi CLI thuần).


* **Why**: Paste ảnh từ clipboard giải quyết 90% nhu cầu gửi screenshot lỗi UI cho agent trong 100 dòng code mà không kéo dependency audio/graphics nặng.
* **Security/compat**: Ảnh chỉ đọc khi user chủ động nhấn phím/gõ lệnh (không tự đọc clipboard nền); giới hạn cứng 20 MiB; lỗi không có ảnh trong clipboard trả thông báo sạch không crash.

---

# Phần 4 — Rủi ro và quyết định mở

## 4.1 Bảng rủi ro và cách giảm thiểu

| # | Rủi ro | Mức độ | Cách giảm thiểu |
| --- | --- | --- | --- |
| R1 | **Xung đột với uncommitted work tại `interactive.rs:3316-3352`** (đợt TUI UX optimization) | **Cao** | Tách `SelectorKind::HunkApproval` và `SelectorKind::PlanApproval` thành nhánh hoàn toàn độc lập; bắt buộc chạy `git diff -- crates/mehh-coding-agent/src/interactive.rs` trước và sau khi sửa. |
| R2 | **SSRF / DNS rebinding / redirect bypass trên `WebFetchTool` (A3)** | **Cao** | Cấm redirect tự động (`Policy::none()`), kiểm tra `validate_url` + `resolve_and_check_ssrf` trên từng hop (tối đa 5), pin IP đã duyệt qua `resolve_to_addrs`, cấm `file://` và `userinfo@`, áp cả trong `SecurityInspector` ở mọi mode. |
| R3 | **Prompt injection tự chèn hook hoặc lệnh LSP độc hại qua repo untrusted (A2, A4)** | **Cao** | Gate `.mehh/hooks.json` và `Settings::lsp` bằng `trusted == true` (`has_local_trust_targets`); khóa ghi `.mehh/*` và `~/.mehh/agent/*` trong `SecurityInspector`; scrub mọi env bí mật trước khi spawn child process. |
| R4 | **Rò rỉ tiến trình nền / LSP zombie trên Windows (A4, A6)** | **Trung bình** | Dùng `kill_process_tree` (`taskkill /PID ... /T /F` trên Windows, `kill -KILL -<pgid>` trên Unix), giới hạn cứng `MAX_BG_SHELLS = 8`, idle timeout 300s cho LSP, và gọi `shutdown_*` tại mọi đường thoát CLI/TUI/RPC.

 |
| R5 | **Phình dung lượng đĩa từ checkpoint blobs (A9)** | **Trung bình** | Content-addressed theo SHA-256 (trùng nội dung không lưu lại), bỏ qua file > 4 MiB, giới hạn tổng `~/.mehh/checkpoints/` = 256 MiB với cơ chế dọn phiên cũ nhất theo mtime. |
| R6 | **Xung đột phím giữa fuzzy filter, Vim Normal mode và các phím tắt modal (A7, A10)** | **Trung bình** | Không dùng ký tự chữ đơn (`d`) làm phím hành động trong modal có filter (`Resume` dùng `delete`/`ctrl+d`); khi modal đang mở, modal chặn input trước `Editor` (nên Vim mode không làm hỏng picker); luôn chạy `Keybindings::validate_and_guard()` bảo vệ `ctrl+c`. |
| R7 | **LSP URI & UTF-16 column mismatch trên Windows (A4)** | **Trung bình** | Viết hàm chuyển đổi `path_to_file_uri`/`file_uri_to_path` có test riêng cho ổ đĩa Windows (`C:\...` ↔ `file:///c:/...`) và quy đổi char offset sang UTF-16 code units trước khi gửi `position.character`. |
| R8 | **Thay đổi hành vi mặc định của `AGENTS.md` (B1) làm hụt rule ở repo đa tầng** | **Thấp** | Cung cấp tùy chọn `contextFileMode: "nearest"`

## 4.2 Các quyết định mở cần bạn chốt trước khi implement

1. **A3 — Search provider mặc định cho `web_search**`: Bạn muốn ưu tiên tích hợp backend nào trước: (a) **Tavily + Brave + SearXNG** (API key chuẩn, ổn định, fail-closed khi chưa cấu hình), hay (b) thêm fallback không cần key (như DuckDuckGo HTML endpoint, có rủi ro bị rate-limit/CAPTCHA)?
2. **A4 — LSP client**: Bạn đồng ý phương án **0 dependency mới** (tự viết ~350 dòng JSON-RPC `Content-Length` stdio client), hay muốn chấp nhận thêm crate `lsp-types` (~400 KB compile overhead, không cần C compiler) để có sẵn full type schema LSP?
3. **A9 — Vị trí lưu checkpoint blobs**: Bạn muốn lưu tập trung tại **`~/.mehh/checkpoints/<session_id>/blobs/`** (sạch `git status`, đồng bộ với `~/.mehh/sessions/`), hay lưu trong `<cwd>/.mehh/checkpoints/`?
4. **A1 — Phím tắt chuyển Plan mode**: Bạn muốn `Shift+Tab` (`backtab`, giống Claude Code, hiện chưa bị phím nào chiếm) hay phím khác?
5. **B7 — Sandbox Windows**: Hiện thiết kế giữ 0 dependency Win32 mới (cô lập `TEMP`/`TMP` + scrub env + báo trung thực `Degraded`). Bạn có muốn mở một wave riêng dùng `windows-sys` để làm Windows Job Object (kill-on-close) + Restricted Token không?
6. **B1 — Mặc định `AGENTS.md` discovery**: Bạn muốn mặc định đổi sang **`nearest`** (dừng ở `AGENTS.md` gần `cwd` nhất + hỗ trợ `@import`), hay giữ mặc định `hierarchy` (trong phạm vi `.git`) và chỉ bật `nearest` khi gặp `AGENTS.override.md`?
