# Prompt cho Gemini (bản web) — Thiết kế các tính năng còn thiếu của MEHH Code

> Dán nội dung file này vào Gemini trước, sau đó đính kèm lần lượt các file dump `01_` … `11_` trong thư mục này. Dùng chế độ reasoning/suzy nghĩ dài nếu có. Nếu Gemini báo đầy context, hãy đưa từng group một, yêu cầu nó cập nhật "architecture map" trước khi chuyển group.

---

## 1. Vai trò và mục tiêu

Bạn là kiến trúc sư Rust senior chuyên về coding agent CLI/TUI. Nhiệm vụ của bạn là **THIẾT KẾ**, không phải viết code implementation, cho các tính năng mà repository MEHH Code (`D:\Mehhcode`) còn thiếu so với một coding agent hiện đại (Claude Code, OpenCode, Goose).

Đầu ra mong đợi là hai văn bản theo đúng phong cách có sẵn của repo:

1. `docs/superpowers/specs/2026-11-22-coding-agent-gap-design.md`
2. `docs/superpowers/plans/2026-11-22-coding-agent-gap-plan.md`

Không commit, không sửa source. Chỉ trả về thiết kế + kế hoạch dạng Markdown, có thể copy vào repo.

---

## 2. Bối cảnh repository (đã được xác thực bằng code)

- Rust workspace `edition = "2021"`, `resolver = "2"`, 10 crate: `mehh-ai`, `mehh-agent`, `mehh-tools`, `mehh-coding-agent`, `mehh-tui`, `mehh-mcp`, `mehh-codemode`, `mehh-execpolicy`, `mehh-sandbox`, `mehh-cli`.
- Dependency workspace hiện có: `tokio`, `tokio-util`, `futures`, `async-trait`, `reqwest` (rustls, stream, json), `serde`, `serde_json`, `thiserror`, `anyhow`, `uuid`, `base64`, `sha2`, `regex`, `globset`, `ignore`, `similar`, `clap`, `crossterm`, `unicode-width`, `unicode-segmentation`, `tracing`. **Không có** tree-sitter, axum/warp, tauri, LSP client, OpenTelemetry.
- Binary native single-file, chạy 4 chế độ: `text | json | rpc | interactive`, TUI dùng crossterm 0.28, session lưu JSONL append-only theo cwd, có compaction/branch summary, subagent 3 mode, MCP stdio/HTTP, skills, prompt templates, 4 permission mode + inspector 3 tầng + exec-policy + sandbox OS/QuickJS.
- Đang có uncommitted work ở đợt TUI UX optimization; **không được đụng vào** permission-rule block tại `crates/mehh-coding-agent/src/interactive.rs:3316-3352` (theo ledger) và không được commit.
- Toàn bộ source được dump **nguyên si** ở các file `01_`…`11_`, kèm bảng SHA-256 ở đầu mỗi file. Thư mục `raw/` chứa bản copy byte-for-byte của từng file gốc. **Khi trích dẫn code, phải giữ nguyên từng byte, không viết lại, không rút gọn, không chèn `...` vào giữa code.** Nếu cần cắt, hãy cắt theo ranh giới hàm/struct và nói rõ là trích đoạn.

---

## 3. Các tính năng còn thiếu cần thiết kế

### Nhóm A — Ưu tiên cao

| # | Tính năng | Thực tế hiện tại | Bằng chứng |
|---|---|---|---|
| A1 | **Plan mode** | Không có `EnterPlanMode`/`ExitPlanMode`; các tên này chỉ nằm trong bảng常量 đổi casing của Claude Code. Gần nhất là tool `todo` + `<working_checklist>` inject mỗi turn, nhưng không có chế độ "chỉ lập kế hoạch, không sửa file". | `crates/mehh-ai/src/anthropic.rs:217-236`; `crates/mehh-coding-agent/src/tool_inspection.rs:26-49`; `crates/mehh-coding-agent/src/todo.rs:176-182`; `crates/mehh-coding-agent/src/agent_session.rs:907-954` |
| A2 | **Hooks do người dùng cấu hình** | Chỉ có 4 hook nội bộ (`before_tool_call`, `after_tool_call`, `prepare_request`, `prepare_next_turn`). Không có `hooks.json`, không có `PreToolUse/PostToolUse/SessionStart/Stop/Notification`. | `crates/mehh-agent/src/agent_loop.rs:159-273`; `crates/mehh-coding-agent/src/agent_session.rs:868-905,911-1041`; `crates/mehh-coding-agent/src/settings.rs:90-122` |
| A3 | **Web search / WebFetch tool** | Không có tool; hai cái tên chỉ là hằng số. Security inspector chặn `curl \| sh`, codemode chặn remote image URL. Cần thiết kế tool có network policy riêng, fail-closed, không bypass inspector. | `crates/mehh-ai/src/anthropic.rs:234-235`; `crates/mehh-coding-agent/src/tool_inspection.rs:74-95,168-210`; `crates/mehh-codemode/src/sandbox.rs:317-321` |
| A4 | **LSP / code intelligence thật** | `analyze` là regex tự viết cho Rust/TS/Python/Go; không go-to-definition, find-references, rename type-aware, diagnostics. Cần đánh giá: thêm LSP client hay mở rộng `analyze`? Không được thêm dependency C nặng nếu không có lý do chính đáng. | `crates/mehh-tools/src/analyze.rs:20-40,202-321`; `Cargo.toml:21-42` |
| A5 | **`apply_patch` + accept/reject từng hunk** | Diff chỉ render read-only; approval áp cho cả tool call (`y/a/A/n`). `apply_replacements_to_original` chỉ là helper nội bộ. | `crates/mehh-tools/src/edit.rs:69-150`; `crates/mehh-coding-agent/src/interactive.rs:2712-2739` |
| A6 | **Background shell / quản lý process** | `run_shell` luôn chờ process, có timeout + kill tree sau grace 500ms; không có start/list/kill shell; `KillShell` chỉ là hằng số. | `crates/mehh-tools/src/core/shell.rs:204-223,268-326`; `crates/mehh-ai/src/anthropic.rs:228` |
| A7 | **Session switcher tương tác** | Đã có spec+plan từ 17/11 nhưng **chưa implement**; không `Ctrl+S`, không `/sessions`, `SelectorKind` chỉ 6 loại, chỉ có `/resume`. | `crates/mehh-coding-agent/src/interactive.rs:72-81,1622-1643`; `crates/mehh-tui/src/keys.rs:146-175`; `docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md:13-17,31-40` |
| A8 | **Auto-memory** | `MOIM.md` chỉ đọc; không có tool ghi/cập nhật; không có `MEMORY.md` tự học. | `crates/mehh-coding-agent/src/todo.rs:103-122` |
| A9 | **Checkpoint/rewind gắn turn/session** | `undo_edit` là snapshot in-memory (20/file, 16 MB tổng), không persist, không gắn session. `branch_from` chỉ đổi con trỏ hội thoại, không khôi phục file. | `crates/mehh-tools/src/undo_edit.rs:14-18,58-141`; `crates/mehh-coding-agent/src/session.rs:840-863` |
| A10 | **Keybindings tuỳ biến / vim mode** | Binding hardcode; `Settings` không có field keybindings; không có trạng thái normal/insert. | `crates/mehh-tui/src/keys.rs:189-274,288-290`; `crates/mehh-coding-agent/src/settings.rs:93-122` |

### Nhóm B — Có nhưng chưa đầy đủ

- `AGENTS.md`/`CLAUDE.md`: walk-up có, nhưng inject **tất cả** file root→cwd thay vì nearest-wins; không `@import`, `CLAUDE.local.md`, `.claude/rules/`; không strip BOM — `crates/mehh-coding-agent/src/system_prompt.rs:27-50,106-124`.
- Config/skills/templates/MOIM chỉ theo **cwd gốc**, không phân cấp thư mục con — `settings.rs:142-145`, `skills.rs:145-154`, `prompt_templates.rs:265-272`.
- Export chỉ JSONL; chưa có Markdown/HTML/text — `session.rs:526-550`.
- Token/cost chỉ để hiển thị; không có limit/ngân sách/cảnh báo. Context bar có nhưng không đưa hướng dẫn bằng chữ cho model — `interactive.rs:2390-2428,2423-2434`.
- Session không tự đặt title; `list_sessions` lấy dòng prompt đầu — `session.rs:439-466`.
- `is_safe_for_auto_approve` định nghĩa nhưng **không ai gọi**; governance dùng `is_read_only` — `crates/mehh-agent/src/types.rs:73-75`, `todo.rs:235-244`.
- `PowerShellTool` **không truyền sandbox plan**; sandbox Windows chỉ degraded (scrub credential), không có kernel isolation — `crates/mehh-tools/src/powershell.rs:90-100`; `crates/mehh-sandbox/src/lib.rs:366-374`.
- Ảnh chỉ vào qua `read` file local; không screenshot/computer use, không graphics protocol; voice không có — `read.rs:130-157`; `crates/mehh-codemode/src/sandbox.rs:317-321`.

### Nhóm C — Đã cố tình hoãn (non-goal, chỉ thiết kế nếu tôi yêu cầu riêng)

- `mehh serve` (HTTP/WebSocket daemon), IDE **ACP server**, Goose **Recipe engine**, **ChatRecall** tìm kiếm跨 session, plugin/extension host — `docs/superpowers/specs/2026-11-16-goose-parity-core-design.md:42`, `2026-11-13-strongest-rust-agent-design.md:19`.
- Web UI, telemetry/OpenTelemetry, `TuiAltScreen` (mouse drag select, alt-screen search), codemode `.d.ts` generator + VM replay — `docs/superpowers/specs/2026-11-15-ts-parity-waves-4-5-6-design.md:20-23`.
- NotebookEdit/KillShell/screenshot: chỉ là tên trong bảng常量 Claude Code — `anthropic.rs:218-236`.

---

## 4. Ràng buộc thiết kế (bắt buộc)

1. **Không viết implementation**, chỉ thiết kế + kế hoạch. Không commit, không sửa file source, không tạo worktree.
2. Mọi thiết kế phải bám vào API và cấu trúc có thật trong code dump. Không bịa module/type/field không tồn tại; nếu đề xuất cái mới, phải nói rõ là mới và chỉ ra nơi tích hợp.
3. **Fail-closed** cho mọi đường dẫn liên quan đến bảo mật, permission, path, trust, network. Không im lặng fallback về mặc định an toàn giả tạo. Phân biệt `ENOENT` với JSON corrupt; strip BOM khi đọc config.
4. Không thêm dependency nặng (`tree-sitter`, `axum`, `tauri`, LSP crate lớn, OpenTelemetry…) nếu chưa phân tích trade-off. Ưu tiên dependency có sẵn. Nếu bắt buộc, nêu rõ lý do, dung lượng binary và rủi ro build trên Windows.
5. HTTP/network tool (A3) phải đi qua inspection pipeline, có allowlist domain, timeout, size cap, redact secret, log có kiểm soát; không được đọc `auth.json`/`.env`/`~/.ssh`.
6. Mọi màu trong TUI phải từ `Theme`; mọi tính toán độ rộng phải qua `mehh_tui::visible_width` / `wrap_text_with_ansi` / `truncate_to_width` / `slice_by_column`; phải bảo tồn `CURSOR_MARKER = "\x1b_mehh:c\x07"` trong mọi path render editor.
7. Mọi field mới trên struct serialize phải `#[serde(default)]`; schema JSONL v3 phải backward-compatible; keybinding mới phải đăng ký trong `TuiAction`/`Keybindings` (`crates/mehh-tui/src/keys.rs`), không hardcode key check rời rạc.
8. Chạy và báo cáo lệnh kiểm tra ở mức debug: `cargo test -p <crate> --lib`, `cargo check -p <crate> --tests`, `cargo clippy -p <crate> --tests`. Không dùng release build trên laptop này.
9. Source và tài liệu phải UTF-8 **không BOM**.
10. Tuân thủ toàn bộ `AGENTS.md` của repo (đính kèm nguyên si ở cuối prompt này và trong `raw/AGENTS.md`).

---

## 5. Cấu trúc output bắt buộc

Trả về đúng 5 phần theo thứ tự:

### Phần 1 — Architecture map
- Bảng "hiện tại → đề xuất" với đường dẫn file cụ thể.
- Sơ đồ luồng dữ liệu cho mỗi tính năng (ai gọi ai, hook nào, tool nào, state ở đâu).
- Chỉ ra điểm tích hợp tối thiểu (minimal diff surface).

### Phần 2 — Design spec từng tính năng (A1→A10, rồi B)
Với mỗi tính năng:
- **Problem**: 1 đoạn, kèm ví dụ cụ thể/trace ngắn.
- **Solution**: cấu trúc dữ liệu, signature hàm/tool, state transitions, file cần sửa. Nếu là hook/tool mới, mô tả schema JSON và ví dụ cấu hình.
- **Why**: tại sao cần giải pháp này, tại sao không dùng cách đơn giản hơn, và cái nào là complexity không cần thiết.
- **Security/compat**: fail-closed, trust, serde default, JSONL migration, cross-platform (Windows/macOS/Linux).

### Phần 3 — Implementation plan kiểu superpowers
- Các task đánh số, mỗi task có checkbox `- [ ]`, **Files** (Modify/Test), **Interfaces** (Consumes/Produces), các bước TDD: viết test failing → chạy thấy đỏ → implement tối thiểu → chạy thấy xanh.
- Tên test cụ thể, lệnh chạy cụ thể, expected result.
- Global constraints + review focus giống `docs/superpowers/plans/2026-11-21-tui-ux-optimization.md`.

### Phần 4 — Rủi ro và quyết định mở
- Bảng risk với mức độ và cách giảm thiểu.
- Các câu hỏi cần tôi quyết định (ví dụ: A3 dùng provider search API nào; A4 chấp nhận thêm dep hay không; A9 persist snapshot ở đâu).

### Phần 5 — Verification checklist
- Lệnh kiểm tra, tiêu chí pass, và các grep "không được có" (hardcoded SGR, `renderer.reset()` thừa, commit ngoài ý muốn).

---

## 6. Cách dùng bộ dump

| File | Nội dung |
|---|---|
| `00_PROMPT.md` | File này |
| `01_AGENT_RUNTIME.md` | `agent_loop.rs`, `mehh-agent/types.rs`, `agent_session.rs`, `tool_inspection.rs` |
| `02_INTERACTIVE_TUI.md` | `interactive.rs` (toàn bộ 6.164 dòng) |
| `03_TUI_COMPONENTS.md` | `keys.rs`, `basic.rs`, `settings_list.rs`, `editor.rs` |
| `04_SESSION_SETTINGS.md` | `session.rs`, `settings.rs`, `trust.rs`, `permissions.rs` |
| `05_TOOLS_EXECUTION.md` | `tools/lib.rs`, `edit.rs`, `write.rs`, `undo_edit.rs`, `read.rs`, `bash.rs`, `core/shell.rs`, `analyze.rs` |
| `06_CONTEXT_EXTENSIBILITY.md` | `todo.rs`, `system_prompt.rs`, `skills.rs`, `prompt_templates.rs`, `compaction.rs` |
| `07_PROVIDER_CLI.md` | `anthropic.rs`, `mehh-cli/main.rs` |
| `08_POLICY_SANDBOX.md` | `mehh-execpolicy/lib.rs`, `mehh-sandbox/lib.rs` |
| `09_DESIGN_SPECS.md` | 7 file spec thiết kế (nguyên si) |
| `10_DESIGN_PLANS.md` | 11 file plan (nguyên si) |
| `11_WORKSPACE_MANIFESTS.md` | `Cargo.toml` gốc + `crates/*/Cargo.toml` + `AGENTS.md` |
| `raw/` | Copy byte-for-byte của mọi file gốc để đối chiếu |

Mỗi file dump có bảng `path | KB | dòng | SHA-256` ở đầu. SHA-256 tính trên byte gốc; nếu bạn paste code vào nơi khác và muốn kiểm tra, hash lại và so với bảng. Code nằm trong fence 5 dấu backtick để không xung đột với markdown trong source.

**Nếu context bị đầy:** đọc `00` → `11` → `09` → `01` → `04` → `05` → `02` (riêng file lớn nhất) → `03` → `06` → `08` → `07` → `10`. Sau mỗi group, viết lại architecture map 10 dòng trước khi tiếp tục.

---

## 7. Phụ lục — AGENTS.md của repo (nguyên si)

Xem file `raw/AGENTS.md` và mục cuối của `11_WORKSPACE_MANIFESTS.md`. Áp dụng nguyên vẹn khi thiết kế, đặc biệt: không `any`, fail-closed, không xoá code có chủ đích khi chưa hỏi, không commit trừ khi được yêu cầu, UTF-8 không BOM.
