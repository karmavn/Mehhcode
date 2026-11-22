# OpenCode-Style Interactive Session Switcher & Management — Design Specification

**Date:** 2026-11-17  
**Target:** `crates/*` (`mehh-tui`, `mehh-coding-agent`, `mehh-cli`)  
**Approach:** Approach B — Interactive TUI-First Session Switcher & Management  

---

## 1. Intent and Why

Mehhcode already possesses a robust append-only JSONL v3 branching tree session format (`crates/mehh-coding-agent/src/session.rs`), zero-allocation session listing (`FastSessionLine`), and an in-memory transcript rebuilder (`rebuild_transcript_items`).

However, user experience falls short of modern AI coding agents like OpenCode:
1. **Hidden / Fragmented Commands:** Users intuitively type `/session` or `/sessions` to switch sessions, but Mehhcode's `/session` only dumps plain text stats, `/sessions` fails as an unknown command, and switching is hidden under `/resume`.
2. **Missing Hotkey:** No keyboard shortcut (e.g. `Ctrl+S`) exists to invoke the session list.
3. **Directory Isolation:** `list_sessions(cwd)` only searches `~/.mehh/sessions/<cwd_hash>/`. Launching `mehh` from a subfolder (e.g. `target/release` or a sub-crate) isolates the user from previous sessions in the project root.
4. **No In-Modal Actions:** The current `/resume` picker cannot create a new session (`/new`), delete stale sessions, or toggle between current directory and global workspace sessions.

Implementing Approach B transforms session management into an intuitive, keyboard-driven experience matching OpenCode while maintaining Mehhcode's zero-dependency, single-binary architecture.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
- **Unified Slash Commands:**
  - `/sessions` and `/session` (without arguments) open the interactive Session Switcher modal.
  - `/session info` displays session metadata and token stats.
  - `/session new` acts as an alias for `/new`.
  - `/resume` remains supported as a backward-compatible alias.
- **Dedicated Hotkey (`Ctrl+S`):**
  - Register `TuiAction::SessionPicker` (default `ctrl+s`) in `Keybindings` (`crates/mehh-tui/src/keys.rs`).
  - Pressing `Ctrl+S` when no selector is open immediately launches the Session Switcher.
- **Quick New Session Entry:**
  - The first item in the Session Switcher list is always `[+ Start New Session]`. Selecting it invokes `session.session_mut().new_session()` without exiting.
- **Workspace vs Global Scope Toggle (`Tab`):**
  - Implement `list_all_sessions()` in `session.rs` to scan all session subdirectories under `~/.mehh/sessions/`.
  - In the session modal, pressing `Tab` toggles between *Current Workspace* (`~/.mehh/sessions/<cwd_hash>/`) and *All Workspaces* (`~/.mehh/sessions/*`).
- **Session Deletion (`d` / `Delete`):**
  - Pressing `d` or `Delete` on an existing session item deletes the `.jsonl` file from disk and removes it from the selector list.

### Non-Goals
- Adding SQLite databases or migrating away from JSONL v3 files (file-based append-only persistence remains).
- Full cross-session semantic vector search (reserved for `ChatRecall`).
- Electron or browser-based GUI session managers.

### Anti-Goals
- Deleting the currently active running session from within the modal without confirmation or fallback.
- Blocking the UI thread during file deletion or cross-project scanning.
- Adding any new external crates to `Cargo.toml`.

---

## 3. Approaches Considered

1. **Approach A (Goose-style CLI Subcommands):** Focuses on external CLI flags (`mehh session list`, `mehh session resume`). Rejected by the user because daily coding happens inside the interactive TUI.
2. **Approach B (OpenCode-style Interactive TUI Focus - Chosen):** In-terminal modal popup with `Ctrl+S`, `/sessions`, scope toggling, quick new session, and deletion. Delivers immediate UX parity with OpenCode and Claude Code.
3. **Approach C (Hybrid CLI + TUI):** Combines both. Approach B covers the high-priority interactive workflows immediately without expanding CLI surface area unnecessarily.

---

## 4. Detailed Design & Architecture

### 4.1 Keybindings (`crates/mehh-tui/src/keys.rs`)

Add `TuiAction::SessionPicker`:
```rust
pub enum TuiAction {
    // ... existing variants
    SessionPicker,
}
```
In `Keybindings::default_bindings()`:
```rust
map.insert(TuiAction::SessionPicker, vec!["ctrl+s".into()]);
```
Format hotkeys in `/hotkeys` to display:
```text
Ctrl+S: Switch or create sessions
```

### 4.2 Cross-Directory Session Discovery (`crates/mehh-coding-agent/src/session.rs`)

Add `list_all_sessions() -> Result<Vec<SessionSummary>>`:
1. Locates `~/.mehh/sessions/`.
2. Iterates over all subdirectories (`<cwd_hash>`).
3. Reads all `.jsonl` session files using `BufReader` and `FastSessionLine`.
4. Extracts `SessionSummary { id, path, timestamp, cwd, entry_count, name, first_prompt }`.
5. Sorts descending by `timestamp` (newest first).

Add `delete_session_file(path: &Path) -> Result<()>`:
- Verifies path is within `~/.mehh/sessions/` and ends with `.jsonl` before calling `std::fs::remove_file`.

### 4.3 Interactive Session Switcher Overlay (`crates/mehh-coding-agent/src/interactive.rs`)

#### Selector State Extension:
Add scope tracking in `Selector`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerScope {
    CurrentWorkspace,
    AllWorkspaces,
}
```
`SelectorKind::Resume` handles:
- **Item 0:** `SelectItem::new("__new__", "[+ Start New Session]").with_description("Start a fresh conversation in current directory")`
- **Subsequent Items:** Sessions formatted with relative time/timestamp, title/first prompt snippet, entry count, and (in All Workspaces mode) project directory name.

#### Key Event Dispatches:
When `state.selector` has `kind == SelectorKind::Resume`:
- **`Enter`:**
  - If `item.id == "__new__"`: calls `session.clear_queues()`, `session.session_mut().new_session()`, resets transcript to welcome message.
  - Otherwise: loads `SessionManager::load_file(&item.id)` and calls `rebuild_transcript_items`.
- **`Tab`:**
  - Toggles `scope` between `CurrentWorkspace` and `AllWorkspaces`.
  - Re-populates `sel.list` items without closing the popup.
- **`d` or `delete`:**
  - If `item.id == "__new__"`: no-op.
  - If `item.id == current_session_path`: prints notice "Cannot delete currently active session."
  - Otherwise: calls `delete_session_file(&item.id)`, removes from `sel.list`, prints confirmation.
- **`Escape`:** Closes selector.

#### Outside Selector:
- Pressing `keybindings.matches(&key_str, TuiAction::SessionPicker)` opens the Session Switcher modal.

---

## 5. Verification & Testing

1. **Unit tests in `crates/mehh-coding-agent/src/session.rs`:**
   - Test `list_all_sessions` aggregates sessions across multiple `cwd_hash` directories.
   - Test `delete_session_file` removes target `.jsonl` and fails closed on external paths.
2. **TUI tests in `crates/mehh-coding-agent/src/interactive.rs`:**
   - Test `/sessions` and `/session` open `SelectorKind::Resume`.
   - Test `/session info` prints session details.
   - Test selecting `__new__` initializes a fresh session.
   - Test `Tab` toggles scope and updates item list.
   - Test `d` removes session entry from selector list.
3. **Workspace Checks:**
   - `cargo check --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
