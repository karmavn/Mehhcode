# Mehhcode Feature Map

Quick 1-1 lookup from user-facing features, commands, tools, and guardrails to owning source files and targeted verification commands. Verified automatically by `crates/mehh-cli/tests/workspace_guardrails.rs`.

## 1. Execution Modes & CLI Entrypoint

| Feature | Primary Source Files | Verification |
| --- | --- | --- |
| CLI flags, startup & mode routing | `crates/mehh-cli/src/lib.rs`, `crates/mehh-cli/src/bootstrap.rs`, `crates/mehh-cli/src/main.rs` | `cargo test -p mehh-cli --test e2e_cli_rpc` |
| Interactive TUI mode | `crates/mehh-coding-agent/src/interactive/mod.rs`, `crates/mehh-coding-agent/src/interactive/events/mod.rs`, `crates/mehh-coding-agent/src/interactive/events/signals.rs`, `crates/mehh-coding-agent/src/interactive/events/tasks.rs`, `crates/mehh-coding-agent/src/interactive/events/input.rs`, `crates/mehh-coding-agent/src/interactive/render.rs`, `crates/mehh-coding-agent/src/interactive/state.rs` | `cargo test -p mehh-coding-agent --test test_tui_headless_e2e` |
| Print modes (`text` / `json`) | `crates/mehh-coding-agent/src/modes.rs` | `cargo test -p mehh-cli --test e2e_cli_rpc` |
| Headless JSON-RPC mode (`--mode rpc`) | `crates/mehh-coding-agent/src/rpc/mod.rs`, `crates/mehh-coding-agent/src/rpc/protocol.rs` | `cargo test -p mehh-cli --test e2e_cli_rpc` |
| Offline scripted faux provider | `crates/mehh-ai/src/faux.rs`, `crates/mehh-cli/src/lib.rs`, `crates/mehh-cli/src/bootstrap.rs` | `cargo test -p mehh-cli parse_faux_script` |
| Binary & headless TUI E2E suites | `crates/mehh-cli/tests/e2e_cli_rpc.rs`, `crates/mehh-coding-agent/tests/test_tui_headless_e2e.rs` | `cargo test -p mehh-cli --test e2e_cli_rpc; cargo test -p mehh-coding-agent --test test_tui_headless_e2e` |

## 2. Interactive Slash Commands & Keybindings

All 30 built-in slash commands are registered and dispatched in `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, with interactive state/tests in `crates/mehh-coding-agent/src/interactive/state.rs` and `crates/mehh-coding-agent/src/interactive/tests.rs`.

| Group | Commands / Features | Primary Source Files |
| --- | --- | --- |
| Provider, Model & Thinking | `/connect`, `/login`, `/logout`, `/model`, `/scoped-models`, `/thinking` | `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, `crates/mehh-coding-agent/src/interactive/commands/auth.rs`, `crates/mehh-coding-agent/src/auth.rs`, `crates/mehh-coding-agent/src/models_config.rs` |
| Settings, Theme & Mode | `/settings`, `/theme`, `/mode`, `/reload` | `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, `crates/mehh-coding-agent/src/interactive/commands/settings_ui.rs`, `crates/mehh-coding-agent/src/settings.rs`, `crates/mehh-tui/src/theme.rs` |
| Sessions & Branching | `/new`, `/sessions`, `/resume`, `/compact`, `/tree`, `/fork`, `/clone`, `/name`, `/session`, `/export`, `/import` | `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, `crates/mehh-coding-agent/src/interactive/commands/autocomplete.rs`, `crates/mehh-coding-agent/src/session/mod.rs`, `crates/mehh-coding-agent/src/session/types.rs`, `crates/mehh-coding-agent/src/session/storage.rs`, `crates/mehh-coding-agent/src/session/projection.rs`, `crates/mehh-coding-agent/src/compaction.rs`, `crates/mehh-coding-agent/src/branch_summarization.rs` |
| Tools, MCP & Turn Queue | `/tools`, `/mcp`, `/follow-up` | `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, `crates/mehh-coding-agent/src/mcp_manager.rs`, `crates/mehh-agent/src/agent.rs` |
| Help, Clipboard & Exit | `/help`, `/hotkeys`, `/copy`, `/clear`, `/exit`, `/quit` | `crates/mehh-coding-agent/src/interactive/commands/mod.rs`, `crates/mehh-coding-agent/src/clipboard.rs` |
| Keybindings (`TuiAction` / `Keybindings`) | Global & editor key normalization and bindings | `crates/mehh-tui/src/keys.rs`, `crates/mehh-coding-agent/src/interactive/events/input.rs` |
| TUI Renderer & Components | `TuiRenderer`, `MockTerminal`, `Editor`, `PasteBurst`, `MarkdownView`, `SettingsList`, ANSI width | `crates/mehh-tui/src/tui.rs`, `crates/mehh-tui/src/paste_burst.rs`, `crates/mehh-tui/src/ansi.rs`, `crates/mehh-tui/src/components/editor.rs`, `crates/mehh-tui/src/components/markdown.rs`, `crates/mehh-tui/src/components/settings_list.rs`, `crates/mehh-tui/src/components/basic.rs`, `crates/mehh-tui/src/wheel_scroll.rs` |

## 3. Built-in Tools

| Tool(s) | Primary Source Files | Verification |
| --- | --- | --- |
| Tool selection & web config | `crates/mehh-tools/src/lib.rs`, `crates/mehh-tools/src/core/context.rs`, `crates/mehh-tools/src/core/fs.rs`, `crates/mehh-tools/src/core/output.rs` | `cargo test -p mehh-tools --test test_regressions` |
| File read, write, edit & undo | `crates/mehh-tools/src/read.rs`, `crates/mehh-tools/src/write.rs`, `crates/mehh-tools/src/edit.rs`, `crates/mehh-tools/src/undo_edit.rs`, `crates/mehh-tools/src/file_mutation_queue.rs` | `cargo test -p mehh-tools --test test_undo_edit` |
| Search & symbol analysis (`grep`, `find`, `ls`, `analyze`) | `crates/mehh-tools/src/grep.rs`, `crates/mehh-tools/src/find.rs`, `crates/mehh-tools/src/ls.rs`, `crates/mehh-tools/src/analyze.rs` | `cargo test -p mehh-tools --test test_analyze` |
| Shell tools (`bash`, `powershell`) | `crates/mehh-tools/src/bash.rs`, `crates/mehh-tools/src/powershell.rs`, `crates/mehh-tools/src/core/shell.rs` | `cargo test -p mehh-tools bash` |
| Web tools (`web_search`, `web_fetch`) | `crates/mehh-tools/src/web/search_tool.rs`, `crates/mehh-tools/src/web/fetch_tool.rs`, `crates/mehh-tools/src/web/router.rs`, `crates/mehh-tools/src/web/providers.rs`, `crates/mehh-tools/src/web/native/mod.rs`, `crates/mehh-tools/src/web/native/sources.rs`, `crates/mehh-tools/src/web/native/html_parse.rs`, `crates/mehh-tools/src/web/native/rank.rs`, `crates/mehh-tools/src/web/native/enrich.rs` | `cargo test -p mehh-tools web` |
| Session tools (`todo`, `subagent`) | `crates/mehh-coding-agent/src/todo.rs`, `crates/mehh-coding-agent/src/subagent/mod.rs`, `crates/mehh-coding-agent/src/subagent/runner.rs`, `crates/mehh-coding-agent/src/subagent/scratchpad.rs`, `crates/mehh-coding-agent/src/subagent_control.rs`, `crates/mehh-coding-agent/src/subagent_lifecycle.rs` | `cargo test -p mehh-coding-agent --test test_todo_moim` |

## 4. Governance, Permissions, Trust & Sandboxing

| Feature | Primary Source Files | Verification |
| --- | --- | --- |
| Permission rules & approval gate | `crates/mehh-coding-agent/src/permissions.rs`, `crates/mehh-coding-agent/src/tool_inspection.rs` | `cargo test -p mehh-coding-agent --test test_permissions` |
| Project trust gating | `crates/mehh-coding-agent/src/trust.rs` | `cargo test -p mehh-coding-agent --test test_approval_gate` |
| Shell command prefix policy | `crates/mehh-execpolicy/src/lib.rs` | `cargo test -p mehh-execpolicy` |
| OS sandbox & secret scrubbing | `crates/mehh-sandbox/src/lib.rs` | `cargo test -p mehh-sandbox` |

## 5. Sessions, Providers, MCP, Codemode & Agent Core

| Feature | Primary Source Files | Verification |
| --- | --- | --- |
| Session orchestrator & turn loop | `crates/mehh-coding-agent/src/agent_session/mod.rs`, `crates/mehh-coding-agent/src/agent_session/tools.rs`, `crates/mehh-coding-agent/src/agent_session/prompt.rs`, `crates/mehh-coding-agent/src/agent_session/compaction.rs`, `crates/mehh-agent/src/agent_loop.rs`, `crates/mehh-agent/src/types.rs` | `cargo test -p mehh-agent` |
| Lead/worker model routing | `crates/mehh-coding-agent/src/lead_worker.rs` | `cargo test -p mehh-coding-agent --test test_lead_worker` |
| System prompt, `@file` args, skills, prompt templates | `crates/mehh-coding-agent/src/system_prompt.rs`, `crates/mehh-coding-agent/src/file_args.rs`, `crates/mehh-coding-agent/src/file_complete.rs`, `crates/mehh-coding-agent/src/skills.rs`, `crates/mehh-coding-agent/src/prompt_templates.rs`, `crates/mehh-coding-agent/src/tool_search.rs` | `cargo test -p mehh-coding-agent --test test_bm25_tool_search` |
| Built-in LLM providers, OAuth, retry & overflow | `crates/mehh-ai/src/catalog.rs`, `crates/mehh-ai/src/anthropic.rs`, `crates/mehh-ai/src/openai.rs`, `crates/mehh-ai/src/openai_responses.rs`, `crates/mehh-ai/src/google.rs`, `crates/mehh-ai/src/bedrock.rs`, `crates/mehh-ai/src/azure_openai.rs`, `crates/mehh-ai/src/mistral.rs`, `crates/mehh-ai/src/oauth.rs`, `crates/mehh-ai/src/retry.rs`, `crates/mehh-ai/src/overflow.rs` | `cargo test -p mehh-ai` |
| MCP client, stdio/HTTP transport & tool adapter | `crates/mehh-coding-agent/src/mcp_manager.rs`, `crates/mehh-mcp/src/client.rs`, `crates/mehh-mcp/src/transport.rs`, `crates/mehh-mcp/src/tool_adapter.rs`, `crates/mehh-mcp/src/config.rs`, `crates/mehh-mcp/src/oauth.rs` | `cargo test -p mehh-mcp` |
| QuickJS embedded codemode | `crates/mehh-codemode/src/tool.rs`, `crates/mehh-codemode/src/sandbox.rs` | `cargo test -p mehh-codemode` |

## 6. Guardrails & CI

| Guardrail | Primary Source Files | Verification |
| --- | --- | --- |
| Workspace lints & test exemptions | `Cargo.toml`, `clippy.toml`, `.github/workflows/ci.yml` | `cargo clippy -p mehh-cli --no-deps --all-targets -- -D warnings` |
| Dependency/license policy | `deny.toml`, `.github/workflows/ci.yml` | Checked in CI (`cargo-deny-action`) |
| Crate layer, lint, BOM, `AGENTS.md`, `FEATURE_MAP.md` | `crates/mehh-cli/tests/workspace_guardrails.rs` | `cargo test -p mehh-cli --test workspace_guardrails` |
