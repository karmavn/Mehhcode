# SDD ledger — plan: docs/superpowers/plans/2026-11-16-goose-parity-core-plan.md

Pre-flight scan:
- Task 1 produces `McpToolAnnotations`, `AgentTool::is_read_only(&self) -> Option<bool>`, `AgentTool::is_safe_for_auto_approve`.
- Task 2 consumes Task 1's `is_read_only`, produces `ToolInspectionManager`, `PermissionMode`, `InspectionVerdict`.
- Task 3 consumes Task 2's `ToolInspectionManager` and binds it to `AgentSession.prepare_prompt_content` (`before_tool_call`) and `CodemodeTool`.
- Task 4 consumes Task 2's `PermissionMode` and Task 3's `AgentSession` hooks to build TUI modal and rich tool card rendering.
- Task 5 produces `AnalyzeTool` (`is_read_only() -> Some(true)`).
- Task 6 produces `EditUndoStore` and `UndoEditTool`, consuming `file_mutation_queue`.
- Task 7 produces `TodoTool` and ephemeral MOIM injection in `prepare_request`.
- Task 8 produces BM25 Okapi in `tool_search.rs`.
- Task 9 consumes Task 2's `ToolInspectionManager` in `SubagentTool` child loop.
- Task 10 produces `LeadWorkerManager` and cross-provider swapping in `AgentLoopTurnUpdate`.
Pre-flight: no unresolvable conflicts found.

Task 1: complete (tests: cargo test -p mehh-mcp --test test_annotations -> 2 passed, cargo check --workspace -> 0 errors)
Task 2: complete (tests: cargo test -p mehh-coding-agent --test test_tool_inspection -> 4 passed, cargo check --workspace -> 0 errors)
Task 3: complete (tests: cargo test -p mehh-coding-agent --test test_agent_session_governance -> 3 passed, cargo check --workspace -> 0 errors)
Task 4: complete (tests: cargo test -p mehh-coding-agent --test test_interactive_approvals -> 1 passed, cargo test -p mehh-coding-agent -> 107 passed, cargo test -p mehh-cli -> 47 passed, cargo clippy -p mehh-coding-agent -> 0 warnings)
Task 5: complete (tests: cargo test -p mehh-tools --test test_analyze -> 3 passed, cargo test -p mehh-tools -> 27 passed, cargo check --workspace -> 0 errors)
Task 6: complete (tests: cargo test -p mehh-tools --test test_undo_edit -> 1 passed, cargo test -p mehh-tools -> 28 passed, cargo clippy -p mehh-tools -> 0 warnings, cargo check --workspace -> 0 errors)
Task 7: complete (tests: cargo test -p mehh-coding-agent --test test_todo_moim -> 2 passed, cargo test -p mehh-coding-agent -> 117 passed, cargo clippy -p mehh-coding-agent -> 0 warnings, cargo check --workspace -> 0 errors)
Task 8: complete (tests: cargo test -p mehh-coding-agent --test test_bm25_tool_search -> 1 passed, cargo test -p mehh-coding-agent -> 118 passed, cargo clippy -p mehh-coding-agent -> 0 warnings, cargo check --workspace -> 0 errors)
Task 9: complete (tests: cargo test -p mehh-coding-agent --test test_subagent_multimode -> 2 passed, cargo clippy -p mehh-coding-agent -> 0 warnings, cargo check --workspace -> 0 errors)
Task 10: complete (tests: cargo test -p mehh-coding-agent --test test_lead_worker -> 2 passed, cargo test --workspace -> all passed, cargo clippy --workspace --all-targets -- -D warnings -> 0 warnings)
