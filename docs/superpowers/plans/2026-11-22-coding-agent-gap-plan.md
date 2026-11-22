# Phần 3 — Implementation plan kiểu superpowers

> **Target file:** `docs/superpowers/plans/2026-11-22-coding-agent-gap-plan.md`
> **Spec:** `docs/superpowers/specs/2026-11-22-coding-agent-gap-design.md`

## Global Constraints

* **Không sửa source ở bước thiết kế này**; khi thực thi plan, không commit trừ khi người dùng yêu cầu rõ.
* **Vùng cấm chỉnh sửa**: tuyệt đối không đụng vào permission-rule block tại `crates/mehh-coding-agent/src/interactive.rs:3316-3352`. Trước mọi task chạm `interactive.rs`, chạy `git diff -- crates/mehh-coding-agent/src/interactive.rs` để xác nhận.
* **Chỉ chạy lệnh debug** trên laptop này: `cargo test -p <crate> --lib`, `cargo check -p <crate> --tests`, `cargo clippy -p <crate> --tests`. Không chạy `--release`.
* **Zero dependency nặng mới**: dùng hoàn toàn workspace dependencies hiện có (`tokio`, `reqwest`, `serde`, `serde_json`, `regex`, `globset`, `ignore`, `similar`, `sha2`, `uuid`, `crossterm`, `unicode-width`, `unicode-segmentation`, `tracing`, `anyhow`, `thiserror`).
* **TDD nghiêm ngặt**: viết test failing → chạy thấy đỏ → implement tối thiểu → chạy thấy xanh trước khi chuyển task.
* **Fail-closed & BOM hygiene**: phân biệt `ENOENT` với JSON corrupt; strip UTF-8 BOM khi đọc; ghi UTF-8 không BOM qua `write_file_atomic`.
* **TUI invariants**: mọi màu lấy từ `Theme` (không hardcode SGR color); mọi phép tính độ rộng qua `mehh_tui::visible_width` / `wrap_text_with_ansi` / `truncate_to_width` / `slice_by_column`; bảo toàn `CURSOR_MARKER = "\x1b_mehh:c\x07"` trong mọi đường dẫn render `Editor`; không gọi `renderer.reset()` ngoài nhánh `Event::Resize`.
* **Backward compatibility**: mọi field mới trên struct serialize phải có `#[serde(default)]`; JSONL v3 phải đọc được mọi file cũ; mọi keybinding mới phải đăng ký trong `TuiAction`/`Keybindings`.

## Review Focus

1. **Plan mode isolation (A1)**: `PermissionMode::Plan` không cho phép `write`/`edit`/`apply_patch`/`bash`/`subagent(coding)` lọt qua, và từ chối `exit_plan_mode` phải giữ nguyên ở `Plan` mode (Task 2).
2. **Hook security & trust boundary (A2)**: `.mehh/hooks.json` bị bỏ qua hoàn toàn khi `trusted == false`; hook không thể ghi đè `SecurityInspector::Deny`; `Stop` hook bị chặn vòng lặp ở mức 3 (Task 3).
3. **SSRF & redirect hop guard (A3)**: `WebFetchTool` chặn `127.0.0.1`, `169.254.169.254`, `::1`, và chặn redirect `[https://allowed.test](https://allowed.test) → [http://127.0.0.1](http://127.0.0.1)` (Task 4).
4. **Hunk approval không đụng vùng cấm (A5)**: chấp nhận 1/2 hunk chỉ sửa đúng hunk #1; multi-file patch có 1 file `.mehh/settings.json` bị chặn toàn bộ; block `interactive.rs:3316-3352` nguyên vẹn 0 diff (Task 6).
5. **Session switcher filter vs delete (A7)**: gõ `'d'` trong modal `Resume` lọc chuỗi thay vì xóa file; `delete_session_file` chặn symlink và path ngoài `~/.mehh/sessions/` (Task 8).
6. **Checkpoint dirty-file conflict (A9)**: rewind từ chối ghi đè file đã bị user sửa ngoài luồng khi `force == false` và khôi phục chính xác BOM + CRLF khi `force == true` (Task 10).
7. **Vim mode cursor & interrupt safety (A10)**: `Editor::render` ở `Normal` mode luôn chứa `CURSOR_MARKER`; project config không thể xóa `ctrl+c` khỏi `TuiAction::Interrupt` (Task 11).

---

### Task 1 (Nền tảng — B6 + A5 core): Nối `is_safe_for_auto_approve` & `BeforeToolCallResult.modified_args`

**Files:**

* Modify: `crates/mehh-agent/src/types.rs`, `crates/mehh-agent/src/agent_loop.rs`
* Modify: `crates/mehh-coding-agent/src/tool_inspection.rs`, `crates/mehh-coding-agent/src/agent_session.rs`, `crates/mehh-coding-agent/src/todo.rs`
* Test: `crates/mehh-agent/src/agent_loop.rs` (`mod tests`), `crates/mehh-coding-agent/src/tool_inspection.rs` (`mod tests`)

**Interfaces:**

* Consumes: `AgentTool::is_safe_for_auto_approve(&self, args)` (`mehh-agent/src/types.rs`), `BeforeToolCallResult`
* Produces: `BeforeToolCallResult.modified_args: Option<serde_json::Map<String, serde_json::Value>>`, `ToolInspectionContext.is_safe_for_auto_approve: Option<bool>`, `ApprovalOutcome { Allow, AllowPartial { accepted_hunks }, Deny }`, `ApprovalHandler::request_approval_ext`
* [ ] **Step 1 (RED):** Viết test `before_tool_call_can_override_tool_arguments` trong `crates/mehh-agent/src/agent_loop.rs` kiểm tra `modified_args` được truyền vào `tool.execute(...)`, và test `permission_inspector_uses_is_safe_for_auto_approve` trong `crates/mehh-coding-agent/src/tool_inspection.rs` kiểm tra tool có `is_read_only = None` nhưng `is_safe_for_auto_approve = Some(true)` được `Allow` trong `SmartApprove`, còn `Some(false)` trả `RequireApproval`.
* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-agent --lib before_tool_call_can_override_tool_arguments && cargo test -p mehh-coding-agent --lib permission_inspector_uses_is_safe_for_auto_approve`
Expected: FAIL (chưa có field `modified_args` và `is_safe_for_auto_approve`).
* [ ] **Step 3 (GREEN):** Thêm `modified_args` vào `BeforeToolCallResult` và nối vào `execute_single_tool`; thêm `is_safe_for_auto_approve` vào `ToolInspectionContext`, `ApprovalOutcome` + `request_approval_ext` vào `tool_inspection.rs`; truyền `tool.is_safe_for_auto_approve(&args)` tại `agent_session.rs` (cả `before_tool_call` lẫn `create_codemode_tool`).
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-agent --lib && cargo test -p mehh-coding-agent --lib tool_inspection`
Expected: PASS, 0 failures.

---

### Task 2 (A1): Plan Mode (`PermissionMode::Plan`, `enter_plan_mode`, `exit_plan_mode`, `/plan`)

**Files:**

* Create: `crates/mehh-coding-agent/src/plan_mode.rs`
* Modify: `crates/mehh-tui/src/keys.rs`
* Modify: `crates/mehh-ai/src/anthropic.rs`
* Modify: `crates/mehh-coding-agent/src/{lib.rs, tool_inspection.rs, agent_session.rs, interactive.rs}`
* Test: `crates/mehh-coding-agent/src/plan_mode.rs` (`mod tests`), `crates/mehh-coding-agent/src/interactive.rs` (`mod tests`)

**Interfaces:**

* Consumes: `ToolInspectionManager::set_mode`, `ApprovalHandler`, `write_file_atomic`
* Produces: `PermissionMode::Plan`, `PlanModeState`, `EnterPlanModeTool`, `ExitPlanModeTool`, `TuiAction::TogglePlanMode` (`backtab`), `SelectorKind::PlanApproval`
* [ ] **Step 1 (RED):** Viết các test:
* `plan_mode_blocks_mutating_tools_and_allows_read_only` (`plan_mode.rs`)
* `exit_plan_mode_rejected_keeps_plan_mode_active` (`plan_mode.rs`)
* `exit_plan_mode_approved_writes_plan_and_restores_previous_mode` (`plan_mode.rs`)
* `anthropic_oauth_maps_enter_and_exit_plan_mode_aliases` (`anthropic.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-coding-agent --lib plan_mode && cargo test -p mehh-ai --lib anthropic_oauth_maps_enter_and_exit_plan_mode_aliases`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `plan_mode.rs`, thêm `PermissionMode::Plan` trong `tool_inspection.rs`, đăng ký `TuiAction::TogglePlanMode` (`backtab`) trong `keys.rs`, nối `PlanModeState` + `<plan_mode>` vào `prepare_request` (`agent_session.rs`), thêm `/plan` + badge `[plan]` + `SelectorKind::PlanApproval` trong `interactive.rs`, thêm alias mapping trong `anthropic.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tui --lib keys && cargo test -p mehh-ai --lib anthropic && cargo test -p mehh-coding-agent --lib plan_mode`
Expected: PASS.

---

### Task 3 (A2): User-Configurable Hooks (`hooks.json` & `HookRunner`)

**Files:**

* Create: `crates/mehh-coding-agent/src/hooks.rs`
* Modify: `crates/mehh-coding-agent/src/{lib.rs, trust.rs, agent_session.rs, interactive.rs}`
* Test: `crates/mehh-coding-agent/src/hooks.rs` (`mod tests`), `crates/mehh-coding-agent/src/trust.rs` (`mod tests`)

**Interfaces:**

* Consumes: `mehh_sandbox::{SandboxPlan, is_secret_env_key}`, `trust::has_local_trust_targets`, `AgentLoopConfig::{before_tool_call, after_tool_call, finish_turn}`
* Produces: `HooksConfigFile`, `HookEvent`, `HookEntry`, `HookDecision`, `HookRunner`
* [ ] **Step 1 (RED):** Viết các test:
* `hooks_config_strips_bom_allows_enoent_fails_closed_on_corrupt_json` (`hooks.rs`)
* `untrusted_workspace_skips_project_hooks_json` (`hooks.rs` & `trust.rs`)
* `pre_tool_use_exit_2_blocks_tool_and_cannot_override_security_deny` (`hooks.rs`)
* `stop_hook_continuation_is_bounded_to_three_loops` (`agent_session.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-coding-agent --lib hooks`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `hooks.rs` (load/merge global + trusted project, regex matcher, bounded subprocess với stdin JSON + secret env scrub + timeout kill tree), cập nhật `has_local_trust_targets` nhận diện `.mehh/hooks.json`, nối vào `before_tool_call`, `after_tool_call`, `finish_turn` trên `AgentSession`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-coding-agent --lib hooks && cargo test -p mehh-coding-agent --lib trust`
Expected: PASS.

---

### Task 4 (A3): Network Policy, `WebFetchTool` & `WebSearchTool`

**Files:**

* Create: `crates/mehh-tools/src/net_policy.rs`
* Create: `crates/mehh-tools/src/web_fetch.rs`
* Create: `crates/mehh-tools/src/web_search.rs`
* Modify: `crates/mehh-tools/src/lib.rs`
* Modify: `crates/mehh-coding-agent/src/{settings.rs, tool_inspection.rs, agent_session.rs}`
* Modify: `crates/mehh-ai/src/anthropic.rs`
* Test: `crates/mehh-tools/src/net_policy.rs`, `web_fetch.rs`, `web_search.rs` (dùng `tokio::net::TcpListener` loopback mock với flag test-only hoặc kiểm tra chặn trực tiếp)

**Interfaces:**

* Consumes: `reqwest::Client`, `tokio::net::lookup_host`, `AgentTool::is_safe_for_auto_approve`
* Produces: `NetworkPolicy`, `WebFetchTool`, `WebSearchTool`, `Settings.network`
* [ ] **Step 1 (RED):** Viết các test:
* `net_policy_blocks_ssrf_private_loopback_and_metadata_ips` (`net_policy.rs`)
* `net_policy_blocks_file_scheme_and_userinfo_credentials` (`net_policy.rs`)
* `net_policy_redacts_api_keys_in_urls_and_logs` (`net_policy.rs`)
* `web_fetch_auto_approve_only_for_allowlisted_domains` (`web_fetch.rs`)
* `project_settings_cannot_widen_network_allow_domains_with_wildcard` (`settings.rs`)
* `security_inspector_blocks_web_fetch_to_localhost_in_auto_mode` (`tool_inspection.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tools --lib net_policy && cargo test -p mehh-tools --lib web_fetch`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `net_policy.rs` (domain matching, SSRF IPv4/IPv6 filter, hop-by-hop redirect guard, body byte cap, HTML stripper, `<untrusted_web_content>` wrapper), `web_fetch.rs`, `web_search.rs`; thêm `NetworkPolicy` vào `Settings` + escalation guard; bổ sung rule kiểm tra URL vào `SecurityInspector` và đăng ký vào `AgentSession` / `select_coding_tools`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tools --lib net_policy && cargo test -p mehh-tools --lib web_fetch && cargo test -p mehh-coding-agent --lib settings`
Expected: PASS.

---

### Task 5 (A4): Lightweight Pure-Rust LSP Client & `LspTool`

**Files:**

* Create: `crates/mehh-tools/src/lsp.rs`
* Modify: `crates/mehh-tools/src/lib.rs`
* Modify: `crates/mehh-coding-agent/src/{settings.rs, agent_session.rs}`
* Test: `crates/mehh-tools/src/lsp.rs` (`mod tests` với mock stdio framing in-memory)

**Interfaces:**

* Consumes: `tokio::process::Command`, `mehh_sandbox::is_secret_env_key`, `with_file_mutation_queue`, `EditUndoStore`
* Produces: `LspServerConfig`, `LspSettings`, `LspManager`, `LspTool`, framing helpers `encode_lsp_frame` / `read_lsp_frame`, `path_to_file_uri` / `file_uri_to_path`
* [ ] **Step 1 (RED):** Viết các test:
* `lsp_content_length_framing_roundtrips_multibyte_utf8` (`lsp.rs`)
* `windows_and_unix_file_uri_roundtrip_preserves_drive_letter` (`lsp.rs`)
* `lsp_tool_reports_missing_server_explicitly_without_regex_fallback` (`lsp.rs`)
* `lsp_tool_auto_approve_is_true_for_definition_and_false_for_rename` (`lsp.rs`)
* `untrusted_project_cannot_inject_custom_lsp_command` (`settings.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tools --lib lsp`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `lsp.rs` (framing, URI normalization, lazy server spawn với env scrub & idle timeout, `definition`/`references`/`hover`/`diagnostics`/`workspace_symbols`/`rename` qua `with_file_mutation_queue` + `EditUndoStore`), gắn `LspSettings` vào `Settings` (gated by `trusted`) và đăng ký `LspTool` trong `AgentSession`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tools --lib lsp && cargo test -p mehh-coding-agent --lib settings`
Expected: PASS.

---

### Task 6 (A5): `ApplyPatchTool` & Per-Hunk Interactive Approval

**Files:**

* Create: `crates/mehh-tools/src/core/text.rs`
* Create: `crates/mehh-tools/src/apply_patch.rs`
* Modify: `crates/mehh-tools/src/{core/mod.rs, edit.rs, lib.rs}`
* Modify: `crates/mehh-coding-agent/src/{tool_inspection.rs, agent_session.rs, interactive.rs}` (**không đụng `interactive.rs:3316-3352**`)
* Test: `crates/mehh-tools/src/apply_patch.rs` (`mod tests`), `crates/mehh-coding-agent/src/interactive.rs` (`mod tests`)

**Interfaces:**

* Consumes: `BeforeToolCallResult.modified_args`, `ApprovalOutcome::AllowPartial`, `with_file_mutation_queue`, `EditUndoStore`
* Produces: `ApplyPatchTool`, `ParsedPatch`, `PatchHunk`, `SelectorKind::HunkApproval`, `PendingHunkApproval`
* [ ] **Step 1 (RED):** Viết các test:
* `apply_patch_applies_only_accepted_hunks_and_preserves_bom_crlf` (`apply_patch.rs`)
* `apply_patch_rejects_all_hunks_without_touching_disk` (`apply_patch.rs`)
* `security_inspector_blocks_multi_file_patch_touching_dot_mehh` (`tool_inspection.rs`)
* `hunk_approval_selector_toggles_individual_hunks_and_renders_within_60x20` (`interactive.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tools --lib apply_patch && cargo test -p mehh-coding-agent --lib hunk_approval`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Tách `core/text.rs` từ `edit.rs`; implement `apply_patch.rs`; mở rộng `SecurityInspector` kiểm tra mọi file trong `patch`; implement `SelectorKind::HunkApproval` độc lập hoàn toàn với nhánh `SelectorKind::ToolApproval` trong `interactive.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tools --lib apply_patch && cargo test -p mehh-tools --lib edit && cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS; xác nhận `git diff -U0 -- crates/mehh-coding-agent/src/interactive.rs` không chạm dòng `3316-3352`.

---

### Task 7 (A6 + B7): Background Shell Manager, `PowerShellTool` Sandbox & Cleanup

**Files:**

* Create: `crates/mehh-tools/src/bg_shell.rs`
* Modify: `crates/mehh-tools/src/{core/shell.rs, bash.rs, powershell.rs, lib.rs}`
* Modify: `crates/mehh-sandbox/src/lib.rs`
* Modify: `crates/mehh-coding-agent/src/{agent_session.rs, tool_inspection.rs, interactive.rs}`
* Modify: `crates/mehh-cli/src/main.rs`, `crates/mehh-ai/src/anthropic.rs`
* Test: `crates/mehh-tools/src/bg_shell.rs`, `crates/mehh-tools/src/powershell.rs`

**Interfaces:**

* Consumes: `core/shell::{build_command_spec, kill_process_tree, RollingBytes}`, `mehh_sandbox::{SandboxPlan, is_secret_env_key}`
* Produces: `BackgroundShellManager`, `ShellOutputTool`, `KillShellTool`, `BashTool` `run_in_background`, `PowerShellTool.sandbox_policy`, `/shells`
* [ ] **Step 1 (RED):** Viết các test:
* `bg_shell_spawn_poll_incremental_and_kill_cleans_process` (`bg_shell.rs`)
* `bg_shell_enforces_max_concurrent_limit` (`bg_shell.rs`)
* `smart_approve_never_auto_approves_background_bash` (`tool_inspection.rs`)
* `powershell_tool_wires_sandbox_plan_and_respects_policy` (`powershell.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tools --lib bg_shell && cargo test -p mehh-tools --lib powershell`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `bg_shell.rs`; mở rộng `BashTool` thêm `run_in_background`; truyền `Some(&plan)` + `detect_violation` vào `PowerShellTool`; thêm `shutdown_background_shells()` trên `AgentSession` và gọi ở cuối `interactive.rs`/`rpc.rs`/`main.rs`; thêm `/shells` + pill `bg: N` trong footer; thêm alias `kill_shell` ↔ `KillShell` trong `anthropic.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tools --lib && cargo test -p mehh-sandbox --lib && cargo test -p mehh-coding-agent --lib tool_inspection`
Expected: PASS.

---

### Task 8 (A7): Interactive Session Switcher (`Ctrl+S`, `/sessions`, `Tab`, Confirm Delete)

**Files:**

* Modify: `crates/mehh-tui/src/keys.rs`
* Modify: `crates/mehh-coding-agent/src/{session.rs, interactive.rs}`
* Test: `crates/mehh-tui/src/keys.rs`, `crates/mehh-coding-agent/src/{session.rs, interactive.rs}`

**Interfaces:**

* Consumes: `SessionSummary`, `list_sessions`, `SelectList`, `TuiAction`
* Produces: `TuiAction::SessionPicker` (`ctrl+s`), `session::{parse_session_summary, list_all_sessions, delete_session_file}`, `SessionPickerScope`, `/sessions`, `/session info|new`
* [ ] **Step 1 (RED):** Viết các test:
* `session_picker_keybinding_defaults_to_ctrl_s` (`keys.rs`)
* `list_all_sessions_aggregates_and_delete_fails_closed_outside_root_or_symlink` (`session.rs`)
* `slash_sessions_and_session_open_picker_while_session_info_prints_stats` (`interactive.rs`)
* `resume_picker_typing_d_filters_instead_of_deleting_and_ctrl_d_confirms_delete` (`interactive.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tui --lib session_picker && cargo test -p mehh-coding-agent --lib list_all_sessions`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Thêm `TuiAction::SessionPicker`; tách `parse_session_summary` + implement `list_all_sessions` & `delete_session_file` (canonical containment + symlink check); nâng cấp `SelectorKind::Resume` (`__new__` ở vị trí 0, `Tab` đổi scope, `delete`/`ctrl+d` 2 bước xác nhận, cập nhật `format_hotkeys` và xử lý `Ctrl+S`).
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tui --lib keys && cargo test -p mehh-coding-agent --lib session::tests && cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS.

---

### Task 9 (A8): Auto-Memory (`MEMORY.md` + `MemoryTool` + Ephemeral Injection)

**Files:**

* Create: `crates/mehh-coding-agent/src/memory.rs`
* Modify: `crates/mehh-coding-agent/src/{lib.rs, settings.rs, agent_session.rs, interactive.rs}`
* Test: `crates/mehh-coding-agent/src/memory.rs` (`mod tests`), `crates/mehh-coding-agent/src/agent_session.rs` (`mod tests`)

**Interfaces:**

* Consumes: `session::session_dir_for_cwd` (lấy `cwd_hash`), `settings::write_file_atomic`, `AgentLoopConfig::prepare_request`
* Produces: `MemoryStore`, `MemoryTool`, `Settings.auto_memory`, `/memory`
* [ ] **Step 1 (RED):** Viết các test:
* `memory_store_rejects_invalid_slug_and_secret_payloads` (`memory.rs`)
* `memory_tool_save_updates_index_and_delete_removes_entry` (`memory.rs`)
* `prepare_request_injects_auto_memory_idempotently_without_persisting_jsonl` (`agent_session.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-coding-agent --lib memory`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement `memory.rs` (slug regex, secret guard, atomic index update, BOM strip, 8 KiB char-boundary cap), nối `<auto_memory>` vào `prepare_request` (`agent_session.rs`), thêm `auto_memory` vào `Settings` và `/memory` vào `interactive.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-coding-agent --lib memory && cargo test -p mehh-coding-agent --lib agent_session::tests`
Expected: PASS.

---

### Task 10 (A9): Turn/Session Checkpoint & Rewind

**Files:**

* Create: `crates/mehh-coding-agent/src/checkpoint.rs`
* Modify: `crates/mehh-tools/src/{undo_edit.rs, edit.rs, write.rs, apply_patch.rs, lib.rs}`
* Modify: `crates/mehh-coding-agent/src/{lib.rs, session.rs, agent_session.rs, interactive.rs}`
* Test: `crates/mehh-coding-agent/src/checkpoint.rs` (`mod tests`), `crates/mehh-coding-agent/src/session.rs` (`mod tests`)

**Interfaces:**

* Consumes: `sha2::Sha256`, `with_file_mutation_queue`, `EditUndoStore`, `SessionManager::branch_from`
* Produces: `FileMutationObserver`, `CheckpointStore`, `TurnCheckpointRecorder`, `SessionEntryKind::Checkpoint`, `RewindMode`, `RewindReport`, `AgentSession::rewind_to`, `/rewind`
* [ ] **Step 1 (RED):** Viết các test:
* `session_checkpoint_entry_roundtrips_and_is_omitted_from_llm_context` (`session.rs`)
* `rewind_restores_exact_bom_crlf_and_deletes_created_files` (`checkpoint.rs`)
* `rewind_fails_closed_on_externally_modified_dirty_file_without_force` (`checkpoint.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-coding-agent --lib checkpoint && cargo test -p mehh-coding-agent --lib session_checkpoint_entry`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Thêm `FileMutationObserver` vào `mehh-tools`; thêm `SessionEntryKind::Checkpoint` vào `session.rs`; implement `checkpoint.rs` (content-addressed blobs, 4 MiB/file & 256 MiB total LRU cap, dirty-hash detection); nối recorder vào `AgentSession` + `rewind_to` + `/rewind` trong `interactive.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tools --lib && cargo test -p mehh-coding-agent --lib checkpoint && cargo test -p mehh-coding-agent --lib session::tests`
Expected: PASS.

---

### Task 11 (A10): Custom Keybindings & Vim Mode

**Files:**

* Modify: `crates/mehh-tui/src/{keys.rs, components/editor.rs}`
* Modify: `crates/mehh-coding-agent/src/{settings.rs, interactive.rs}`
* Test: `crates/mehh-tui/src/{keys.rs, components/editor.rs}`, `crates/mehh-coding-agent/src/interactive.rs`

**Interfaces:**

* Consumes: `Keybindings`, `Editor`, `KillRing`, `find_word_forward`, `find_word_backward`, `CURSOR_MARKER`
* Produces: `TuiAction` serde, `Keybindings::{from_overrides, load_file, validate_and_guard}`, `EditorMode { Insert, Normal }`, `Editor.vim_enabled`, `Settings.{keybindings, vim_mode}`
* [ ] **Step 1 (RED):** Viết các test:
* `keybindings_load_file_strips_bom_rejects_unknown_action_and_guards_ctrl_c` (`keys.rs`)
* `editor_vim_mode_switches_normal_insert_motions_and_preserves_cursor_marker` (`editor.rs`)
* `editor_vim_disabled_keeps_emacs_behavior_unchanged` (`editor.rs`)
* `reload_reloads_custom_keybindings_and_vim_mode` (`interactive.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-tui --lib keybindings_load_file && cargo test -p mehh-tui --lib editor_vim_mode`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Derive serde cho `TuiAction`, thêm `ToggleThinking`/`FollowUpSubmit`, implement `load_file`/`from_overrides`/`validate_and_guard`; implement `EditorMode` + motions/operators trong `editor.rs` giữ nguyên `CURSOR_MARKER`; nối `Settings.{keybindings, vim_mode}`, `/settings`, `/reload`, `/hotkeys`, và thay hardcode `ctrl+t`/`alt+enter` bằng `keybindings.matches(...)` trong `interactive.rs`.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-tui --lib && cargo test -p mehh-coding-agent --lib interactive::tests`
Expected: PASS.

---

### Task 12 (Nhóm B — B1, B2, B3, B4, B5, B8): Context Discovery, Hierarchical Config, Multi-Format Export, Budget, Auto-Title & Clipboard Image

**Files:**

* Modify: `crates/mehh-coding-agent/src/{system_prompt.rs, trust.rs, settings.rs, skills.rs, prompt_templates.rs, todo.rs, session.rs, agent_session.rs, clipboard.rs, interactive.rs}`
* Modify: `crates/mehh-cli/src/main.rs`
* Test: unit test modules của từng file trên

**Interfaces:**

* Produces: `ContextDiscoveryOptions` + `@import` + `CLAUDE.local.md` + `.claude/rules/` (B1); `collect_trusted_ancestors` (B2); `ExportFormat { Jsonl, Markdown, Text, Html }` (B3); `BudgetConfig` + `<context_budget>` warning (B4); `derive_session_title` (B5); `read_clipboard_image_png` + `/paste-image` (B8).
* [ ] **Step 1 (RED):** Viết các test tập trung:
* `discover_context_files_nearest_wins_strips_bom_resolves_imports_and_blocks_escape` (`system_prompt.rs`)
* `hierarchical_trusted_settings_and_skills_merge_outer_to_inner_with_escalation_guard` (`settings.rs`, `skills.rs`)
* `export_session_renders_markdown_text_and_escaped_html` (`session.rs`)
* `budget_config_warns_model_at_threshold_and_stops_when_cost_exceeded` (`agent_session.rs`)
* `derive_session_title_strips_file_tags_and_truncates_utf8_safely` (`session.rs`)


* [ ] **Step 2 (Verify RED):**
Run: `cargo test -p mehh-coding-agent --lib discover_context_files_nearest_wins && cargo test -p mehh-coding-agent --lib export_session_renders && cargo test -p mehh-coding-agent --lib derive_session_title`
Expected: FAIL.
* [ ] **Step 3 (GREEN):** Implement từng hạng mục B1, B2, B3, B4, B5, B8 theo đúng spec ở Phần 2.
* [ ] **Step 4 (Verify GREEN):**
Run: `cargo test -p mehh-coding-agent --lib && cargo test -p mehh-cli --lib`
Expected: PASS.

---

# Phần 5 — Verification checklist

## 5.1 Lệnh kiểm tra bắt buộc (Debug-only, không dùng `--release`)

Thực hiện tuần tự từng crate bị tác động và cuối cùng toàn workspace ở mức debug:

```bash
# 1. Unit tests theo từng crate (nhanh, nhẹ RAM trên laptop)
cargo test -p mehh-ai --lib
cargo test -p mehh-agent --lib
cargo test -p mehh-tui --lib
cargo test -p mehh-sandbox --lib
cargo test -p mehh-tools --lib
cargo test -p mehh-coding-agent --lib
cargo test -p mehh-cli --lib

# 2. Biên dịch toàn bộ test targets (bao gồm integration tests)
cargo check -p mehh-ai --tests
cargo check -p mehh-agent --tests
cargo check -p mehh-tui --tests
cargo check -p mehh-sandbox --tests
cargo check -p mehh-tools --tests
cargo check -p mehh-coding-agent --tests
cargo check -p mehh-cli --tests

# 3. Clippy ở mức debug (0 warnings)
cargo clippy -p mehh-ai --tests -- -D warnings
cargo clippy -p mehh-agent --tests -- -D warnings
cargo clippy -p mehh-tui --tests -- -D warnings
cargo clippy -p mehh-sandbox --tests -- -D warnings
cargo clippy -p mehh-tools --tests -- -D warnings
cargo clippy -p mehh-coding-agent --tests -- -D warnings
cargo clippy -p mehh-cli --tests -- -D warnings

```

## 5.2 Tiêu chí Pass

* Tất cả các lệnh `cargo test`, `cargo check`, `cargo clippy` ở mục 5.1 thoát với mã `0`, không có test fail, không có warning.
* Mọi tính năng mới (A1–A10, B1–B8) đều có ít nhất 1 test fail-closed / edge-case đi qua nhánh lỗi (JSON corrupt, untrusted workspace, SSRF, dirty rewind, missing LSP binary, unknown keybinding action).
* TUI render sạch không tràn cột tại viewport `60x20` và `160x45` cho mọi modal mới (`PlanApproval`, `HunkApproval`, `Resume` mới, Prompt dock Vim `[NORMAL]`/`[INSERT]`).
* Mọi file tạo/sửa đều là UTF-8 **không BOM**.

## 5.3 Các Grep "không được có" & Kiểm tra ranh giới Git

```bash
# 1. Không có mã màu SGR hardcode trong render path (ngoài Theme và test fixtures)
git grep -n '\\x1b\[3[0-9]m\|\\x1b\[9[0-9]m\|\\x1b\[1;3[0-9]m' -- crates/mehh-tui/src/components crates/mehh-coding-agent/src/interactive.rs

# 2. Không có renderer.reset() thừa hoặc full-screen clear ngoài nhánh Event::Resize
git grep -n 'renderer\.reset()\|\\x1b\[H\|\\x1b\[J' -- crates/mehh-coding-agent/src/interactive.rs

# 3. Không hardcode phím mới rời rạc trong interactive.rs (ctrl+t, ctrl+s, backtab, alt+enter)
git grep -n 'key_str == "ctrl+t"\|key_str == "ctrl+s"\|key_str == "backtab"\|key_str == "alt+enter"' -- crates/mehh-coding-agent/src/interactive.rs

# 4. Xác nhận CURSOR_MARKER còn nguyên vẹn trong editor.rs
git grep -n 'CURSOR_MARKER' -- crates/mehh-tui/src/components/editor.rs

# 5. Xác nhận không có UTF-8 BOM trong source và docs vừa sửa (PowerShell/Python one-liner hoặc git check)
# Kỳ vọng: 0 file có byte đầu EF BB BF

# 6. Xác nhận không đụng vùng cấm interactive.rs:3316-3352 và không có commit ngoài ý muốn
git diff -U0 -- crates/mehh-coding-agent/src/interactive.rs
git status --short
git log -n 1 --oneline

```

**Kỳ vọng kết quả grep**:

* Grep #1 và #3: **0 kết quả** trong code production.
* Grep #2: chỉ xuất hiện duy nhất 1 `renderer.reset()` trong nhánh `Event::Resize`.
* Grep #4: `CURSOR_MARKER` có mặt ở cả nhánh `is_empty()` và nhánh `cursor_row` của `Editor::render`.
* Kiểm tra #6: `git log` giữ nguyên HEAD (không có commit tự ý), vùng `interactive.rs:3316-3352` không có thay đổi nào ngoài phần uncommitted trước đó.

Bạn muốn chốt trước các câu hỏi mở ở **Phần 4.2** (đặc biệt là provider cho A3, `lsp-types` hay 0-dep cho A4, và vị trí lưu checkpoint A9) để tôi khóa bản thiết kế cuối trước khi bước sang giai đoạn implement không?
