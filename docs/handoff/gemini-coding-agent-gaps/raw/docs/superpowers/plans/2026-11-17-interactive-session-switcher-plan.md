# OpenCode-Style Interactive Session Switcher & Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement an OpenCode-style interactive TUI session switcher and manager in Mehhcode with `Ctrl+S`, `/sessions`, `/session`, quick `[+ Start New Session]`, `Tab` workspace scope toggle, and `d` in-modal deletion without any new external crates.

**Architecture:** Extend `mehh-tui::keys` with `TuiAction::SessionPicker` (`ctrl+s`); add `list_all_sessions` and safe `delete_session_file` in `mehh-coding-agent::session`; enhance `SelectorKind::Resume` in `mehh-coding-agent::interactive` with item 0 `__new__`, `Tab` scope toggle between current directory and all directories, and in-modal session deletion with `d`/`Delete`.

**Tech Stack:** Rust 2021, `crossterm`, `serde`, `serde_json`, `sha2`, `uuid` v7.

**Spec:** [docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md](file:///c:/Users/Admin/.gemini/antigravity/scratch/Mehhcode/docs/superpowers/specs/2026-11-17-interactive-session-switcher-design.md)

## Global Constraints

- Zero new external Cargo crates.
- No emojis anywhere in code, commits, or UI.
- All files UTF-8 without BOM.
- Preserve backward compatibility for `/resume` and `/new`.
- Security checks fail closed: `delete_session_file` only deletes `.jsonl` files strictly inside `~/.mehh/sessions/`.
- Active session cannot be deleted from within the picker modal without warning.

## Review Focus

1. Deleting active running session: attempting to delete currently active session must be rejected with an informative notice.
2. Path traversal in session deletion: deleting paths outside `~/.mehh/sessions/` or non-`.jsonl` files must fail closed with an error.
3. Empty directory session list: opening session picker when no previous sessions exist must still show `[+ Start New Session]` rather than failing or showing an empty popup.
4. Scope toggling with zero sessions: pressing `Tab` when either current workspace or global workspaces have zero sessions must cleanly update without panic or index out-of-bounds.
5. Windows path separator normalization: cross-directory scanning must handle Windows backslashes and forward slashes uniformly.

---

### Task 1: Add `TuiAction::SessionPicker` Keybinding in `mehh-tui`

**Files:**
- Modify: `crates/mehh-tui/src/keys.rs:147-240`
- Test: `crates/mehh-tui/src/keys.rs:250-327`

**Interfaces:**
- Produces: `TuiAction::SessionPicker` variant mapped by default to `"ctrl+s"`.

- [ ] **Step 1: Write the failing test**

Add unit test in `crates/mehh-tui/src/keys.rs` `tests` module:
```rust
#[test]
fn test_session_picker_keybinding_default() {
    let kb = Keybindings::default_bindings();
    assert!(kb.matches("ctrl+s", TuiAction::SessionPicker));
    assert!(kb.matches("ctrl+S", TuiAction::SessionPicker));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-tui test_session_picker_keybinding_default`  
Expected: FAIL with `variant or associated item not found in TuiAction`

- [ ] **Step 3: Implement `TuiAction::SessionPicker`**

In `crates/mehh-tui/src/keys.rs`:
1. Add `SessionPicker` variant to `pub enum TuiAction`.
2. In `Keybindings::default_bindings()`, add:
```rust
map.insert(TuiAction::SessionPicker, vec!["ctrl+s".into()]);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-tui test_session_picker_keybinding_default`  
Expected: PASS

---

### Task 2: Implement Global Session Scanning and Safe Deletion in `mehh-coding-agent`

**Files:**
- Modify: `crates/mehh-coding-agent/src/session.rs:370-500`
- Test: `crates/mehh-coding-agent/src/session.rs:1440-1914`

**Interfaces:**
- Consumes: `SessionSummary`, `session_dir_for_cwd`, `FastSessionLine`.
- Produces:
  - `pub fn parse_session_summary(path: &Path) -> Option<SessionSummary>`
  - `pub fn list_all_sessions() -> Result<Vec<SessionSummary>>`
  - `pub fn delete_session_file(path: &Path) -> Result<()>`

- [ ] **Step 1: Write the failing tests**

In `crates/mehh-coding-agent/src/session.rs` `tests` module:
```rust
#[test]
fn test_list_all_sessions_and_delete() {
    let home = std::env::temp_dir().join(format!("mehh_global_sess_test_{}", Uuid::now_v7()));
    let sess_root = home.join(".mehh").join("sessions");
    let dir_a = sess_root.join("hash_a");
    let dir_b = sess_root.join("hash_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let file_a = dir_a.join("2026-11-17T01-00-00.000Z_sess1.jsonl");
    let file_b = dir_b.join("2026-11-17T02-00-00.000Z_sess2.jsonl");

    let header_a = SessionHeader {
        kind: "session".into(),
        version: 3,
        id: "sess1".into(),
        timestamp: "2026-11-17T01:00:00.000Z".into(),
        cwd: "C:/project_a".into(),
        parent_session: None,
    };
    let header_b = SessionHeader {
        kind: "session".into(),
        version: 3,
        id: "sess2".into(),
        timestamp: "2026-11-17T02:00:00.000Z".into(),
        cwd: "C:/project_b".into(),
        parent_session: None,
    };
    std::fs::write(&file_a, format!("{}\n", serde_json::to_string(&header_a).unwrap())).unwrap();
    std::fs::write(&file_b, format!("{}\n", serde_json::to_string(&header_b).unwrap())).unwrap();

    // Test parse_session_summary
    let summary_a = parse_session_summary(&file_a).expect("parse file_a");
    assert_eq!(summary_a.id, "sess1");

    // Test delete_session_file
    delete_session_file(&file_a).expect("delete file_a");
    assert!(!file_a.exists());

    // Test fail closed on path outside .mehh/sessions
    let outside_file = std::env::temp_dir().join("outside.jsonl");
    std::fs::write(&outside_file, "{}").unwrap();
    assert!(delete_session_file(&outside_file).is_err());
    let _ = std::fs::remove_file(outside_file);
    let _ = std::fs::remove_dir_all(home);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_list_all_sessions_and_delete`  
Expected: FAIL with `cannot find function parse_session_summary in this scope`

- [ ] **Step 3: Implement `parse_session_summary`, `list_all_sessions`, and `delete_session_file`**

In `crates/mehh-coding-agent/src/session.rs`:
1. Extract file parsing logic from `list_sessions` into `pub fn parse_session_summary(path: &Path) -> Option<SessionSummary>`.
2. Implement `pub fn list_all_sessions() -> Result<Vec<SessionSummary>>`:
   - Resolves `~/.mehh/sessions/`.
   - Iterates all subdirectories, reads `.jsonl` files with `parse_session_summary`.
   - Sorts results descending by `timestamp`.
3. Implement `pub fn delete_session_file(path: &Path) -> Result<()>`:
   - Validates that canonical path or normalized path contains `.mehh` and `sessions` component and ends with `.jsonl`.
   - Calls `std::fs::remove_file(path)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_list_all_sessions_and_delete`  
Expected: PASS

---

### Task 3: Support `/sessions` Command and Enhance `/session` Routing

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:225-240, 1315-1345, 1470-1490`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4390-4450`

**Interfaces:**
- Consumes: `open_session_picker`, `SessionPickerScope`.
- Produces:
  - Command `/sessions`: opens session picker modal.
  - Command `/session`: opens session picker modal if no args; `/session info` displays token/entry stats; `/session new` creates new session.
  - Slash command completions list updated with `/sessions` and `/session`.

- [ ] **Step 1: Write the failing test**

In `crates/mehh-coding-agent/src/interactive.rs` `tests` module:
```rust
#[tokio::test]
async fn test_slash_command_sessions_and_session_routing() {
    let mut session = create_test_session();
    let mut state = InteractiveState::new();

    // /sessions opens the selector
    let outcome = handle_slash_command("/sessions", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_some());
    assert_eq!(state.selector.as_ref().unwrap().kind, SelectorKind::Resume);

    // /session without args opens selector
    state.selector = None;
    let outcome = handle_slash_command("/session", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_some());

    // /session info displays stats
    state.selector = None;
    let outcome = handle_slash_command("/session info", &mut session, &mut state, None, false).await;
    assert_eq!(outcome, SlashCommandOutcome::Handled);
    assert!(state.selector.is_none());
    let last_item = state.items.last().unwrap();
    if let TranscriptItem::System(text) = last_item {
        assert!(text.contains("Session:"));
        assert!(text.contains("Entries:"));
    } else {
        panic!("expected system transcript item");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_slash_command_sessions_and_session_routing`  
Expected: FAIL because `/sessions` is unhandled and `/session` prints stats directly.

- [ ] **Step 3: Update slash command handling**

In `crates/mehh-coding-agent/src/interactive.rs`:
1. In `slash_command_items()`:
   - Add `/sessions` with description `"Switch or start sessions (Ctrl+S)"`.
   - Update `/session` description to `"Manage sessions (/session info, /session new)"`.
2. In `handle_slash_command`:
   - Match `"/sessions" | "/resume"` -> call `open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace)`.
   - Match `"/session"`:
     - If `arg == Some("info")` -> print session stats.
     - If `arg == Some("new")` -> execute new session logic (alias for `/new`).
     - Otherwise -> call `open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_slash_command_sessions_and_session_routing`  
Expected: PASS

---

### Task 4: Enhance Interactive Selector with Quick New Session, `Tab` Scope, and `d` Delete

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:50-70, 1575-1620, 1820-1845, 2745-2880`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4390-4457`

**Interfaces:**
- Produces:
  - `pub enum SessionPickerScope { CurrentWorkspace, AllWorkspaces }`
  - Field `pub session_scope: Option<SessionPickerScope>` on `Selector`
  - In `resume_selector_items`: item 0 is always `__new__` (`[+ Start New Session]`).
  - Key `Tab` when `kind == SelectorKind::Resume` toggles scope and updates items.
  - Key `d` / `delete` when `kind == SelectorKind::Resume` deletes session file.
  - Key `Enter` on `__new__` calls `new_session()` and initializes transcript.

- [ ] **Step 1: Write the failing test**

In `crates/mehh-coding-agent/src/interactive.rs` `tests` module:
```rust
#[tokio::test]
async fn test_session_picker_items_have_new_session_at_zero() {
    let sessions = vec![SessionSummary {
        path: PathBuf::from("test.jsonl"),
        id: "test1".into(),
        timestamp: "2026-11-17T01:00:00.000Z".into(),
        cwd: "C:/test".into(),
        entry_count: 5,
        name: Some("test session".into()),
        first_prompt: Some("hello".into()),
    }];
    let items = resume_selector_items(&sessions, SessionPickerScope::CurrentWorkspace);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "__new__");
    assert!(items[0].label.contains("Start New Session"));
    assert_eq!(items[1].id, "test.jsonl");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_session_picker_items_have_new_session_at_zero`  
Expected: FAIL with arguments mismatch in `resume_selector_items`.

- [ ] **Step 3: Implement Selector Extensions and Key Actions**

In `crates/mehh-coding-agent/src/interactive.rs`:
1. Add `SessionPickerScope` enum with `CurrentWorkspace` and `AllWorkspaces`.
2. Add `pub session_scope: Option<SessionPickerScope>` to `Selector`.
3. Update `resume_selector_items(sessions: &[SessionSummary], scope: SessionPickerScope) -> Vec<SelectItem>`:
   - Insert `SelectItem::new("__new__", "[+ Start New Session]").with_description("Start a fresh conversation in current directory")` as first item.
   - For all subsequent sessions, format label and description. When `scope == SessionPickerScope::AllWorkspaces`, append directory name to description.
4. Implement `open_session_picker`:
   - Loads sessions according to `scope`.
   - Creates `Selector` with `kind: SelectorKind::Resume`, items from `resume_selector_items`, and `session_scope: Some(scope)`.
5. Update `SelectorKind::Resume` title in `render_now`:
   ```rust
   SelectorKind::Resume => match selector.session_scope {
       Some(SessionPickerScope::AllWorkspaces) => "Sessions: All Workspaces (Enter: select, Tab: current dir, d: delete, Esc: cancel)",
       _ => "Sessions: Current Workspace (Enter: select, Tab: all dirs, d: delete, Esc: cancel)",
   }
   ```
6. In event loop key handler:
   - On `Enter`:
     - If `item.id == "__new__"`: reset queues, call `session.session_mut().new_session()`, reset transcript.
     - Else: call `SessionManager::open_file` and `rebuild_transcript_items`.
   - On `Tab` with `SelectorKind::Resume`:
     - Toggle scope (`CurrentWorkspace` <-> `AllWorkspaces`).
     - Reload sessions, re-populate `sel.list`, update `sel.session_scope`.
   - On `d` | `delete` with `SelectorKind::Resume`:
     - If `item.id == "__new__"`: no-op.
     - If `item.id == active_session_path`: push system message "Cannot delete currently active session."
     - Otherwise: call `delete_session_file`, remove item from `sel.list`, push system message "Deleted session."

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_session_picker_items_have_new_session_at_zero`  
Expected: PASS

---

### Task 5: Wire `Ctrl+S` Hotkey in Interactive Loop

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:2935-2945`
- Test: `crates/mehh-coding-agent/src/interactive.rs:4440-4457`

**Interfaces:**
- Consumes: `TuiAction::SessionPicker`, `open_session_picker`.

- [ ] **Step 1: Write test for hotkey triggering session picker**

In `crates/mehh-coding-agent/src/interactive.rs`:
```rust
#[test]
fn test_hotkeys_help_includes_session_picker() {
    let kb = Keybindings::default_bindings();
    let help = format_hotkeys(&kb);
    assert!(help.contains("Ctrl+S") || help.contains("ctrl+s"));
    assert!(help.contains("session") || help.contains("Session"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mehh-coding-agent test_hotkeys_help_includes_session_picker`  
Expected: FAIL because `format_hotkeys` doesn't list `SessionPicker`.

- [ ] **Step 3: Wire `SessionPicker` in `format_hotkeys` and event loop**

1. In `format_hotkeys`: add `Ctrl+S: Switch or create sessions`.
2. In `interactive.rs` loop before `handle_editor_key`:
```rust
if keybindings.matches(&key_str, TuiAction::SessionPicker) {
    open_session_picker(session, &mut state, SessionPickerScope::CurrentWorkspace);
    render_now(&state, &editor, session, &mut terminal, &mut renderer);
    continue 'outer;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p mehh-coding-agent test_hotkeys_help_includes_session_picker`  
Expected: PASS

---

### Task 6: Workspace-wide Compilation, Clippy, and Test Verification

**Files:**
- None (verification step across all 8 crates)

- [ ] **Step 1: Run `cargo check --workspace`**

Run: `cargo check --workspace`  
Expected: Clean exit 0 with 0 errors.

- [ ] **Step 2: Run `cargo clippy --workspace --all-targets -- -D warnings`**

Run: `cargo clippy --workspace --all-targets -- -D warnings`  
Expected: Clean exit 0 with 0 warnings.

- [ ] **Step 3: Run workspace tests**

Run: `cargo test --workspace`  
Expected: 100% test pass across all 8 crates.
