# Mehhcode

Mehhcode (`mehh`) is a Rust terminal coding agent CLI for software engineers, combining multi-provider LLM streaming, built-in codebase tools, MCP integration, and fail-closed execution governance.

## Core Capabilities

- **Multiple execution modes:** Run the interactive terminal UI (`mehh`), one-shot print mode (`mehh -p "..."` or piped stdin), structured JSON output (`--mode json`), or JSON-RPC (`--mode rpc`) for editor and automation integrations.
- **15 built-in LLM providers and OAuth:** Stream via Anthropic, OpenAI (Chat & Responses), Google Gemini / Vertex, Amazon Bedrock, Azure OpenAI, Mistral, OpenRouter, Groq, DeepSeek, xAI, Cerebras, Together, Fireworks, and GitHub Copilot, with custom providers/models in `~/.mehh/agent/models.json` and OAuth login via `mehh --login <provider>`.
- **Built-in coding and web tools with undo:** Inspect workspaces, browse web sources, and edit files with `read`, `write`, `edit`, `undo_edit`, `bash`, `grep`, `find`, `ls`, `analyze`, `web_search`, `web_fetch`, `todo`, `subagent`, and opt-in `powershell` (selectable via `--tools` / `--exclude-tools`, with `web_search` and `web_fetch` configured via `settings.json`).
- **Fail-closed governance and sandboxing:** Enforce `smart_approve` (default), `approve`, `auto`, and `chat` permission modes, project trust gating for workspace-local configs, shell-aware prefix rules (`mehh-execpolicy`), secret environment variable scrubbing, and OS sandboxing (`bwrap` on Linux, Seatbelt `sandbox-exec` on macOS).
- **MCP, subagents, and QuickJS codemode:** Connect local stdio and streamable HTTP MCP servers (`mcp.json`), delegate tasks to subagents or lead/worker model splits (`--worker-model`, `--lead-turns`), and batch tool calls inside an embedded QuickJS runtime (`--codemode`).
- **Branching sessions and project context:** Persist JSONL v3 sessions with `-c`/`--continue`, `--resume`, `--fork`, `/tree`, `/compact`, and `--export`, while automatically discovering `AGENTS.md` / `CLAUDE.md`, `SKILL.md` skills, and prompt templates.

## Prerequisites & Requirements

| Requirement | Details | Notes |
| --- | --- | --- |
| Rust toolchain | Stable `rustc` and `cargo` (Edition 2021) | Required to build and install from source |
| C compiler | `cc` / Clang (Linux/macOS) or MSVC Build Tools (Windows) | Required by `rquickjs` (`mehh-codemode`) |
| Operating system | Linux, macOS, or Windows | Linux uses `bwrap` and macOS uses `sandbox-exec` for shell sandboxing; Windows scrubs secret env vars and runs with degraded OS sandboxing |
| Provider credentials | API key env var (e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`) or OAuth (`mehh --login <provider>`) | Not required for `--list-models` or `--provider faux` |
| `bwrap` (optional, Linux) | Bubblewrap binary in `PATH` | Enables user/mount/network namespace isolation for shell commands |

## Installation & Getting Started

Clone the repository and install the `mehh` binary from source:

```bash
git clone https://github.com/karmavn/Mehhcode.git
cd Mehhcode
cargo install --path crates/mehh-cli
```

Verify the installation offline:

```bash
mehh --version
mehh --list-models
```

Configure provider credentials via environment variables or stored credentials:

```bash
export ANTHROPIC_API_KEY="your-api-key"
# Or log in via OAuth / save an API key to ~/.mehh/agent/auth.json:
mehh --login anthropic
```

Common usage examples:

```bash
# Interactive TUI
mehh

# One-shot prompt with a specific provider/model
mehh -m openai/gpt-4.1 -p "Summarize crates/mehh-cli/src/main.rs"

# Pipe stdin together with a prompt
git diff | mehh -p "Review these changes"

# Continue the latest session or pick a saved session
mehh -c
mehh --resume

# Enable QuickJS codemode or choose a permission mode
mehh --codemode --permission-mode smart_approve

# Run as a JSON-RPC server over stdin/stdout
mehh --mode rpc
```

## Configuration

Global configuration files live under `~/.mehh/agent/`. Trusted workspaces may also load project-local files under `<project>/.mehh/` (project-local settings can only tighten permissions, never loosen them).

| File | Scope | Purpose |
| --- | --- | --- |
| `settings.json` | Global & project | Default provider/model, thinking level, theme, permission mode, compaction, and lead/worker settings |
| `auth.json` | Global only | Stored API keys and OAuth tokens (written with restricted file permissions) |
| `models.json` | Global only | Custom provider base URLs, headers, and model definitions |
| `mcp.json` | Global & project | Stdio and streamable HTTP MCP server definitions and overrides |
| `permissions.json` | Global & project | Persisted `always_allow`, `ask_before`, and `never_allow` tool rules |

## Workspace Architecture

| Crate | Responsibility |
| --- | --- |
| `mehh-cli` | `mehh` CLI binary entrypoint and argument handling |
| `mehh-coding-agent` | Session lifecycle (JSONL v3), TUI orchestration, RPC/print modes, trust, permissions, subagents, skills, compaction |
| `mehh-agent` | Core agent turn loop, tool dispatch, steering/follow-up queues, cancellation |
| `mehh-ai` | Multi-provider LLM streaming adapters, built-in model catalog, OAuth, retry/overflow handling |
| `mehh-tools` | Built-in filesystem, search, web, analysis, undo, and shell tools |
| `mehh-tui` | Terminal UI widgets, markdown renderer, editor, themes, and input handling (`crossterm`) |
| `mehh-mcp` | MCP client over stdio and streamable HTTP, OAuth, and resource meta-tools |
| `mehh-codemode` | Embedded QuickJS (`rquickjs`) sandbox for programmatic tool orchestration |
| `mehh-execpolicy` | Shell command tokenizer and fail-closed prefix permission evaluator |
| `mehh-sandbox` | OS-level sandbox planning (`bwrap` / Seatbelt) and secret env var scrubbing |

### Development

```bash
cargo check
cargo clippy
cargo test -p mehh-cli
```

## License

Licensed under the [MIT License](LICENSE).
